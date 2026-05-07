use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use claude_agent_sdk::{
    AssistantMessage, ClaudeAgentOptions, ContentBlock, Message, Prompt, Result, ResultMessage,
    ToolUseBlock, Transport, query,
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
    response_messages: Vec<Value>,
    sent_responses: Arc<AtomicBool>,
}

impl MockTransport {
    fn new(response_messages: Vec<Value>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(std::sync::Mutex::new(Some(rx))),
            writes: Arc::new(Mutex::new(Vec::new())),
            response_messages,
            sent_responses: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn writes(&self) -> Vec<String> {
        self.writes.lock().await.clone()
    }

    fn send_responses_after_user(&self) {
        if self.sent_responses.swap(true, Ordering::SeqCst) {
            return;
        }
        let tx = self.tx.clone();
        let messages = self.response_messages.clone();
        tokio::spawn(async move {
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
        Ok(())
    }

    async fn write(&self, data: &str) -> Result<()> {
        self.writes.lock().await.push(data.to_string());
        let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
            return Ok(());
        };
        match value.get("type").and_then(Value::as_str) {
            Some("control_request")
                if value
                    .get("request")
                    .and_then(|r| r.get("subtype"))
                    .and_then(Value::as_str)
                    == Some("initialize") =>
            {
                let request_id = value
                    .get("request_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.tx
                    .send(Ok(json!({
                        "type": "control_response",
                        "response": {
                            "subtype": "success",
                            "request_id": request_id,
                            "response": {"commands": []},
                        },
                    })))
                    .map_err(|err| claude_agent_sdk::ClaudeSdkError::Runtime(err.to_string()))?;
            }
            Some("user") => self.send_responses_after_user(),
            _ => {}
        }
        Ok(())
    }

    fn read_messages(&self) -> BoxStream<'static, Result<Value>> {
        let rx = self.rx.lock().unwrap().take();
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
async fn simple_query_response_matches_python_integration_test() {
    let transport = MockTransport::new(vec![
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "2 + 2 equals 4"}],
                "model": "claude-opus-4-1-20250805",
            },
        }),
        json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1000,
            "duration_api_ms": 800,
            "is_error": false,
            "num_turns": 1,
            "session_id": "test-session",
            "total_cost_usd": 0.001,
        }),
    ]);
    let messages = collect_query(
        Prompt::Text("What is 2 + 2?".to_string()),
        ClaudeAgentOptions::default(),
        transport,
    )
    .await;

    assert_eq!(messages.len(), 2);
    let Message::Assistant(AssistantMessage { content, .. }) = &messages[0] else {
        panic!("expected assistant message");
    };
    assert!(matches!(&content[0], ContentBlock::Text { text } if text == "2 + 2 equals 4"));

    let Message::Result(ResultMessage {
        total_cost_usd,
        session_id,
        ..
    }) = &messages[1]
    else {
        panic!("expected result message");
    };
    assert_eq!(*total_cost_usd, Some(0.001));
    assert_eq!(session_id, "test-session");
}

#[tokio::test]
async fn query_with_tool_use_matches_python_integration_test() {
    let transport = MockTransport::new(vec![
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "Let me read that file for you."},
                    {"type": "tool_use", "id": "tool-123", "name": "Read", "input": {"file_path": "/test.txt"}}
                ],
                "model": "claude-opus-4-1-20250805",
            },
        }),
        json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1500,
            "duration_api_ms": 1200,
            "is_error": false,
            "num_turns": 1,
            "session_id": "test-session-2",
            "total_cost_usd": 0.002,
        }),
    ]);
    let mut options = ClaudeAgentOptions::default();
    options.allowed_tools.push("Read".to_string());
    let messages = collect_query(
        Prompt::Text("Read /test.txt".to_string()),
        options,
        transport,
    )
    .await;

    let Message::Assistant(AssistantMessage { content, .. }) = &messages[0] else {
        panic!("expected assistant message");
    };
    assert!(
        matches!(&content[0], ContentBlock::Text { text } if text == "Let me read that file for you.")
    );
    assert!(matches!(
        &content[1],
        ContentBlock::ToolUse { id, name, input }
            if id == "tool-123" && name == "Read" && input["file_path"] == "/test.txt"
    ));
    let tool: ToolUseBlock = match &content[1] {
        ContentBlock::ToolUse { id, name, input } => ToolUseBlock {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
        },
        _ => panic!("expected tool use"),
    };
    assert_eq!(tool.name, "Read");
}

#[tokio::test]
async fn max_budget_result_matches_python_integration_test() {
    let transport = MockTransport::new(vec![
        json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "Starting to read..."}],
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
    let mut options = ClaudeAgentOptions::default();
    options.max_budget_usd = Some(0.0001);
    let messages = collect_query(
        Prompt::Text("Read the readme".to_string()),
        options,
        transport,
    )
    .await;

    let Message::Result(ResultMessage {
        subtype,
        is_error,
        total_cost_usd,
        ..
    }) = &messages[1]
    else {
        panic!("expected result message");
    };
    assert_eq!(subtype, "error_max_budget_usd");
    assert!(!is_error);
    assert_eq!(*total_cost_usd, Some(0.0002));
}

#[tokio::test]
async fn continuation_option_reaches_transport_command_path() {
    let transport = MockTransport::new(vec![json!({
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [{"type": "text", "text": "Continuing from previous conversation"}],
            "model": "claude-opus-4-1-20250805",
        },
    })]);
    let mut options = ClaudeAgentOptions::default();
    options.continue_conversation = true;
    let messages = collect_query(
        Prompt::Text("Continue".to_string()),
        options,
        transport.clone(),
    )
    .await;
    assert_eq!(messages.len(), 1);
    assert!(
        transport
            .writes()
            .await
            .iter()
            .any(|write| write.contains("\"type\":\"user\""))
    );
}

async fn collect_query(
    prompt: Prompt,
    options: ClaudeAgentOptions,
    transport: MockTransport,
) -> Vec<Message> {
    let mut stream = query(prompt, Some(options), Some(Arc::new(transport)));
    let mut messages = Vec::new();
    while let Some(message) = stream.next().await {
        messages.push(message.unwrap());
    }
    messages
}
