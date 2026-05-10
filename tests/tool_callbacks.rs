use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use claude_agent_sdk::internal::query::Query;
use claude_agent_sdk::{
    CanUseTool, ClaudeAgentOptions, ClaudeSdkError, HookCallback, HookInput, HookJsonOutput,
    HookMatcher, HookSpecificOutput, NotificationHookSpecificOutput, PermissionBehavior,
    PermissionRequestHookSpecificOutput, PermissionResult, PermissionRuleValue, PermissionUpdate,
    PermissionUpdateDestination, PostToolUseHookSpecificOutput, PreToolUseHookSpecificOutput,
    Result, SubagentStartHookSpecificOutput, SyncHookJsonOutput, ToolInput, ToolPermissionContext,
    Transport,
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
                    updated_input: Some(
                        ToolInput::new(json!({"file_path": "/tmp/file", "safe_mode": true}))
                            .unwrap(),
                    ),
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
            let HookInput::PreToolUse(input) = input else {
                panic!("expected PreToolUse hook input");
            };
            assert_eq!(input.tool_input["test"], "data");
            assert_eq!(tool_use_id.as_deref(), Some("tool-456"));
            Ok(HookJsonOutput::Sync(SyncHookJsonOutput {
                continue_: Some(false),
                suppress_output: Some(false),
                stop_reason: Some("Testing field conversion".to_string()),
                decision: Some("block".to_string()),
                system_message: Some("Fields should be converted".to_string()),
                reason: Some("Test reason for blocking".to_string()),
                hook_specific_output: Some(HookSpecificOutput::PreToolUse(
                    PreToolUseHookSpecificOutput {
                        hook_event_name: "PreToolUse".to_string(),
                        permission_decision: Some("deny".to_string()),
                        permission_decision_reason: Some("Security policy violation".to_string()),
                        updated_input: Some(ToolInput::new(json!({"modified": "input"})).unwrap()),
                        additional_context: None,
                    },
                )),
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
            "input": {
                "hook_event_name": "PreToolUse",
                "session_id": "sess-1",
                "transcript_path": "/tmp/t",
                "cwd": "/home",
                "tool_name": "TestTool",
                "tool_input": {"test": "data"},
                "tool_use_id": "tool-456",
            },
            "tool_use_id": "tool-456",
        },
    }));
    let response = transport.wait_for_control_response("hook-1").await;
    let result = &response["response"]["response"];
    assert_eq!(result["continue"], false);
    assert!(result.get("continue_").is_none());
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
            let hook_specific_output = match input {
                HookInput::Notification(_) => {
                    HookSpecificOutput::Notification(NotificationHookSpecificOutput {
                        hook_event_name: "Notification".to_string(),
                        additional_context: Some("Notification processed".to_string()),
                    })
                }
                HookInput::PermissionRequest(_) => {
                    HookSpecificOutput::PermissionRequest(PermissionRequestHookSpecificOutput {
                        hook_event_name: "PermissionRequest".to_string(),
                        decision: json!({"type": "allow"}),
                    })
                }
                HookInput::SubagentStart(_) => {
                    HookSpecificOutput::SubagentStart(SubagentStartHookSpecificOutput {
                        hook_event_name: "SubagentStart".to_string(),
                        additional_context: Some("Subagent approved".to_string()),
                    })
                }
                HookInput::PostToolUse(_) => {
                    HookSpecificOutput::PostToolUse(PostToolUseHookSpecificOutput {
                        hook_event_name: "PostToolUse".to_string(),
                        additional_context: None,
                        updated_tool_output: None,
                        updated_mcp_tool_output: Some(json!({"result": "modified output"})),
                    })
                }
                _ => HookSpecificOutput::PreToolUse(PreToolUseHookSpecificOutput {
                    hook_event_name: "PreToolUse".to_string(),
                    permission_decision: Some("allow".to_string()),
                    permission_decision_reason: None,
                    updated_input: None,
                    additional_context: Some("Extra context for Claude".to_string()),
                }),
            };
            Ok(HookJsonOutput::Sync(SyncHookJsonOutput {
                hook_specific_output: Some(hook_specific_output),
                ..Default::default()
            }))
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
        let input = match event {
            "Notification" => json!({
                "hook_event_name": event,
                "session_id": "sess-1",
                "transcript_path": "/tmp/t",
                "cwd": "/home",
                "message": "done",
                "notification_type": "info",
            }),
            "PermissionRequest" => json!({
                "hook_event_name": event,
                "session_id": "sess-1",
                "transcript_path": "/tmp/t",
                "cwd": "/home",
                "tool_name": "Bash",
                "tool_input": {"command": "pwd"},
            }),
            "SubagentStart" => json!({
                "hook_event_name": event,
                "session_id": "sess-1",
                "transcript_path": "/tmp/t",
                "cwd": "/home",
                "agent_id": "agent-1",
                "agent_type": "coder",
            }),
            "PostToolUse" => json!({
                "hook_event_name": event,
                "session_id": "sess-1",
                "transcript_path": "/tmp/t",
                "cwd": "/home",
                "tool_name": "Bash",
                "tool_input": {"command": "pwd"},
                "tool_response": {"stdout": "/home"},
                "tool_use_id": "tool-1",
            }),
            _ => json!({
                "hook_event_name": event,
                "session_id": "sess-1",
                "transcript_path": "/tmp/t",
                "cwd": "/home",
                "tool_name": "Bash",
                "tool_input": {"command": "pwd"},
                "tool_use_id": "tool-1",
            }),
        };
        transport.push(json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "hook_callback",
                "callback_id": hook_id,
                "input": input,
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
