use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use claude_agent_sdk::internal::query::Query;
use claude_agent_sdk::{
    CanUseTool, ClaudeAgentOptions, ClaudeSdkError, HookCallback, HookMatcher, PermissionBehavior,
    PermissionResult, PermissionRuleValue, PermissionUpdate, PermissionUpdateDestination, Result,
    ToolPermissionContext, Transport,
};
use futures::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

#[derive(Clone)]
struct MockTransport {
    tx: mpsc::UnboundedSender<Result<Value>>,
    rx: Arc<std::sync::Mutex<Option<mpsc::UnboundedReceiver<Result<Value>>>>>,
    writes: Arc<Mutex<Vec<String>>>,
    connected: Arc<AtomicBool>,
}

impl MockTransport {
    fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Arc::new(std::sync::Mutex::new(Some(rx))),
            writes: Arc::new(Mutex::new(Vec::new())),
            connected: Arc::new(AtomicBool::new(false)),
        }
    }

    fn push(&self, value: Value) {
        self.tx.send(Ok(value)).unwrap();
    }

    async fn writes(&self) -> Vec<String> {
        self.writes.lock().await.clone()
    }

    async fn wait_for_control_response(&self, request_id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                for write in self.writes().await {
                    if let Ok(value) = serde_json::from_str::<Value>(write.trim()) {
                        if value["type"] == "control_response"
                            && value["response"]["request_id"] == request_id
                        {
                            return value;
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
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
            if obj.get("type").and_then(Value::as_str) == Some("control_request")
                && obj
                    .get("request")
                    .and_then(|request| request.get("subtype"))
                    .and_then(Value::as_str)
                    == Some("initialize")
            {
                let request_id = obj
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
                    .map_err(|err| ClaudeSdkError::Runtime(err.to_string()))?;
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
                Err(ClaudeSdkError::Runtime(
                    "read_messages called twice".to_string(),
                ))
            })),
        }
    }

    async fn close(&self) -> Result<()> {
        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_ready(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    async fn end_input(&self) -> Result<()> {
        Ok(())
    }
}

fn query_with_callback(transport: MockTransport, callback: Option<CanUseTool>) -> Arc<Query> {
    Arc::new(Query::new(
        Arc::new(transport),
        true,
        callback,
        None,
        Default::default(),
        Duration::from_secs(1),
        None,
        None,
        None,
    ))
}

#[tokio::test]
async fn permission_callback_allow_deny_and_updates_are_serialized() {
    let transport = MockTransport::new();
    let callback: CanUseTool = Arc::new(|tool_name, input, _context| {
        Box::pin(async move {
            match tool_name.as_str() {
                "AllowTool" => {
                    assert_eq!(input, json!({"param": "value"}));
                    Ok(PermissionResult::Allow {
                        updated_input: None,
                        updated_permissions: Some(vec![PermissionUpdate {
                            update_type: "addRules".to_string(),
                            rules: Some(vec![PermissionRuleValue {
                                tool_name: "Bash".to_string(),
                                rule_content: Some("echo *".to_string()),
                            }]),
                            behavior: Some(PermissionBehavior::Allow),
                            mode: None,
                            directories: None,
                            destination: Some(PermissionUpdateDestination::Session),
                        }]),
                    })
                }
                "ModifyTool" => Ok(PermissionResult::Allow {
                    updated_input: Some(json!({"file_path": "/tmp/file", "safe_mode": true})),
                    updated_permissions: None,
                }),
                "DenyTool" => Ok(PermissionResult::Deny {
                    message: "Security policy violation".to_string(),
                    interrupt: true,
                }),
                other => Err(ClaudeSdkError::Runtime(format!("unexpected tool {other}"))),
            }
        })
    });
    let query = query_with_callback(transport.clone(), Some(callback));
    query.start().await;

    for (request_id, tool_name, input) in [
        ("allow-1", "AllowTool", json!({"param": "value"})),
        ("modify-1", "ModifyTool", json!({"file_path": "/tmp/file"})),
        ("deny-1", "DenyTool", json!({"command": "rm -rf /"})),
    ] {
        transport.push(json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "can_use_tool",
                "tool_name": tool_name,
                "input": input,
                "permission_suggestions": [],
            },
        }));
    }

    let allow = transport.wait_for_control_response("allow-1").await;
    assert_eq!(allow["response"]["subtype"], "success");
    assert_eq!(allow["response"]["response"]["behavior"], "allow");
    assert_eq!(
        allow["response"]["response"]["updatedInput"],
        json!({"param": "value"})
    );
    assert_eq!(
        allow["response"]["response"]["updatedPermissions"],
        json!([{
            "type": "addRules",
            "destination": "session",
            "rules": [{"toolName": "Bash", "ruleContent": "echo *"}],
            "behavior": "allow",
        }])
    );

    let modify = transport.wait_for_control_response("modify-1").await;
    assert_eq!(modify["response"]["response"]["behavior"], "allow");
    assert_eq!(
        modify["response"]["response"]["updatedInput"]["safe_mode"],
        true
    );

    let deny = transport.wait_for_control_response("deny-1").await;
    assert_eq!(deny["response"]["response"]["behavior"], "deny");
    assert_eq!(
        deny["response"]["response"]["message"],
        "Security policy violation"
    );
    assert_eq!(deny["response"]["response"]["interrupt"], true);
    query.close().await.unwrap();
}

#[tokio::test]
async fn permission_callback_receives_full_context_and_errors_are_returned() {
    let transport = MockTransport::new();
    let captured: Arc<Mutex<Option<ToolPermissionContext>>> = Arc::new(Mutex::new(None));
    let captured_for_callback = captured.clone();
    let callback: CanUseTool = Arc::new(move |tool_name, _input, context| {
        let captured = captured_for_callback.clone();
        Box::pin(async move {
            if tool_name == "Explode" {
                return Err(ClaudeSdkError::Runtime("Callback error".to_string()));
            }
            *captured.lock().await = Some(context);
            Ok(PermissionResult::Allow {
                updated_input: None,
                updated_permissions: None,
            })
        })
    });
    let query = query_with_callback(transport.clone(), Some(callback));
    query.start().await;

    transport.push(json!({
        "type": "control_request",
        "request_id": "ctx-1",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Bash",
            "input": {"command": "rm -rf /tmp/x"},
            "permission_suggestions": [
                "deny",
                {"type": "allow", "rule": "Bash(echo *)"}
            ],
            "tool_use_id": "toolu_01DEF456",
            "agent_id": "agent-456",
            "blocked_path": "/tmp/x",
            "decision_reason": "PreToolUse hook flagged this as destructive",
            "title": "Claude wants to run a Bash command",
            "display_name": "Bash",
            "description": "rm -rf /tmp/x",
        },
    }));
    let response = transport.wait_for_control_response("ctx-1").await;
    assert_eq!(response["response"]["subtype"], "success");

    let context = captured.lock().await.clone().unwrap();
    assert_eq!(
        context.suggestions,
        vec![
            json!("deny"),
            json!({"type": "allow", "rule": "Bash(echo *)"}),
        ]
    );
    assert_eq!(context.tool_use_id.as_deref(), Some("toolu_01DEF456"));
    assert_eq!(context.agent_id.as_deref(), Some("agent-456"));
    assert_eq!(context.blocked_path.as_deref(), Some("/tmp/x"));
    assert_eq!(
        context.decision_reason.as_deref(),
        Some("PreToolUse hook flagged this as destructive")
    );
    assert_eq!(
        context.title.as_deref(),
        Some("Claude wants to run a Bash command")
    );
    assert_eq!(context.display_name.as_deref(), Some("Bash"));
    assert_eq!(context.description.as_deref(), Some("rm -rf /tmp/x"));

    transport.push(json!({
        "type": "control_request",
        "request_id": "err-1",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Explode",
            "input": {},
            "permission_suggestions": [],
        },
    }));
    let error = transport.wait_for_control_response("err-1").await;
    assert_eq!(error["response"]["subtype"], "error");
    assert!(
        error["response"]["error"]
            .as_str()
            .unwrap()
            .contains("Callback error")
    );
    query.close().await.unwrap();
}

#[tokio::test]
async fn hook_callbacks_are_registered_and_outputs_are_converted() {
    let transport = MockTransport::new();
    let hook: HookCallback = Arc::new(|input, tool_use_id, _context| {
        Box::pin(async move {
            assert_eq!(input["test"], "data");
            assert_eq!(tool_use_id.as_deref(), Some("tool-456"));
            Ok(json!({
                "async_": true,
                "asyncTimeout": 5000,
                "continue_": false,
                "suppressOutput": false,
                "stopReason": "Testing field conversion",
                "systemMessage": "Fields should be converted",
                "decision": "block",
                "reason": "Test reason for blocking",
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": "Security policy violation",
                    "updatedInput": {"modified": "input"},
                },
            }))
        })
    });
    let mut hooks = std::collections::HashMap::new();
    hooks.insert(
        "PreToolUse".to_string(),
        vec![HookMatcher {
            matcher: Some("TestTool".to_string()),
            hooks: vec![hook],
            timeout: Some(2.5),
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
    query.initialize().await.unwrap();

    let init = transport.writes().await[0].clone();
    let init = serde_json::from_str::<Value>(init.trim()).unwrap();
    let hook_id = init["request"]["hooks"]["PreToolUse"][0]["hookCallbackIds"][0]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        init["request"]["hooks"]["PreToolUse"][0]["matcher"],
        "TestTool"
    );
    assert_eq!(init["request"]["hooks"]["PreToolUse"][0]["timeout"], 2.5);

    transport.push(json!({
        "type": "control_request",
        "request_id": "hook-1",
        "request": {
            "subtype": "hook_callback",
            "callback_id": hook_id,
            "input": {"test": "data"},
            "tool_use_id": "tool-456",
        },
    }));
    let response = transport.wait_for_control_response("hook-1").await;
    let result = &response["response"]["response"];
    assert_eq!(result["async"], true);
    assert!(result.get("async_").is_none());
    assert_eq!(result["continue"], false);
    assert!(result.get("continue_").is_none());
    assert_eq!(result["asyncTimeout"], 5000);
    assert_eq!(result["suppressOutput"], false);
    assert_eq!(result["stopReason"], "Testing field conversion");
    assert_eq!(result["systemMessage"], "Fields should be converted");
    assert_eq!(result["decision"], "block");
    assert_eq!(result["reason"], "Test reason for blocking");
    assert_eq!(
        result["hookSpecificOutput"]["permissionDecisionReason"],
        "Security policy violation"
    );
    query.close().await.unwrap();
}

#[tokio::test]
async fn new_hook_event_shapes_round_trip_through_control_callback() {
    let transport = MockTransport::new();
    let hook: HookCallback = Arc::new(|input, _tool_use_id, _context| {
        Box::pin(async move {
            let event = input["hook_event_name"].as_str().unwrap();
            let output = match event {
                "Notification" => json!({
                    "hookSpecificOutput": {
                        "hookEventName": "Notification",
                        "additionalContext": "Notification processed",
                    },
                }),
                "PermissionRequest" => json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PermissionRequest",
                        "decision": {"type": "allow"},
                    },
                }),
                "SubagentStart" => json!({
                    "hookSpecificOutput": {
                        "hookEventName": "SubagentStart",
                        "additionalContext": "Subagent approved",
                    },
                }),
                "PostToolUse" => json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PostToolUse",
                        "updatedMCPToolOutput": {"result": "modified output"},
                    },
                }),
                _ => json!({
                    "hookSpecificOutput": {
                        "hookEventName": "PreToolUse",
                        "permissionDecision": "allow",
                        "additionalContext": "Extra context for Claude",
                    },
                }),
            };
            Ok(output)
        })
    });
    let mut hooks = std::collections::HashMap::new();
    for event in [
        "Notification",
        "PermissionRequest",
        "SubagentStart",
        "PostToolUse",
        "PreToolUse",
    ] {
        hooks.insert(
            event.to_string(),
            vec![HookMatcher {
                matcher: None,
                hooks: vec![hook.clone()],
                timeout: None,
            }],
        );
    }
    let options = ClaudeAgentOptions {
        hooks: Some(hooks.clone()),
        ..Default::default()
    };
    assert!(options.hooks.as_ref().unwrap().contains_key("Notification"));
    assert!(
        options
            .hooks
            .as_ref()
            .unwrap()
            .contains_key("SubagentStart")
    );
    assert!(
        options
            .hooks
            .as_ref()
            .unwrap()
            .contains_key("PermissionRequest")
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
    query.initialize().await.unwrap();

    let init = serde_json::from_str::<Value>(transport.writes().await[0].trim()).unwrap();
    for (index, event) in [
        "Notification",
        "PermissionRequest",
        "SubagentStart",
        "PostToolUse",
        "PreToolUse",
    ]
    .into_iter()
    .enumerate()
    {
        let hook_id = init["request"]["hooks"][event][0]["hookCallbackIds"][0]
            .as_str()
            .unwrap()
            .to_string();
        let request_id = format!("event-{index}");
        transport.push(json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "hook_callback",
                "callback_id": hook_id,
                "input": {
                    "hook_event_name": event,
                    "session_id": "sess-1",
                    "transcript_path": "/tmp/t",
                    "cwd": "/home",
                },
                "tool_use_id": Value::Null,
            },
        }));
        let response = transport.wait_for_control_response(&request_id).await;
        assert_eq!(
            response["response"]["response"]["hookSpecificOutput"]["hookEventName"],
            event
        );
    }
    query.close().await.unwrap();
}
