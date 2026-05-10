use claude_agent_sdk::{
    AgentDefinition, AgentEffort, AgentMcpServer, AssistantMessage, BaseHookInput,
    ClaudeAgentOptions, ContentBlock, ContextUsageResponse, EffortLevel, HookJsonOutput,
    HookSpecificOutput, McpServerConfig, McpServerConnectionStatus, McpServerStatus,
    McpStatusResponse, NotificationHookInput, NotificationHookSpecificOutput, PermissionBehavior,
    PermissionMode, PermissionRequestHookInput, PermissionRequestHookSpecificOutput,
    PermissionRuleValue, PermissionUpdate, PermissionUpdateDestination,
    PostToolUseHookSpecificOutput, PreToolUseHookInput, PreToolUseHookSpecificOutput,
    RateLimitInfo, ResultMessage, ServerToolResultBlock, ServerToolUseBlock, SessionStoreFlushMode,
    SubagentStartHookInput, SubagentStartHookSpecificOutput, SyncHookJsonOutput, TextBlock,
    ThinkingBlock, ToolInput, ToolResultBlock, ToolUseBlock, UserMessage, UserMessageContent,
};
use serde_json::json;

fn agent() -> AgentDefinition {
    AgentDefinition {
        description: "test".to_string(),
        prompt: "p".to_string(),
        tools: None,
        disallowed_tools: None,
        model: None,
        skills: None,
        memory: None,
        mcp_servers: None,
        initial_prompt: None,
        max_turns: None,
        background: None,
        effort: None,
        permission_mode: None,
    }
}

#[test]
fn default_options_match_python_defaults() {
    let options = ClaudeAgentOptions::default();
    assert!(options.allowed_tools.is_empty());
    assert!(options.system_prompt.is_none());
    assert!(options.permission_mode.is_none());
    assert!(!options.continue_conversation);
    assert!(options.disallowed_tools.is_empty());
    assert!(options.sdk_mcp_servers.is_empty());
    assert_eq!(options.session_store_flush, SessionStoreFlushMode::Batched);
}

#[test]
fn permission_modes_serialize_to_cli_values() {
    assert_eq!(PermissionMode::Default.to_string(), "default");
    assert_eq!(
        PermissionMode::BypassPermissions.to_string(),
        "bypassPermissions"
    );
    assert_eq!(PermissionMode::Plan.to_string(), "plan");
    assert_eq!(PermissionMode::AcceptEdits.to_string(), "acceptEdits");
    assert_eq!(PermissionMode::DontAsk.to_string(), "dontAsk");
    assert_eq!(PermissionMode::Auto.to_string(), "auto");
    assert_eq!(
        serde_json::to_value(PermissionMode::Auto).unwrap(),
        json!("auto")
    );
}

#[test]
fn permission_update_to_value_matches_python_control_protocol() {
    let add_rules = PermissionUpdate {
        update_type: "addRules".to_string(),
        rules: Some(vec![
            PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: Some("echo *".to_string()),
            },
            PermissionRuleValue {
                tool_name: "Read".to_string(),
                rule_content: None,
            },
        ]),
        behavior: Some(PermissionBehavior::Allow),
        mode: None,
        directories: None,
        destination: Some(PermissionUpdateDestination::ProjectSettings),
    };
    assert_eq!(
        add_rules.to_value(),
        json!({
            "type": "addRules",
            "destination": "projectSettings",
            "rules": [
                {"toolName": "Bash", "ruleContent": "echo *"},
                {"toolName": "Read", "ruleContent": null}
            ],
            "behavior": "allow"
        })
    );

    let set_mode = PermissionUpdate {
        update_type: "setMode".to_string(),
        rules: None,
        behavior: None,
        mode: Some(PermissionMode::BypassPermissions),
        directories: None,
        destination: Some(PermissionUpdateDestination::Session),
    };
    assert_eq!(
        set_mode.to_value(),
        json!({
            "type": "setMode",
            "destination": "session",
            "mode": "bypassPermissions"
        })
    );

    let add_directories = PermissionUpdate {
        update_type: "addDirectories".to_string(),
        rules: None,
        behavior: None,
        mode: None,
        directories: Some(vec!["/repo".to_string(), "/tmp/work".to_string()]),
        destination: None,
    };
    assert_eq!(
        add_directories.to_value(),
        json!({"type": "addDirectories", "directories": ["/repo", "/tmp/work"]})
    );
}

#[test]
fn rate_limit_info_uses_rust_string_fields_while_accepting_python_runtime_values() {
    let info = RateLimitInfo {
        status: "allowed_warning".to_string(),
        resets_at: Some(1_700_000_000),
        rate_limit_type: Some("seven_day_sonnet".to_string()),
        utilization: Some(0.9),
        overage_status: None,
        overage_resets_at: None,
        overage_disabled_reason: None,
        raw: json!({"source": "cli"}),
    };
    assert_eq!(info.status, "allowed_warning");
    assert_eq!(info.rate_limit_type.as_deref(), Some("seven_day_sonnet"));
}

#[test]
fn message_content_types_are_constructible() {
    let user = UserMessage {
        content: UserMessageContent::Text("Hello, Claude!".to_string()),
        uuid: None,
        parent_tool_use_id: None,
        tool_use_result: None,
    };
    match user.content {
        UserMessageContent::Text(text) => assert_eq!(text, "Hello, Claude!"),
        UserMessageContent::Blocks(_) => panic!("expected text content"),
    }

    let assistant = AssistantMessage {
        content: vec![
            ContentBlock::Text {
                text: "Hello, human!".to_string(),
            },
            ContentBlock::Thinking {
                thinking: "I'm thinking...".to_string(),
                signature: "sig-123".to_string(),
            },
            ContentBlock::ToolUse {
                id: "tool-123".to_string(),
                name: "Read".to_string(),
                input: json!({"file_path": "/test.txt"}),
            },
            ContentBlock::ToolResult {
                tool_use_id: "tool-123".to_string(),
                content: Some(json!("File contents here")),
                is_error: Some(false),
            },
        ],
        model: "claude-opus-4-1-20250805".to_string(),
        parent_tool_use_id: None,
        error: None,
        usage: None,
        message_id: None,
        stop_reason: None,
        session_id: None,
        uuid: None,
    };
    assert_eq!(assistant.content.len(), 4);
    match &assistant.content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "Hello, human!"),
        _ => panic!("expected text block"),
    }
    match &assistant.content[1] {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(thinking, "I'm thinking...");
            assert_eq!(signature, "sig-123");
        }
        _ => panic!("expected thinking block"),
    }
}

#[test]
fn python_named_content_block_types_convert_to_content_block_enum() {
    let text = TextBlock {
        text: "Hello, human!".to_string(),
    };
    let content: ContentBlock = text.clone().into();
    assert!(matches!(content, ContentBlock::Text { text } if text == "Hello, human!"));

    let thinking = ThinkingBlock {
        thinking: "I'm thinking...".to_string(),
        signature: "sig-123".to_string(),
    };
    let content: ContentBlock = thinking.into();
    assert!(matches!(
        content,
        ContentBlock::Thinking { thinking, signature }
            if thinking == "I'm thinking..." && signature == "sig-123"
    ));

    let tool_use = ToolUseBlock {
        id: "tool-123".to_string(),
        name: "Read".to_string(),
        input: json!({"file_path": "/test.txt"}),
    };
    let content: ContentBlock = tool_use.into();
    assert!(matches!(
        content,
        ContentBlock::ToolUse { id, name, input }
            if id == "tool-123" && name == "Read" && input["file_path"] == "/test.txt"
    ));

    let tool_result = ToolResultBlock {
        tool_use_id: "tool-123".to_string(),
        content: Some(json!("File contents here")),
        is_error: Some(false),
    };
    let content: ContentBlock = tool_result.into();
    assert!(matches!(
        content,
        ContentBlock::ToolResult { tool_use_id, content: Some(content), is_error: Some(false) }
            if tool_use_id == "tool-123" && content == "File contents here"
    ));

    let server_use = ServerToolUseBlock {
        id: "srv-1".to_string(),
        name: "web_search".to_string(),
        input: json!({"query": "rust"}),
    };
    let content: ContentBlock = server_use.into();
    assert!(matches!(
        content,
        ContentBlock::ServerToolUse { id, name, input }
            if id == "srv-1" && name == "web_search" && input["query"] == "rust"
    ));

    let server_result = ServerToolResultBlock {
        tool_use_id: "srv-1".to_string(),
        content: json!({"type": "web_search_result"}),
    };
    let content: ContentBlock = server_result.into();
    assert!(matches!(
        &content,
        ContentBlock::ServerToolResult { tool_use_id, content }
            if tool_use_id == "srv-1" && content["type"] == "web_search_result"
    ));
    assert_eq!(
        serde_json::to_value(content).unwrap()["type"],
        "advisor_tool_result"
    );
}

#[test]
fn result_message_is_constructible() {
    let msg = ResultMessage {
        subtype: "success".to_string(),
        duration_ms: 1500,
        duration_api_ms: 1200,
        is_error: false,
        num_turns: 1,
        session_id: "session-123".to_string(),
        stop_reason: None,
        total_cost_usd: Some(0.01),
        usage: None,
        result: None,
        structured_output: None,
        model_usage: None,
        permission_denials: None,
        deferred_tool_use: None,
        errors: None,
        uuid: None,
    };
    assert_eq!(msg.subtype, "success");
    assert_eq!(msg.total_cost_usd, Some(0.01));
    assert_eq!(msg.session_id, "session-123");
}

#[test]
fn agent_definition_serializes_with_cli_keys_and_omits_none() {
    let mut agent_def = agent();
    let payload = serde_json::to_value(&agent_def).unwrap();
    assert_eq!(payload, json!({"description": "test", "prompt": "p"}));

    agent_def.skills = Some(vec!["skill-a".to_string(), "skill-b".to_string()]);
    agent_def.memory = Some("project".to_string());
    agent_def.mcp_servers = Some(vec![
        AgentMcpServer::name("slack"),
        AgentMcpServer::inline(
            "local",
            McpServerConfig::Stdio {
                command: "python".to_string(),
                args: vec!["server.py".to_string()],
                env: Default::default(),
            },
        ),
    ]);
    agent_def.disallowed_tools = Some(vec!["Bash".to_string(), "Write".to_string()]);
    agent_def.max_turns = Some(10);
    agent_def.initial_prompt = Some("/review-pr 123".to_string());
    agent_def.model = Some("claude-opus-4-5".to_string());
    agent_def.background = Some(true);
    agent_def.effort = Some(AgentEffort::level(EffortLevel::Xhigh));
    agent_def.permission_mode = Some(PermissionMode::BypassPermissions);

    let payload = serde_json::to_value(agent_def).unwrap();
    assert_eq!(payload["skills"], json!(["skill-a", "skill-b"]));
    assert_eq!(payload["memory"], json!("project"));
    assert!(payload.get("mcpServers").is_some());
    assert!(payload.get("mcp_servers").is_none());
    assert_eq!(payload["disallowedTools"], json!(["Bash", "Write"]));
    assert!(payload.get("disallowed_tools").is_none());
    assert_eq!(payload["maxTurns"], json!(10));
    assert_eq!(payload["initialPrompt"], json!("/review-pr 123"));
    assert_eq!(payload["model"], json!("claude-opus-4-5"));
    assert_eq!(payload["background"], json!(true));
    assert_eq!(payload["effort"], json!("xhigh"));
    assert_eq!(payload["permissionMode"], json!("bypassPermissions"));

    let mut int_effort_agent = agent();
    int_effort_agent.effort = Some(AgentEffort::tokens(32000));
    let payload = serde_json::to_value(int_effort_agent).unwrap();
    assert_eq!(payload["effort"], json!(32000));
}

#[test]
fn mcp_status_types_deserialize_python_typed_dict_shape() {
    let response: McpStatusResponse = serde_json::from_value(json!({
        "mcpServers": [
            {
                "name": "my-server",
                "status": "connected",
                "serverInfo": {"name": "my-server", "version": "1.2.3"},
                "config": {"type": "http", "url": "https://example.com"},
                "scope": "project",
                "tools": [
                    {
                        "name": "greet",
                        "description": "Greet a user",
                        "annotations": {
                            "readOnly": true,
                            "destructive": false,
                            "openWorld": false
                        }
                    }
                ]
            },
            {"name": "pending-server", "status": "pending"},
            {
                "name": "proxy-server",
                "status": "needs-auth",
                "config": {
                    "type": "claudeai-proxy",
                    "url": "https://claude.ai/proxy",
                    "id": "proxy-abc"
                }
            }
        ]
    }))
    .unwrap();

    assert_eq!(response.mcp_servers.len(), 3);
    assert_eq!(
        response.mcp_servers[0].status,
        McpServerConnectionStatus::Connected
    );
    assert_eq!(
        response.mcp_servers[0]
            .server_info
            .as_ref()
            .unwrap()
            .version,
        "1.2.3"
    );
    assert_eq!(
        response.mcp_servers[0].tools.as_ref().unwrap()[0]
            .annotations
            .as_ref()
            .unwrap()
            .read_only,
        Some(true)
    );
    assert_eq!(
        response.mcp_servers[1].status,
        McpServerConnectionStatus::Pending
    );
    assert_eq!(
        response.mcp_servers[2].config.as_ref().unwrap()["id"],
        "proxy-abc"
    );

    let minimal = McpServerStatus {
        name: "failed-server".to_string(),
        status: McpServerConnectionStatus::Failed,
        server_info: None,
        error: Some("Connection refused".to_string()),
        config: None,
        scope: None,
        tools: None,
        extra: Default::default(),
    };
    let payload = serde_json::to_value(minimal).unwrap();
    assert_eq!(payload["status"], "failed");
    assert_eq!(payload["error"], "Connection refused");
    assert!(payload.get("serverInfo").is_none());
}

#[test]
fn context_usage_response_deserializes_cli_shape() {
    let usage: ContextUsageResponse = serde_json::from_value(json!({
        "categories": [
            {"name": "System prompt", "tokens": 3200, "color": "#abc"},
            {"name": "Deferred", "tokens": 10, "color": "#def", "isDeferred": true}
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
        "skills": {"frontmatterTokens": 3}
    }))
    .unwrap();

    assert_eq!(usage.total_tokens, 98200);
    assert_eq!(usage.raw_max_tokens, 200000);
    assert_eq!(usage.categories[1].is_deferred, Some(true));
    assert_eq!(usage.mcp_tools[0]["serverName"], "ref");
    assert_eq!(usage.skills.as_ref().unwrap()["frontmatterTokens"], 3);
    assert!(usage.api_usage.is_none());
}

#[test]
fn hook_input_types_match_python_typed_dict_shapes() {
    let base = BaseHookInput {
        session_id: "sess-1".to_string(),
        transcript_path: "/tmp/transcript".to_string(),
        cwd: "/home/user".to_string(),
        permission_mode: None,
    };
    let notification = NotificationHookInput {
        base: base.clone(),
        hook_event_name: "Notification".to_string(),
        message: "Task completed".to_string(),
        title: Some("Success".to_string()),
        notification_type: "info".to_string(),
    };
    let payload = serde_json::to_value(&notification).unwrap();
    assert_eq!(payload["hook_event_name"], "Notification");
    assert_eq!(payload["message"], "Task completed");
    assert_eq!(payload["title"], "Success");

    let subagent = SubagentStartHookInput {
        base: base.clone(),
        hook_event_name: "SubagentStart".to_string(),
        agent_id: "agent-42".to_string(),
        agent_type: "researcher".to_string(),
    };
    assert_eq!(
        serde_json::to_value(subagent).unwrap()["agent_id"],
        "agent-42"
    );

    let pre_tool = PreToolUseHookInput {
        base: base.clone(),
        hook_event_name: "PreToolUse".to_string(),
        tool_name: "Bash".to_string(),
        tool_input: ToolInput::new(json!({"command": "echo hello"})).unwrap(),
        tool_use_id: "toolu_abc123".to_string(),
        agent_id: Some("agent-42".to_string()),
        agent_type: Some("researcher".to_string()),
    };
    let payload = serde_json::to_value(pre_tool).unwrap();
    assert_eq!(payload["agent_id"], "agent-42");
    assert_eq!(payload["tool_input"]["command"], "echo hello");

    let permission_request = PermissionRequestHookInput {
        base,
        hook_event_name: "PermissionRequest".to_string(),
        tool_name: "Bash".to_string(),
        tool_input: ToolInput::new(json!({"command": "ls"})).unwrap(),
        permission_suggestions: Some(vec![json!({"type": "allow", "rule": "Bash(*)"})]),
        agent_id: None,
        agent_type: None,
    };
    let payload = serde_json::to_value(permission_request).unwrap();
    assert_eq!(payload["hook_event_name"], "PermissionRequest");
    assert_eq!(payload["permission_suggestions"][0]["rule"], "Bash(*)");
}

#[test]
fn hook_output_types_serialize_cli_field_names() {
    let notification = NotificationHookSpecificOutput {
        hook_event_name: "Notification".to_string(),
        additional_context: Some("Extra info".to_string()),
    };
    let payload = serde_json::to_value(notification).unwrap();
    assert_eq!(payload["hookEventName"], "Notification");
    assert_eq!(payload["additionalContext"], "Extra info");

    let subagent = SubagentStartHookSpecificOutput {
        hook_event_name: "SubagentStart".to_string(),
        additional_context: Some("Starting subagent for research".to_string()),
    };
    assert_eq!(
        serde_json::to_value(subagent).unwrap()["hookEventName"],
        "SubagentStart"
    );

    let permission = PermissionRequestHookSpecificOutput {
        hook_event_name: "PermissionRequest".to_string(),
        decision: json!({"type": "allow"}),
    };
    let payload = serde_json::to_value(permission).unwrap();
    assert_eq!(payload["decision"], json!({"type": "allow"}));

    let pre_tool = PreToolUseHookSpecificOutput {
        hook_event_name: "PreToolUse".to_string(),
        permission_decision: Some("ask".to_string()),
        permission_decision_reason: Some("needs review".to_string()),
        updated_input: Some(ToolInput::new(json!({"command": "pwd"})).unwrap()),
        additional_context: Some("context for claude".to_string()),
    };
    let payload = serde_json::to_value(pre_tool).unwrap();
    assert_eq!(payload["permissionDecision"], "ask");
    assert_eq!(payload["permissionDecisionReason"], "needs review");
    assert_eq!(payload["updatedInput"]["command"], "pwd");

    let post_tool = PostToolUseHookSpecificOutput {
        hook_event_name: "PostToolUse".to_string(),
        additional_context: None,
        updated_tool_output: Some(
            json!({"stdout": "replaced", "stderr": "", "interrupted": false}),
        ),
        updated_mcp_tool_output: Some(json!({"result": "modified"})),
    };
    let sync_output = HookJsonOutput::Sync(SyncHookJsonOutput {
        continue_: Some(false),
        suppress_output: Some(true),
        stop_reason: Some("Testing field conversion".to_string()),
        decision: Some("block".to_string()),
        system_message: None,
        reason: None,
        hook_specific_output: Some(HookSpecificOutput::PostToolUse(post_tool)),
    });
    let payload = serde_json::to_value(sync_output).unwrap();
    assert_eq!(payload["continue"], false);
    assert_eq!(payload["suppressOutput"], true);
    assert_eq!(
        payload["hookSpecificOutput"]["updatedMCPToolOutput"]["result"],
        "modified"
    );
}
