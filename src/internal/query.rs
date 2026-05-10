use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_stream::try_stream;
use futures::stream::{BoxStream, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{Mutex, Notify, mpsc, oneshot};
use tokio::time::timeout;
use uuid::Uuid;

use crate::errors::{ClaudeSdkError, Result};
use crate::internal::task_compat::{TaskHandle, spawn_detached};
use crate::mcp::SdkMcpServer;
use crate::transport::Transport;
use crate::types::{
    CanUseTool, HookContext, HookInput, HookMatcher, InputMessage, PermissionMode,
    PermissionResult, SessionKey, Skills, ToolPermissionContext,
};

use super::transcript_mirror_batcher::TranscriptMirrorBatcher;

pub struct Query {
    transport: Arc<dyn Transport>,
    is_streaming_mode: bool,
    can_use_tool: Option<CanUseTool>,
    hooks: HashMap<String, Vec<HookMatcher>>,
    hook_callbacks: Mutex<HashMap<String, crate::types::HookCallback>>,
    next_callback_id: AtomicU64,
    request_counter: AtomicU64,
    pending_control: Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>,
    message_tx: mpsc::Sender<Result<Value>>,
    message_rx: Arc<Mutex<Option<mpsc::Receiver<Result<Value>>>>>,
    read_task: Mutex<Option<TaskHandle>>,
    child_tasks: Mutex<Vec<TaskHandle>>,
    inflight_requests: Mutex<HashMap<String, TaskHandle>>,
    initialized: AtomicBool,
    closed: AtomicBool,
    first_result: Notify,
    initialization_result: Mutex<Option<Value>>,
    transcript_mirror_batcher: Mutex<Option<Arc<TranscriptMirrorBatcher>>>,
    sdk_mcp_servers: HashMap<String, Arc<SdkMcpServer>>,
    agents: Option<Value>,
    exclude_dynamic_sections: Option<bool>,
    skills: Option<Skills>,
    initialize_timeout: Duration,
}

impl Query {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        transport: Arc<dyn Transport>,
        is_streaming_mode: bool,
        can_use_tool: Option<CanUseTool>,
        hooks: Option<HashMap<String, Vec<HookMatcher>>>,
        sdk_mcp_servers: HashMap<String, Arc<SdkMcpServer>>,
        initialize_timeout: Duration,
        agents: Option<Value>,
        exclude_dynamic_sections: Option<bool>,
        skills: Option<Skills>,
    ) -> Self {
        let (message_tx, message_rx) = mpsc::channel(100);
        Self {
            transport,
            is_streaming_mode,
            can_use_tool,
            hooks: hooks.unwrap_or_default(),
            hook_callbacks: Mutex::new(HashMap::new()),
            next_callback_id: AtomicU64::new(0),
            request_counter: AtomicU64::new(0),
            pending_control: Mutex::new(HashMap::new()),
            message_tx,
            message_rx: Arc::new(Mutex::new(Some(message_rx))),
            read_task: Mutex::new(None),
            child_tasks: Mutex::new(Vec::new()),
            inflight_requests: Mutex::new(HashMap::new()),
            initialized: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            first_result: Notify::new(),
            initialization_result: Mutex::new(None),
            transcript_mirror_batcher: Mutex::new(None),
            sdk_mcp_servers,
            agents,
            exclude_dynamic_sections,
            skills,
            initialize_timeout,
        }
    }

    pub async fn set_transcript_mirror_batcher(&self, batcher: TranscriptMirrorBatcher) {
        *self.transcript_mirror_batcher.lock().await = Some(Arc::new(batcher));
    }

    pub async fn report_mirror_error(&self, key: Option<SessionKey>, error: String) {
        let msg = json!({
            "type": "system",
            "subtype": "mirror_error",
            "error": error,
            "key": key,
            "uuid": Uuid::new_v4().to_string(),
            "session_id": key.as_ref().map(|k| k.session_id.clone()).unwrap_or_default(),
        });
        let _ = self.message_tx.try_send(Ok(msg));
    }

    pub async fn initialize(&self) -> Result<Option<Value>> {
        if !self.is_streaming_mode {
            return Ok(None);
        }

        let mut hooks_config = serde_json::Map::new();
        if !self.hooks.is_empty() {
            let mut callback_map = self.hook_callbacks.lock().await;
            for (event, matchers) in &self.hooks {
                if matchers.is_empty() {
                    continue;
                }
                let mut event_matchers = Vec::new();
                for matcher in matchers {
                    let mut callback_ids = Vec::new();
                    for callback in &matcher.hooks {
                        let id = self.next_callback_id.fetch_add(1, Ordering::SeqCst);
                        let callback_id = format!("hook_{id}");
                        callback_map.insert(callback_id.clone(), callback.clone());
                        callback_ids.push(Value::String(callback_id));
                    }
                    let mut matcher_config = serde_json::Map::new();
                    matcher_config.insert(
                        "matcher".to_string(),
                        matcher
                            .matcher
                            .clone()
                            .map(Value::String)
                            .unwrap_or(Value::Null),
                    );
                    matcher_config
                        .insert("hookCallbackIds".to_string(), Value::Array(callback_ids));
                    if let Some(timeout) = matcher.timeout {
                        matcher_config.insert("timeout".to_string(), json!(timeout));
                    }
                    event_matchers.push(Value::Object(matcher_config));
                }
                hooks_config.insert(event.clone(), Value::Array(event_matchers));
            }
        }

        let mut request = serde_json::Map::new();
        request.insert(
            "subtype".to_string(),
            Value::String("initialize".to_string()),
        );
        request.insert(
            "hooks".to_string(),
            if hooks_config.is_empty() {
                Value::Null
            } else {
                Value::Object(hooks_config)
            },
        );
        if let Some(agents) = &self.agents {
            request.insert("agents".to_string(), agents.clone());
        }
        if let Some(exclude) = self.exclude_dynamic_sections {
            request.insert("excludeDynamicSections".to_string(), Value::Bool(exclude));
        }
        if let Some(Skills::List(skills)) = &self.skills {
            request.insert(
                "skills".to_string(),
                Value::Array(skills.iter().cloned().map(Value::String).collect()),
            );
        }

        let response = self
            .send_control_request(Value::Object(request), self.initialize_timeout)
            .await?;
        self.initialized.store(true, Ordering::SeqCst);
        *self.initialization_result.lock().await = Some(response.clone());
        Ok(Some(response))
    }

    pub async fn initialization_result(&self) -> Option<Value> {
        self.initialization_result.lock().await.clone()
    }

    pub async fn start(self: &Arc<Self>) {
        let mut guard = self.read_task.lock().await;
        if guard.is_none() {
            let this = self.clone();
            *guard = Some(spawn_detached(async move {
                this.read_messages_loop().await;
            }));
        }
    }

    pub async fn spawn_task<F>(self: &Arc<Self>, future: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let handle = spawn_detached(future);
        self.child_tasks.lock().await.push(handle);
    }

    async fn read_messages_loop(self: Arc<Self>) {
        let mut stream = self.transport.read_messages();
        while let Some(item) = stream.next().await {
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            match item {
                Ok(message) => {
                    if let Err(err) = self.route_message(message).await {
                        let _ = self.message_tx.send(Err(err)).await;
                    }
                }
                Err(err) => {
                    for (_, sender) in self.pending_control.lock().await.drain() {
                        let _ = sender.send(Err(ClaudeSdkError::Runtime(err.to_string())));
                    }
                    let _ = self.message_tx.send(Err(err)).await;
                    break;
                }
            }
        }

        if let Some(batcher) = self.transcript_mirror_batcher.lock().await.clone() {
            batcher.flush().await;
        }
        self.first_result.notify_waiters();
        let _ = self.message_tx.send(Ok(json!({ "type": "end" }))).await;
    }

    async fn route_message(self: &Arc<Self>, message: Value) -> Result<()> {
        let msg_type = message.get("type").and_then(Value::as_str);
        match msg_type {
            Some("control_response") => {
                let request_id = message
                    .get("response")
                    .and_then(|r| r.get("request_id"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                if let Some(request_id) = request_id {
                    if let Some(sender) = self.pending_control.lock().await.remove(&request_id) {
                        let response = message.get("response").cloned().unwrap_or(Value::Null);
                        let result =
                            if response.get("subtype").and_then(Value::as_str) == Some("error") {
                                Err(ClaudeSdkError::Runtime(
                                    response
                                        .get("error")
                                        .and_then(Value::as_str)
                                        .unwrap_or("Unknown error")
                                        .to_string(),
                                ))
                            } else {
                                Ok(response)
                            };
                        let _ = sender.send(result);
                    }
                }
            }
            Some("control_request") => {
                let req_id = message
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let this = self.clone();
                let req_id_for_task = req_id.clone();
                let (start_tx, start_rx) = oneshot::channel();
                let handle = spawn_detached(async move {
                    let _ = start_rx.await;
                    this.clone().handle_control_request(message).await;
                    this.inflight_requests.lock().await.remove(&req_id_for_task);
                });
                self.inflight_requests.lock().await.insert(req_id, handle);
                let _ = start_tx.send(());
            }
            Some("control_cancel_request") => {
                if let Some(cancel_id) = message.get("request_id").and_then(Value::as_str) {
                    if let Some(handle) = self.inflight_requests.lock().await.remove(cancel_id) {
                        handle.cancel();
                    }
                }
            }
            Some("transcript_mirror") => {
                if let Some(batcher) = self.transcript_mirror_batcher.lock().await.clone() {
                    let file_path = message
                        .get("filePath")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let entries = message
                        .get("entries")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    batcher.enqueue(file_path, entries).await;
                }
            }
            Some("result") => {
                if let Some(batcher) = self.transcript_mirror_batcher.lock().await.clone() {
                    batcher.flush().await;
                }
                self.first_result.notify_waiters();
                self.message_tx
                    .send(Ok(message))
                    .await
                    .map_err(|e| ClaudeSdkError::Runtime(e.to_string()))?;
            }
            _ => {
                self.message_tx
                    .send(Ok(message))
                    .await
                    .map_err(|e| ClaudeSdkError::Runtime(e.to_string()))?;
            }
        }
        Ok(())
    }

    async fn handle_control_request(self: Arc<Self>, request: Value) {
        let request_id = request
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let response = self.handle_control_request_inner(&request).await;
        let wire = match response {
            Ok(response_data) => json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": request_id,
                    "response": response_data,
                }
            }),
            Err(err) => json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": request_id,
                    "error": err.to_string(),
                }
            }),
        };
        let _ = self.transport.write(&(wire.to_string() + "\n")).await;
    }

    async fn handle_control_request_inner(&self, request: &Value) -> Result<Value> {
        let request_data = request.get("request").ok_or_else(|| {
            ClaudeSdkError::Runtime("Missing control request payload".to_string())
        })?;
        let subtype = request_data
            .get("subtype")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ClaudeSdkError::Runtime("Missing control request subtype".to_string())
            })?;

        match subtype {
            "can_use_tool" => {
                let callback = self.can_use_tool.clone().ok_or_else(|| {
                    ClaudeSdkError::Runtime("canUseTool callback is not provided".to_string())
                })?;
                let original_input = request_data.get("input").cloned().unwrap_or(Value::Null);
                let suggestions = request_data
                    .get("permission_suggestions")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let context = ToolPermissionContext {
                    signal: None,
                    suggestions,
                    tool_use_id: opt_str(request_data, "tool_use_id"),
                    agent_id: opt_str(request_data, "agent_id"),
                    blocked_path: opt_str(request_data, "blocked_path"),
                    decision_reason: opt_str(request_data, "decision_reason"),
                    title: opt_str(request_data, "title"),
                    display_name: opt_str(request_data, "display_name"),
                    description: opt_str(request_data, "description"),
                };
                let tool_name = request_data
                    .get("tool_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let response = callback(
                    tool_name,
                    crate::types::ToolInput::from_value(original_input.clone()),
                    context,
                )
                .await?;
                match response {
                    PermissionResult::Allow {
                        updated_input,
                        updated_permissions,
                    } => {
                        let mut obj = serde_json::Map::new();
                        obj.insert("behavior".to_string(), Value::String("allow".to_string()));
                        obj.insert(
                            "updatedInput".to_string(),
                            updated_input
                                .map(crate::types::ToolInput::into_value)
                                .unwrap_or(original_input),
                        );
                        if let Some(updated_permissions) = updated_permissions {
                            obj.insert(
                                "updatedPermissions".to_string(),
                                Value::Array(
                                    updated_permissions.iter().map(|p| p.to_value()).collect(),
                                ),
                            );
                        }
                        Ok(Value::Object(obj))
                    }
                    PermissionResult::Deny { message, interrupt } => {
                        let mut obj = serde_json::Map::new();
                        obj.insert("behavior".to_string(), Value::String("deny".to_string()));
                        obj.insert("message".to_string(), Value::String(message));
                        if interrupt {
                            obj.insert("interrupt".to_string(), Value::Bool(true));
                        }
                        Ok(Value::Object(obj))
                    }
                }
            }
            "hook_callback" => {
                let callback_id = request_data
                    .get("callback_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ClaudeSdkError::Runtime("Missing hook callback_id".to_string())
                    })?;
                let callback = self
                    .hook_callbacks
                    .lock()
                    .await
                    .get(callback_id)
                    .cloned()
                    .ok_or_else(|| {
                        ClaudeSdkError::Runtime(format!(
                            "No hook callback found for ID: {callback_id}"
                        ))
                    })?;
                let input = serde_json::from_value::<HookInput>(
                    request_data.get("input").cloned().unwrap_or(Value::Null),
                )?;
                let output = callback(
                    input,
                    opt_str(request_data, "tool_use_id"),
                    HookContext { signal: None },
                )
                .await?;
                Ok(convert_hook_output_for_cli(serde_json::to_value(output)?))
            }
            "mcp_message" => {
                let server_name = request_data
                    .get("server_name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        ClaudeSdkError::Runtime("Missing server_name for MCP request".to_string())
                    })?;
                let message = request_data.get("message").cloned().unwrap_or(Value::Null);
                let mcp_response = match self.sdk_mcp_servers.get(server_name) {
                    Some(server) => server.handle_json_rpc(message.clone()).await,
                    None => json!({
                        "jsonrpc": "2.0",
                        "id": message.get("id").cloned().unwrap_or(Value::Null),
                        "error": {
                            "code": -32601,
                            "message": format!("Server '{server_name}' not found")
                        }
                    }),
                };
                Ok(json!({ "mcp_response": mcp_response }))
            }
            other => Err(ClaudeSdkError::Runtime(format!(
                "Unsupported control request subtype: {other}"
            ))),
        }
    }

    async fn send_control_request(&self, request: Value, wait: Duration) -> Result<Value> {
        if !self.is_streaming_mode {
            return Err(ClaudeSdkError::Runtime(
                "Control requests require streaming mode".to_string(),
            ));
        }
        let n = self.request_counter.fetch_add(1, Ordering::SeqCst) + 1;
        let request_id = format!("req_{n}_{}", Uuid::new_v4().simple());
        let (tx, rx) = oneshot::channel();
        self.pending_control
            .lock()
            .await
            .insert(request_id.clone(), tx);
        let control_request = json!({
            "type": "control_request",
            "request_id": request_id,
            "request": request,
        });
        self.transport
            .write(&(control_request.to_string() + "\n"))
            .await?;

        let result = match timeout(wait, rx).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => {
                return Err(ClaudeSdkError::Runtime(
                    "Control request response channel closed".to_string(),
                ));
            }
            Err(_) => {
                self.pending_control.lock().await.remove(&request_id);
                return Err(ClaudeSdkError::Runtime(format!(
                    "Control request timeout: {}",
                    control_request
                        .get("request")
                        .and_then(|r| r.get("subtype"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                )));
            }
        };

        let response_data = result
            .get("response")
            .cloned()
            .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
        if response_data.is_object() {
            Ok(response_data)
        } else {
            Ok(Value::Object(serde_json::Map::new()))
        }
    }

    pub async fn get_mcp_status(&self) -> Result<Value> {
        self.send_control_request(json!({"subtype": "mcp_status"}), Duration::from_secs(60))
            .await
    }

    pub async fn get_context_usage(&self) -> Result<Value> {
        self.send_control_request(
            json!({"subtype": "get_context_usage"}),
            Duration::from_secs(60),
        )
        .await
    }

    pub async fn interrupt(&self) -> Result<()> {
        self.send_control_request(json!({"subtype": "interrupt"}), Duration::from_secs(60))
            .await
            .map(|_| ())
    }

    pub async fn set_permission_mode(&self, mode: PermissionMode) -> Result<()> {
        self.send_control_request(
            json!({"subtype": "set_permission_mode", "mode": mode.to_string()}),
            Duration::from_secs(60),
        )
        .await
        .map(|_| ())
    }

    pub async fn set_model(&self, model: Option<String>) -> Result<()> {
        self.send_control_request(
            json!({"subtype": "set_model", "model": model}),
            Duration::from_secs(60),
        )
        .await
        .map(|_| ())
    }

    pub async fn rewind_files(&self, user_message_id: String) -> Result<()> {
        self.send_control_request(
            json!({"subtype": "rewind_files", "user_message_id": user_message_id}),
            Duration::from_secs(60),
        )
        .await
        .map(|_| ())
    }

    pub async fn reconnect_mcp_server(&self, server_name: String) -> Result<()> {
        self.send_control_request(
            json!({"subtype": "mcp_reconnect", "serverName": server_name}),
            Duration::from_secs(60),
        )
        .await
        .map(|_| ())
    }

    pub async fn toggle_mcp_server(&self, server_name: String, enabled: bool) -> Result<()> {
        self.send_control_request(
            json!({"subtype": "mcp_toggle", "serverName": server_name, "enabled": enabled}),
            Duration::from_secs(60),
        )
        .await
        .map(|_| ())
    }

    pub async fn stop_task(&self, task_id: String) -> Result<()> {
        self.send_control_request(
            json!({"subtype": "stop_task", "task_id": task_id}),
            Duration::from_secs(60),
        )
        .await
        .map(|_| ())
    }

    pub async fn wait_for_result_and_end_input(&self) -> Result<()> {
        if !self.sdk_mcp_servers.is_empty() || !self.hooks.is_empty() {
            self.first_result.notified().await;
        }
        self.transport.end_input().await
    }

    pub async fn stream_input(self: Arc<Self>, mut stream: BoxStream<'static, InputMessage>) {
        while let Some(message) = stream.next().await {
            if self.closed.load(Ordering::SeqCst) {
                break;
            }
            let Ok(message) = serde_json::to_string(&message) else {
                continue;
            };
            let _ = self.transport.write(&(message + "\n")).await;
        }
        let _ = self.wait_for_result_and_end_input().await;
    }

    pub fn receive_messages(&self) -> BoxStream<'static, Result<Value>> {
        let rx = self.message_rx.clone();
        try_stream! {
            let mut receiver: mpsc::Receiver<Result<Value>> = rx
                .lock()
                .await
                .take()
                .ok_or_else(|| ClaudeSdkError::Runtime("receive_messages can only be called once".to_string()))?;
            while let Some(item) = receiver.recv().await {
                let message = item?;
                if message.get("type").and_then(Value::as_str) == Some("end") {
                    break;
                }
                yield message;
            }
        }
        .boxed()
    }

    pub async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        if let Some(batcher) = self.transcript_mirror_batcher.lock().await.clone() {
            batcher.close().await;
        }
        for task in self.child_tasks.lock().await.drain(..) {
            task.cancel();
        }
        for (_, task) in self.inflight_requests.lock().await.drain() {
            task.cancel();
        }
        if let Some(task) = self.read_task.lock().await.take() {
            task.cancel();
        }
        let _ = self.message_tx.try_send(Ok(json!({ "type": "end" })));
        self.transport.close().await
    }
}

fn opt_str(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn convert_hook_output_for_cli(output: Value) -> Value {
    let Value::Object(obj) = output else {
        return output;
    };
    let mut converted = serde_json::Map::new();
    for (key, value) in obj {
        match key.as_str() {
            "async_" => {
                converted.insert("async".to_string(), value);
            }
            "continue_" => {
                converted.insert("continue".to_string(), value);
            }
            _ => {
                converted.insert(key, value);
            }
        }
    }
    Value::Object(converted)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use async_trait::async_trait;
    use futures::stream::{self, BoxStream};

    use super::*;
    use crate::transport::Transport;

    #[derive(Clone)]
    struct TestTransport {
        tx: mpsc::UnboundedSender<Result<Value>>,
        rx: Arc<std::sync::Mutex<Option<mpsc::UnboundedReceiver<Result<Value>>>>>,
        writes: Arc<Mutex<Vec<String>>>,
    }

    impl TestTransport {
        fn new() -> Self {
            let (tx, rx) = mpsc::unbounded_channel();
            Self {
                tx,
                rx: Arc::new(std::sync::Mutex::new(Some(rx))),
                writes: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn push(&self, value: Value) {
            self.tx.send(Ok(value)).unwrap();
        }

        async fn writes(&self) -> Vec<String> {
            self.writes.lock().await.clone()
        }
    }

    #[async_trait]
    impl Transport for TestTransport {
        async fn connect(&self) -> Result<()> {
            Ok(())
        }

        async fn write(&self, data: &str) -> Result<()> {
            self.writes.lock().await.push(data.to_string());
            Ok(())
        }

        fn read_messages(&self) -> BoxStream<'static, Result<Value>> {
            let rx = self.rx.lock().unwrap().take();
            match rx {
                Some(mut rx) => Box::pin(async_stream::try_stream! {
                    while let Some(item) = rx.recv().await {
                        yield item?;
                    }
                }),
                None => Box::pin(stream::once(async {
                    Err(ClaudeSdkError::Runtime(
                        "read_messages called twice".to_string(),
                    ))
                })),
            }
        }

        async fn close(&self) -> Result<()> {
            Ok(())
        }

        fn is_ready(&self) -> bool {
            true
        }

        async fn end_input(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn completed_control_request_is_removed_from_inflight() {
        let transport = TestTransport::new();
        let query = Arc::new(Query::new(
            Arc::new(transport.clone()),
            true,
            None,
            None,
            Default::default(),
            Duration::from_secs(1),
            None,
            None,
            None,
        ));
        query.hook_callbacks.lock().await.insert(
            "hook_0".to_string(),
            Arc::new(|_, _, _| Box::pin(async { Ok(crate::types::HookJsonOutput::empty()) })),
        );
        query.start().await;
        transport.push(json!({
            "type": "control_request",
            "request_id": "fast_1",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "hook_0",
                "input": {
                    "hook_event_name": "UserPromptSubmit",
                    "session_id": "sess",
                    "transcript_path": "/tmp/transcript.jsonl",
                    "cwd": "/tmp",
                    "prompt": "hello",
                },
            },
        }));

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let writes = transport.writes().await;
                if writes.iter().any(|w| w.contains("control_response")) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !query.inflight_requests.lock().await.contains_key("fast_1") {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        query.close().await.unwrap();
    }

    #[tokio::test]
    async fn send_control_request_returns_empty_object_for_non_object_response_like_python() {
        let transport = TestTransport::new();
        let query = Arc::new(Query::new(
            Arc::new(transport.clone()),
            true,
            None,
            None,
            Default::default(),
            Duration::from_secs(1),
            None,
            None,
            None,
        ));
        query.start().await;

        let waiter = {
            let query = query.clone();
            tokio::spawn(async move {
                query
                    .send_control_request(json!({"subtype": "custom"}), Duration::from_secs(1))
                    .await
            })
        };

        let request_id = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let writes = transport.writes().await;
                if let Some(request_id) = writes
                    .iter()
                    .filter_map(|write| serde_json::from_str::<Value>(write.trim()).ok())
                    .find(|value| value["type"] == "control_request")
                    .and_then(|value| {
                        value
                            .get("request_id")
                            .and_then(Value::as_str)
                            .map(ToString::to_string)
                    })
                {
                    break request_id;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        transport.push(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
                "response": "not-a-dict",
            },
        }));

        let result = waiter.await.unwrap().unwrap();
        assert_eq!(result, json!({}));
        query.close().await.unwrap();
    }

    struct CancelGuard(Arc<AtomicBool>);

    impl Drop for CancelGuard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn cancel_request_aborts_inflight_hook_without_response() {
        let transport = TestTransport::new();
        let query = Arc::new(Query::new(
            Arc::new(transport.clone()),
            true,
            None,
            None,
            Default::default(),
            Duration::from_secs(1),
            None,
            None,
            None,
        ));
        let started = Arc::new(Notify::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let started_for_hook = started.clone();
        let cancelled_for_hook = cancelled.clone();
        query.hook_callbacks.lock().await.insert(
            "hook_0".to_string(),
            Arc::new(move |_, _, _| {
                let started = started_for_hook.clone();
                let cancelled = cancelled_for_hook.clone();
                Box::pin(async move {
                    let _guard = CancelGuard(cancelled);
                    started.notify_one();
                    futures::future::pending::<()>().await;
                    Ok(crate::types::HookJsonOutput::empty())
                })
            }),
        );
        query.start().await;
        transport.push(json!({
            "type": "control_request",
            "request_id": "hook_1",
            "request": {
                "subtype": "hook_callback",
                "callback_id": "hook_0",
                "input": {
                    "hook_event_name": "UserPromptSubmit",
                    "session_id": "sess",
                    "transcript_path": "/tmp/transcript.jsonl",
                    "cwd": "/tmp",
                    "prompt": "hello",
                },
            },
        }));
        started.notified().await;
        transport.push(json!({
            "type": "control_cancel_request",
            "request_id": "hook_1",
        }));

        tokio::time::timeout(Duration::from_secs(2), async {
            while !cancelled.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!query.inflight_requests.lock().await.contains_key("hook_1"));
        assert!(
            transport
                .writes()
                .await
                .iter()
                .all(|w| !w.contains("control_response"))
        );
        query.close().await.unwrap();
    }
}
