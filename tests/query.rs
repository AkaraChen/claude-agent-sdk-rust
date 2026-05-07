use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use claude_agent_sdk::internal::query::Query;
use claude_agent_sdk::{
    CanUseTool, ClaudeAgentOptions, ContentBlock, HookMatcher, Message, Prompt, Result,
    SdkMcpServer, Skills, Transport, create_sdk_mcp_server, query,
};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

#[derive(Clone)]
struct MockTransport {
    inner: Arc<MockTransportInner>,
}

struct MockTransportInner {
    tx: mpsc::UnboundedSender<Result<Value>>,
    rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<Result<Value>>>>,
    writes: Mutex<Vec<String>>,
    events: Mutex<Vec<String>>,
    connected: AtomicBool,
    closed: AtomicBool,
    end_input_count: AtomicUsize,
    response_messages: Vec<Value>,
    control_requests_after_user: Vec<Value>,
    sent_response_messages: AtomicBool,
}

impl MockTransport {
    fn new(response_messages: Vec<Value>) -> Self {
        Self::with_control_requests(response_messages, Vec::new())
    }

    fn with_control_requests(
        response_messages: Vec<Value>,
        control_requests_after_user: Vec<Value>,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(MockTransportInner {
                tx,
                rx: std::sync::Mutex::new(Some(rx)),
                writes: Mutex::new(Vec::new()),
                events: Mutex::new(Vec::new()),
                connected: AtomicBool::new(false),
                closed: AtomicBool::new(false),
                end_input_count: AtomicUsize::new(0),
                response_messages,
                control_requests_after_user,
                sent_response_messages: AtomicBool::new(false),
            }),
        }
    }

    async fn writes(&self) -> Vec<String> {
        self.inner.writes.lock().await.clone()
    }

    fn end_input_count(&self) -> usize {
        self.inner.end_input_count.load(Ordering::SeqCst)
    }

    fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    fn push(&self, value: Value) {
        self.inner.tx.send(Ok(value)).unwrap();
    }

    fn send_response_messages_after_user(&self) {
        if self
            .inner
            .sent_response_messages
            .swap(true, Ordering::SeqCst)
        {
            return;
        }
        let tx = self.inner.tx.clone();
        let control = self.inner.control_requests_after_user.clone();
        let messages = self.inner.response_messages.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            for message in control {
                let _ = tx.send(Ok(message));
            }
            for message in messages {
                let _ = tx.send(Ok(message));
            }
            let _ = tx.send(Ok(json!({"__mock_end": true})));
        });
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn connect(&self) -> Result<()> {
        self.inner.connected.store(true, Ordering::SeqCst);
        self.inner.events.lock().await.push("connect".to_string());
        Ok(())
    }

    async fn write(&self, data: &str) -> Result<()> {
        self.inner.writes.lock().await.push(data.to_string());
        self.inner.events.lock().await.push("write".to_string());
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(data.trim()) {
            match obj.get("type").and_then(Value::as_str) {
                Some("control_request") => {
                    if obj
                        .get("request")
                        .and_then(|r| r.get("subtype"))
                        .and_then(Value::as_str)
                        == Some("initialize")
                    {
                        let request_id = obj
                            .get("request_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.inner
                            .tx
                            .send(Ok(json!({
                                "type": "control_response",
                                "response": {
                                    "subtype": "success",
                                    "request_id": request_id,
                                    "response": {"commands": []},
                                },
                            })))
                            .map_err(|err| {
                                claude_agent_sdk::ClaudeSdkError::Runtime(err.to_string())
                            })?;
                    }
                }
                Some("user") => self.send_response_messages_after_user(),
                Some("control_response") => {}
                _ => {}
            }
        }
        Ok(())
    }

    fn read_messages(&self) -> BoxStream<'static, Result<Value>> {
        let rx = self.inner.rx.lock().unwrap().take();
        match rx {
            Some(mut rx) => Box::pin(async_stream::try_stream! {
                while let Some(item) = rx.recv().await {
                    let value = item?;
                    if value.get("__mock_end").and_then(Value::as_bool) == Some(true) {
                        break;
                    }
                    yield value;
                }
            }),
            None => Box::pin(stream::once(async {
                Err(claude_agent_sdk::ClaudeSdkError::Runtime(
                    "read_messages called twice".to_string(),
                ))
            })),
        }
    }

    async fn close(&self) -> Result<()> {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.inner.events.lock().await.push("close".to_string());
        Ok(())
    }

    fn is_ready(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst) && !self.inner.closed.load(Ordering::SeqCst)
    }

    async fn end_input(&self) -> Result<()> {
        self.inner.end_input_count.fetch_add(1, Ordering::SeqCst);
        self.inner.events.lock().await.push("end_input".to_string());
        Ok(())
    }
}

fn assistant_and_result() -> Vec<Value> {
    vec![
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "Hello!"}],
                "model": "claude-sonnet-4-20250514",
            },
        }),
        json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 100,
            "duration_api_ms": 80,
            "is_error": false,
            "num_turns": 1,
            "session_id": "test",
            "total_cost_usd": 0.001,
        }),
    ]
}

fn mcp_control_requests() -> Vec<Value> {
    vec![
        json!({
            "type": "control_request",
            "request_id": "mcp_init_1",
            "request": {
                "subtype": "mcp_message",
                "server_name": "greeter",
                "message": {
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {},
                },
            },
        }),
        json!({
            "type": "control_request",
            "request_id": "mcp_init_2",
            "request": {
                "subtype": "mcp_message",
                "server_name": "greeter",
                "message": {
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/list",
                    "params": {},
                },
            },
        }),
    ]
}

fn options_with_server(server: Arc<SdkMcpServer>) -> ClaudeAgentOptions {
    ClaudeAgentOptions::default().with_sdk_mcp_server("greeter", server)
}

#[tokio::test]
async fn initialize_sends_exclude_dynamic_sections_and_skills_list_only() {
    let transport = MockTransport::new(Vec::new());
    let query = Arc::new(Query::new(
        Arc::new(transport.clone()),
        true,
        None,
        None,
        Default::default(),
        Duration::from_secs(1),
        None,
        Some(true),
        Some(Skills::List(vec!["pdf".to_string(), "docx".to_string()])),
    ));
    query.start().await;
    query.initialize().await.unwrap();
    query.close().await.unwrap();

    let writes = transport.writes().await;
    let request = serde_json::from_str::<Value>(writes[0].trim()).unwrap();
    assert_eq!(request["type"], "control_request");
    assert_eq!(request["request"]["subtype"], "initialize");
    assert_eq!(request["request"]["excludeDynamicSections"], true);
    assert_eq!(request["request"]["skills"], json!(["pdf", "docx"]));

    let transport = MockTransport::new(Vec::new());
    let query = Arc::new(Query::new(
        Arc::new(transport.clone()),
        true,
        None,
        None,
        Default::default(),
        Duration::from_secs(1),
        None,
        None,
        Some(Skills::All("all".to_string())),
    ));
    query.start().await;
    query.initialize().await.unwrap();
    query.close().await.unwrap();
    let writes = transport.writes().await;
    let request = serde_json::from_str::<Value>(writes[0].trim()).unwrap();
    assert!(request["request"].get("excludeDynamicSections").is_none());
    assert!(request["request"].get("skills").is_none());
}

#[tokio::test]
async fn string_prompt_query_writes_user_message_and_ends_input() {
    let transport = MockTransport::new(assistant_and_result());
    let mut stream = query(
        Prompt::Text("Hello".to_string()),
        Some(ClaudeAgentOptions::default()),
        Some(Arc::new(transport.clone())),
    );
    let mut messages = Vec::new();
    while let Some(message) = stream.next().await {
        messages.push(message.unwrap());
    }

    assert_eq!(messages.len(), 2);
    match &messages[0] {
        Message::Assistant(msg) => {
            assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "Hello!"));
        }
        other => panic!("expected assistant, got {other:?}"),
    }
    assert!(matches!(messages[1], Message::Result(_)));
    assert_eq!(transport.end_input_count(), 1);

    let writes = transport.writes().await;
    let user = writes
        .iter()
        .filter_map(|write| serde_json::from_str::<Value>(write.trim()).ok())
        .find(|value| value["type"] == "user")
        .unwrap();
    assert_eq!(user["message"]["content"], "Hello");
    assert_eq!(user["session_id"], "");
}

#[tokio::test]
async fn query_with_tool_use_and_budget_result_parses_full_response() {
    let transport = MockTransport::new(vec![
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Let me read that file for you."},
                    {
                        "type": "tool_use",
                        "id": "tool-123",
                        "name": "Read",
                        "input": {"file_path": "/test.txt"}
                    }
                ],
                "model": "claude-opus-4-1-20250805",
            },
        }),
        json!({
            "type": "result",
            "subtype": "error_max_budget_usd",
            "duration_ms": 500,
            "duration_api_ms": 400,
            "is_error": false,
            "num_turns": 1,
            "session_id": "test-session-budget",
            "total_cost_usd": 0.0002,
            "usage": {"input_tokens": 100, "output_tokens": 50},
        }),
    ]);
    let mut stream = query(
        Prompt::Text("Read /test.txt".to_string()),
        Some(ClaudeAgentOptions {
            allowed_tools: vec!["Read".to_string()],
            max_budget_usd: Some(0.0001),
            ..Default::default()
        }),
        Some(Arc::new(transport)),
    );
    let mut messages = Vec::new();
    while let Some(message) = stream.next().await {
        messages.push(message.unwrap());
    }

    assert_eq!(messages.len(), 2);
    let Message::Assistant(assistant) = &messages[0] else {
        panic!("expected assistant message");
    };
    assert!(matches!(
        &assistant.content[0],
        ContentBlock::Text { text } if text == "Let me read that file for you."
    ));
    assert!(matches!(
        &assistant.content[1],
        ContentBlock::ToolUse { id, name, input }
            if id == "tool-123" && name == "Read" && input["file_path"] == "/test.txt"
    ));

    let Message::Result(result) = &messages[1] else {
        panic!("expected result message");
    };
    assert_eq!(result.subtype, "error_max_budget_usd");
    assert!(!result.is_error);
    assert_eq!(result.session_id, "test-session-budget");
    assert_eq!(result.total_cost_usd, Some(0.0002));
    assert_eq!(result.usage.as_ref().unwrap()["input_tokens"], 100);
}

#[tokio::test]
async fn string_prompt_sdk_mcp_control_requests_are_answered_before_end_input() {
    let server = create_sdk_mcp_server("greeter", "1.0.0", vec![]);
    let transport =
        MockTransport::with_control_requests(assistant_and_result(), mcp_control_requests());
    let mut stream = query(
        Prompt::Text("Greet Alice".to_string()),
        Some(options_with_server(server)),
        Some(Arc::new(transport.clone())),
    );
    let mut messages = Vec::new();
    while let Some(message) = stream.next().await {
        messages.push(message.unwrap());
    }

    assert_eq!(messages.len(), 2);
    assert_eq!(transport.end_input_count(), 1);
    let writes = transport.writes().await;
    let control_responses = writes
        .iter()
        .filter_map(|write| serde_json::from_str::<Value>(write.trim()).ok())
        .filter(|value| value["type"] == "control_response")
        .collect::<Vec<_>>();
    assert_eq!(control_responses.len(), 2);
    assert!(
        control_responses
            .iter()
            .all(|value| value["response"]["subtype"] == "success")
    );
}

#[tokio::test]
async fn wait_for_result_and_end_input_waits_for_hooks_until_result() {
    let transport = MockTransport::new(Vec::new());
    let mut hooks = std::collections::HashMap::new();
    hooks.insert(
        "PreToolUse".to_string(),
        vec![HookMatcher {
            matcher: Some("Bash".to_string()),
            hooks: Vec::new(),
            timeout: None,
        }],
    );
    let query = Arc::new(Query::new(
        Arc::new(transport.clone()),
        true,
        None,
        Some(hooks),
        Default::default(),
        Duration::from_secs(1),
        None,
        None,
        None,
    ));
    query.start().await;

    let waiter = {
        let query = query.clone();
        tokio::spawn(async move { query.wait_for_result_and_end_input().await })
    };
    tokio::time::sleep(Duration::from_millis(25)).await;
    assert_eq!(transport.end_input_count(), 0);

    transport.push(json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 100,
        "duration_api_ms": 80,
        "is_error": false,
        "num_turns": 1,
        "session_id": "test",
    }));
    tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(transport.end_input_count(), 1);
    query.close().await.unwrap();
}

#[tokio::test]
async fn wait_for_result_and_end_input_closes_immediately_without_hooks_or_mcp() {
    let transport = MockTransport::new(Vec::new());
    let query = Query::new(
        Arc::new(transport.clone()),
        true,
        None,
        None,
        Default::default(),
        Duration::from_secs(1),
        None,
        None,
        None,
    );

    query.wait_for_result_and_end_input().await.unwrap();
    assert_eq!(transport.end_input_count(), 1);
}

#[tokio::test]
async fn stream_prompt_keeps_control_protocol_alive_until_result() {
    let server = create_sdk_mcp_server("greeter", "1.0.0", vec![]);
    let transport =
        MockTransport::with_control_requests(assistant_and_result(), mcp_control_requests());
    let prompt = stream::iter(vec![json!({
        "type": "user",
        "message": {"role": "user", "content": "Greet Alice"},
    })])
    .boxed();
    let mut stream = query(
        Prompt::Stream(prompt),
        Some(options_with_server(server)),
        Some(Arc::new(transport.clone())),
    );
    let mut messages = Vec::new();
    while let Some(message) = stream.next().await {
        messages.push(message.unwrap());
    }

    assert_eq!(messages.len(), 2);
    assert_eq!(transport.end_input_count(), 1);
    let writes = transport.writes().await;
    let user = writes
        .iter()
        .filter_map(|write| serde_json::from_str::<Value>(write.trim()).ok())
        .find(|value| value["type"] == "user")
        .unwrap();
    assert_eq!(user["message"]["content"], "Greet Alice");
    assert_eq!(user["session_id"], Value::Null);
    assert_eq!(
        writes
            .iter()
            .filter(|write| write.contains("control_response"))
            .count(),
        2
    );
}

#[tokio::test]
async fn unknown_control_cancel_request_is_noop() {
    let transport = MockTransport::with_control_requests(
        assistant_and_result(),
        vec![json!({
            "type": "control_cancel_request",
            "request_id": "nonexistent",
        })],
    );
    let mut stream = query(
        Prompt::Text("Hello".to_string()),
        Some(ClaudeAgentOptions::default()),
        Some(Arc::new(transport.clone())),
    );
    let mut messages = Vec::new();
    while let Some(message) = stream.next().await {
        messages.push(message.unwrap());
    }

    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[0], Message::Assistant(_)));
    assert!(matches!(messages[1], Message::Result(_)));
    assert_eq!(transport.end_input_count(), 1);
}

#[tokio::test]
async fn dropping_query_stream_closes_transport_like_python_finally() {
    let transport = MockTransport::new(assistant_and_result());
    let mut messages = query(
        Prompt::Text("Hello".to_string()),
        Some(ClaudeAgentOptions::default()),
        Some(Arc::new(transport.clone())),
    );

    assert!(matches!(
        messages.next().await.unwrap().unwrap(),
        Message::Assistant(_)
    ));
    drop(messages);

    tokio::time::timeout(Duration::from_secs(1), async {
        while !transport.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn receive_messages_unblocks_when_query_is_closed() {
    let transport = MockTransport::new(Vec::new());
    let query = Arc::new(Query::new(
        Arc::new(transport),
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

    let mut stream = query.receive_messages();
    let consumer = tokio::spawn(async move {
        let mut count = 0;
        while let Some(item) = stream.next().await {
            item.unwrap();
            count += 1;
        }
        count
    });
    tokio::time::sleep(Duration::from_millis(25)).await;
    query.close().await.unwrap();

    let count = tokio::time::timeout(Duration::from_secs(1), consumer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn buffered_messages_drain_after_query_is_closed() {
    let transport = MockTransport::new(Vec::new());
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
    transport.push(json!({"type": "assistant", "message": {"role": "assistant", "content": []}}));
    transport.push(json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1,
        "duration_api_ms": 1,
        "is_error": false,
        "num_turns": 1,
        "session_id": "s",
    }));
    tokio::time::sleep(Duration::from_millis(25)).await;

    let mut stream = query.receive_messages();
    query.close().await.unwrap();
    let mut message_types = Vec::new();
    while let Some(item) = tokio::time::timeout(Duration::from_secs(1), stream.next())
        .await
        .unwrap()
    {
        message_types.push(item.unwrap()["type"].as_str().unwrap().to_string());
    }
    assert_eq!(
        message_types,
        vec!["assistant".to_string(), "result".to_string()]
    );
}

#[tokio::test]
async fn can_use_tool_requires_streaming_prompt_and_no_permission_prompt_tool_name() {
    let callback: CanUseTool = Arc::new(|_, _, _| {
        Box::pin(async {
            Ok(claude_agent_sdk::PermissionResult::Allow {
                updated_input: None,
                updated_permissions: None,
            })
        })
    });

    let mut stream = query(
        Prompt::Text("hello".to_string()),
        Some(ClaudeAgentOptions {
            can_use_tool: Some(callback.clone()),
            ..Default::default()
        }),
        None,
    );
    let err = stream.next().await.unwrap().unwrap_err();
    assert!(
        err.to_string()
            .contains("can_use_tool callback requires streaming mode")
    );

    let mut stream = query(
        Prompt::Stream(stream::empty().boxed()),
        Some(ClaudeAgentOptions {
            can_use_tool: Some(callback),
            permission_prompt_tool_name: Some("stdio".to_string()),
            ..Default::default()
        }),
        None,
    );
    let err = stream.next().await.unwrap().unwrap_err();
    assert!(
        err.to_string()
            .contains("cannot be used with permission_prompt_tool_name")
    );
}
