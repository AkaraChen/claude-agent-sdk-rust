use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use claude_agent_sdk::{
    CanUseTool, ClaudeAgentOptions, ClaudeSdkClient, ContentBlock, InputMessage,
    McpServerConnectionStatus, Message, PermissionMode, Prompt, Result, Transport,
};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

#[derive(Clone)]
struct MockTransport {
    tx: mpsc::UnboundedSender<Result<Value>>,
    rx: Arc<std::sync::Mutex<Option<mpsc::UnboundedReceiver<Result<Value>>>>>,
    writes: Arc<Mutex<Vec<String>>>,
    connected: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    end_input_count: Arc<AtomicUsize>,
    initialize_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl MockTransport {
    fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(std::sync::Mutex::new(Some(rx))),
            writes: Arc::new(Mutex::new(Vec::new())),
            connected: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            end_input_count: Arc::new(AtomicUsize::new(0)),
            initialize_error: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    fn with_initialize_error(error: impl Into<String>) -> Self {
        let transport = Self::new();
        *transport.initialize_error.lock().unwrap() = Some(error.into());
        transport
    }

    async fn writes(&self) -> Vec<String> {
        self.writes.lock().await.clone()
    }

    fn push(&self, value: Value) {
        self.tx.send(Ok(value)).unwrap();
    }

    fn end_input_count(&self) -> usize {
        self.end_input_count.load(Ordering::SeqCst)
    }

    fn control_response_payload(subtype: &str) -> Value {
        match subtype {
            "initialize" => json!({"commands": [], "output_style": "default"}),
            "mcp_status" => json!({
                "mcpServers": [
                    {
                        "name": "my-http-server",
                        "status": "connected",
                        "serverInfo": {"name": "my-http-server", "version": "1.0.0"},
                        "config": {"type": "http", "url": "https://example.com/mcp"},
                        "scope": "project",
                        "tools": [
                            {"name": "greet", "description": "Greet a user", "annotations": {"readOnly": true}},
                            {"name": "reset"},
                        ],
                    },
                    {"name": "failed-server", "status": "failed", "error": "Connection refused"},
                    {
                        "name": "proxy-server",
                        "status": "needs-auth",
                        "config": {
                            "type": "claudeai-proxy",
                            "url": "https://claude.ai/proxy",
                            "id": "proxy-123",
                        },
                    },
                ],
            }),
            "get_context_usage" => json!({
                "categories": [
                    {"name": "System prompt", "tokens": 3200, "color": "#abc"},
                    {"name": "Messages", "tokens": 61400, "color": "#def"},
                ],
                "totalTokens": 98200,
                "maxTokens": 155000,
                "rawMaxTokens": 200000,
                "percentage": 49.1,
                "model": "claude-sonnet-4-5",
                "isAutoCompactEnabled": true,
                "memoryFiles": [{"path": "CLAUDE.md", "type": "project", "tokens": 512}],
                "mcpTools": [{"name": "search", "serverName": "ref", "tokens": 164, "isLoaded": true}],
                "agents": [{"agentType": "coder", "source": "sdk", "tokens": 299}],
                "gridRows": [],
                "apiUsage": null,
            }),
            _ => json!({}),
        }
    }
}

#[async_trait]
impl Transport for MockTransport {
    async fn connect(&self) -> Result<()> {
        self.connected.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn write(&self, data: &str) -> Result<()> {
        self.writes.lock().await.push(data.to_string());
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(data.trim()) {
            if obj.get("type").and_then(Value::as_str) == Some("control_request") {
                let request_id = obj
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let subtype = obj
                    .get("request")
                    .and_then(|r| r.get("subtype"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let response = if subtype == "initialize" {
                    self.initialize_error.lock().unwrap().clone().map(|error| {
                        json!({
                            "type": "control_response",
                            "response": {
                                "subtype": "error",
                                "request_id": request_id,
                                "error": error,
                            },
                        })
                    })
                } else {
                    None
                }
                .unwrap_or_else(|| {
                    let payload = Self::control_response_payload(subtype);
                    json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": request_id,
                            "response": payload,
                        },
                    })
                });
                self.tx
                    .send(Ok(response))
                    .map_err(|err| claude_agent_sdk::ClaudeSdkError::Runtime(err.to_string()))?;
            }
        }
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
                Err(claude_agent_sdk::ClaudeSdkError::Runtime(
                    "read_messages called twice".to_string(),
                ))
            })),
        }
    }

    async fn close(&self) -> Result<()> {
        self.closed.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn is_ready(&self) -> bool {
        self.connected.load(Ordering::SeqCst) && !self.closed.load(Ordering::SeqCst)
    }

    async fn end_input(&self) -> Result<()> {
        self.end_input_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn parse_writes(writes: &[String]) -> Vec<Value> {
    writes
        .iter()
        .filter_map(|write| serde_json::from_str::<Value>(write.trim()).ok())
        .collect()
}

fn control_request(writes: &[String], subtype: &str) -> Value {
    parse_writes(writes)
        .into_iter()
        .find(|value| value["type"] == "control_request" && value["request"]["subtype"] == subtype)
        .unwrap_or_else(|| panic!("missing control request {subtype}"))
}

#[tokio::test]
async fn connect_with_string_prompt_writes_user_message_and_disconnect_closes() {
    let transport = MockTransport::new();
    let mut client = ClaudeSdkClient::new(None, Some(Arc::new(transport.clone())));
    client
        .connect(Some(Prompt::Text("Hello Claude".to_string())))
        .await
        .unwrap();

    let writes = transport.writes().await;
    assert!(control_request(&writes, "initialize")["request"]["hooks"].is_null());
    let user = parse_writes(&writes)
        .into_iter()
        .find(|value| value["type"] == "user")
        .unwrap();
    assert_eq!(user["message"]["content"], "Hello Claude");
    assert_eq!(user["session_id"], "default");

    client.disconnect().await.unwrap();
    assert!(transport.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn connect_with_stream_prompt_writes_messages_and_ends_input() {
    let transport = MockTransport::new();
    let mut client = ClaudeSdkClient::new(None, Some(Arc::new(transport.clone())));
    let prompt = stream::iter(vec![
        InputMessage::user("Hi"),
        InputMessage::user("Bye").with_session_id("existing-session"),
    ])
    .boxed();

    client.connect(Some(Prompt::Stream(prompt))).await.unwrap();

    let mut users = Vec::new();
    for _ in 0..50 {
        users = parse_writes(&transport.writes().await)
            .into_iter()
            .filter(|value| value["type"] == "user")
            .collect::<Vec<_>>();
        if users.len() == 2 && transport.end_input_count() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(users.len(), 2);
    assert_eq!(users[0]["message"]["content"], "Hi");
    assert!(users[0].get("session_id").is_none());
    assert_eq!(users[1]["message"]["content"], "Bye");
    assert_eq!(users[1]["session_id"], "existing-session");
    assert_eq!(transport.end_input_count(), 1);

    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn connect_failure_closes_started_transport() {
    let transport = MockTransport::with_initialize_error("initialize failed");
    let mut client = ClaudeSdkClient::new(None, Some(Arc::new(transport.clone())));
    let err = client.connect(None).await.unwrap_err();
    assert!(err.to_string().contains("initialize failed"));
    assert!(transport.closed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn send_query_uses_default_and_custom_session_ids() {
    let transport = MockTransport::new();
    let mut client = ClaudeSdkClient::new(None, Some(Arc::new(transport.clone())));
    client.connect(None).await.unwrap();

    client
        .send_query(Prompt::Text("Test message".to_string()), None)
        .await
        .unwrap();
    client
        .send_query(
            Prompt::Text("Custom session".to_string()),
            Some("custom-session"),
        )
        .await
        .unwrap();

    let users = parse_writes(&transport.writes().await)
        .into_iter()
        .filter(|value| value["type"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(users[0]["message"]["content"], "Test message");
    assert_eq!(users[0]["session_id"], "default");
    assert_eq!(users[1]["message"]["content"], "Custom session");
    assert_eq!(users[1]["session_id"], "custom-session");
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn methods_fail_when_not_connected() {
    let client = ClaudeSdkClient::new(None, None);
    let err = client.receive_messages().err().unwrap();
    assert!(err.to_string().contains("Not connected"));
    let err = client.receive_response().err().unwrap();
    assert!(err.to_string().contains("Not connected"));
    assert!(
        client
            .interrupt()
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .set_permission_mode(PermissionMode::AcceptEdits)
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .set_model(None)
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .rewind_files("user-message-id".to_string())
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .send_query(Prompt::Text("x".to_string()), None)
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .reconnect_mcp_server("my-server".to_string())
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .toggle_mcp_server("my-server".to_string(), true)
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .stop_task("task-abc123".to_string())
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .get_mcp_status()
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .get_context_usage()
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
    assert!(
        client
            .get_server_info()
            .await
            .unwrap_err()
            .to_string()
            .contains("Not connected")
    );
}

#[tokio::test]
async fn receive_messages_and_receive_response_parse_streams() {
    let transport = MockTransport::new();
    let mut client = ClaudeSdkClient::new(None, Some(Arc::new(transport.clone())));
    client.connect(None).await.unwrap();
    let mut stream = client.receive_messages().unwrap();
    transport.push(json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello!"}],
            "model": "claude-opus-4-1-20250805",
        },
    }));
    transport.push(json!({
        "type": "user",
        "message": {"role": "user", "content": "Hi there"},
    }));

    let first = stream.next().await.unwrap().unwrap();
    let second = stream.next().await.unwrap().unwrap();
    match first {
        Message::Assistant(msg) => {
            assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "Hello!"));
        }
        other => panic!("expected assistant, got {other:?}"),
    }
    match second {
        Message::User(msg) => match msg.content {
            claude_agent_sdk::UserMessageContent::Text(text) => assert_eq!(text, "Hi there"),
            other => panic!("expected text user message, got {other:?}"),
        },
        other => panic!("expected user, got {other:?}"),
    }
    drop(stream);
    client.disconnect().await.unwrap();

    let transport = MockTransport::new();
    let mut client = ClaudeSdkClient::new(None, Some(Arc::new(transport.clone())));
    client.connect(None).await.unwrap();
    let mut response = client.receive_response().unwrap();
    transport.push(json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "Answer"}],
            "model": "claude-opus-4-1-20250805",
        },
    }));
    transport.push(json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1000,
        "duration_api_ms": 800,
        "is_error": false,
        "num_turns": 1,
        "session_id": "test",
        "total_cost_usd": 0.001,
    }));
    transport.push(json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "Should not see this"}],
            "model": "claude-opus-4-1-20250805",
        },
    }));
    assert!(matches!(
        response.next().await.unwrap().unwrap(),
        Message::Assistant(_)
    ));
    assert!(matches!(
        response.next().await.unwrap().unwrap(),
        Message::Result(_)
    ));
    assert!(response.next().await.is_none());
    drop(response);
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn receive_response_can_collect_multiple_messages_until_result() {
    let transport = MockTransport::new();
    let mut client = ClaudeSdkClient::new(None, Some(Arc::new(transport.clone())));
    client.connect(None).await.unwrap();
    let mut response = client.receive_response().unwrap();
    transport.push(json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "Hello"}],
            "model": "claude-opus-4-1-20250805",
        },
    }));
    transport.push(json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "World"}],
            "model": "claude-opus-4-1-20250805",
        },
    }));
    transport.push(json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1000,
        "duration_api_ms": 800,
        "is_error": false,
        "num_turns": 1,
        "session_id": "test",
        "total_cost_usd": 0.001,
    }));

    let mut messages = Vec::new();
    while let Some(message) = response.next().await {
        messages.push(message.unwrap());
    }
    assert_eq!(messages.len(), 3);
    assert!(matches!(messages[0], Message::Assistant(_)));
    assert!(matches!(messages[1], Message::Assistant(_)));
    assert!(matches!(messages[2], Message::Result(_)));
    drop(response);
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn control_methods_send_expected_wire_requests_and_return_payloads() {
    let transport = MockTransport::new();
    let mut client = ClaudeSdkClient::new(None, Some(Arc::new(transport.clone())));
    client.connect(None).await.unwrap();

    client.interrupt().await.unwrap();
    client
        .set_permission_mode(PermissionMode::AcceptEdits)
        .await
        .unwrap();
    client.set_model(Some("sonnet".to_string())).await.unwrap();
    client
        .reconnect_mcp_server("my-server".to_string())
        .await
        .unwrap();
    client
        .toggle_mcp_server("other-server".to_string(), true)
        .await
        .unwrap();
    client.stop_task("task-abc123".to_string()).await.unwrap();

    let status = client.get_mcp_status().await.unwrap();
    assert_eq!(status.mcp_servers[0].name, "my-http-server");
    assert_eq!(
        status.mcp_servers[1].status,
        McpServerConnectionStatus::Failed
    );
    assert_eq!(
        status.mcp_servers[2].status,
        McpServerConnectionStatus::NeedsAuth
    );
    assert_eq!(
        status.mcp_servers[2].config.as_ref().unwrap()["id"],
        "proxy-123"
    );

    let usage = client.get_context_usage().await.unwrap();
    assert_eq!(usage.total_tokens, 98200);
    assert_eq!(usage.mcp_tools[0]["serverName"], "ref");

    let server_info = client.get_server_info().await.unwrap().unwrap();
    assert_eq!(server_info["commands"], json!([]));

    let writes = transport.writes().await;
    assert_eq!(
        control_request(&writes, "set_permission_mode")["request"]["mode"],
        "acceptEdits"
    );
    assert_eq!(
        control_request(&writes, "set_model")["request"]["model"],
        "sonnet"
    );
    assert_eq!(
        control_request(&writes, "mcp_reconnect")["request"]["serverName"],
        "my-server"
    );
    assert_eq!(
        control_request(&writes, "mcp_toggle")["request"]["serverName"],
        "other-server"
    );
    assert_eq!(
        control_request(&writes, "mcp_toggle")["request"]["enabled"],
        true
    );
    assert_eq!(
        control_request(&writes, "stop_task")["request"]["task_id"],
        "task-abc123"
    );
    assert!(control_request(&writes, "interrupt").is_object());
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn stream_send_query_adds_missing_session_id_preserves_existing_and_does_not_end_input() {
    let transport = MockTransport::new();
    let mut client = ClaudeSdkClient::new(None, Some(Arc::new(transport.clone())));
    client.connect(None).await.unwrap();
    let prompt = stream::iter(vec![
        InputMessage::user("streamed"),
        InputMessage::user("already scoped").with_session_id("existing-session"),
    ])
    .boxed();
    client
        .send_query(Prompt::Stream(prompt), Some("stream-session"))
        .await
        .unwrap();

    let users = parse_writes(&transport.writes().await)
        .into_iter()
        .filter(|value| value["type"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 2);
    assert_eq!(users[0]["message"]["content"], "streamed");
    assert_eq!(users[0]["session_id"], "stream-session");
    assert_eq!(users[1]["message"]["content"], "already scoped");
    assert_eq!(users[1]["session_id"], "existing-session");
    assert_eq!(transport.end_input_count(), 0);
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn disconnect_without_connect_is_noop() {
    let mut client = ClaudeSdkClient::new(None, None);
    client.disconnect().await.unwrap();
}

#[tokio::test]
async fn client_can_use_tool_option_validation_matches_python() {
    let callback: CanUseTool = Arc::new(|_, _, _| {
        Box::pin(async {
            Ok(claude_agent_sdk::PermissionResult::Allow {
                updated_input: None,
                updated_permissions: None,
            })
        })
    });

    let mut client = ClaudeSdkClient::new(
        Some(ClaudeAgentOptions {
            can_use_tool: Some(callback.clone()),
            ..Default::default()
        }),
        Some(Arc::new(MockTransport::new())),
    );
    let err = client
        .connect(Some(Prompt::Text("hello".to_string())))
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("can_use_tool callback requires streaming mode")
    );

    let mut client = ClaudeSdkClient::new(
        Some(ClaudeAgentOptions {
            can_use_tool: Some(callback),
            permission_prompt_tool_name: Some("stdio".to_string()),
            ..Default::default()
        }),
        Some(Arc::new(MockTransport::new())),
    );
    let err = client
        .connect(Some(Prompt::Stream(stream::empty().boxed())))
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("cannot be used with permission_prompt_tool_name")
    );
}
