use claude_agent_sdk::{
    ClaudeSdkError, ContentBlock, Message, MessageParseError, UserMessageContent, parse_message,
};
use serde_json::{Value, json};

fn parse_required(data: Value) -> Message {
    parse_message(data)
        .unwrap()
        .expect("expected parsed message")
}

fn expect_parse_error(data: Value, contains: &str) -> MessageParseError {
    let err = parse_message(data).unwrap_err();
    let ClaudeSdkError::MessageParse(err) = err else {
        panic!("expected message parse error");
    };
    assert!(
        err.to_string().contains(contains),
        "expected {:?} to contain {:?}",
        err.to_string(),
        contains
    );
    err
}

fn assert_text(block: &ContentBlock, expected: &str) {
    let ContentBlock::Text { text } = block else {
        panic!("expected text block, got {block:?}");
    };
    assert_eq!(text, expected);
}

fn user_message_with_tool_result(content: &str, is_error: bool) -> Value {
    json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_01ABC",
                "content": content,
                "is_error": is_error,
            }],
        },
        "parent_tool_use_id": Value::Null,
        "tool_use_result": Value::Null,
        "uuid": "test-uuid-1234",
    })
}

fn tool_result_block(message: Message) -> ContentBlock {
    let Message::User(message) = message else {
        panic!("expected user message");
    };
    let UserMessageContent::Blocks(blocks) = message.content else {
        panic!("expected content blocks");
    };
    blocks
        .into_iter()
        .find(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .expect("expected tool result block")
}

#[test]
fn parse_valid_user_message() {
    let Message::User(message) = parse_required(json!({
        "type": "user",
        "message": {"content": [{"type": "text", "text": "Hello"}]},
    })) else {
        panic!("expected user message");
    };
    let UserMessageContent::Blocks(blocks) = message.content else {
        panic!("expected content blocks");
    };
    assert_eq!(blocks.len(), 1);
    assert_text(&blocks[0], "Hello");
}

#[test]
fn parse_user_message_with_uuid_parent_and_tool_use_result() {
    let tool_result = json!({
        "filePath": "/path/to/file.py",
        "oldString": "old code",
        "newString": "new code",
        "structuredPatch": [{"oldStart": 33}],
        "userModified": false,
        "replaceAll": false,
    });
    let Message::User(message) = parse_required(json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "toolu", "content": "updated"}],
        },
        "parent_tool_use_id": "parent_tool",
        "session_id": "84afb479-17ae-49af-8f2b-666ac2530c3a",
        "uuid": "2ace3375-1879-48a0-a421-6bce25a9295a",
        "tool_use_result": tool_result,
    })) else {
        panic!("expected user message");
    };
    assert_eq!(
        message.uuid.as_deref(),
        Some("2ace3375-1879-48a0-a421-6bce25a9295a")
    );
    assert_eq!(message.parent_tool_use_id.as_deref(), Some("parent_tool"));
    assert_eq!(
        message.tool_use_result.as_ref().unwrap()["filePath"],
        "/path/to/file.py"
    );
    let UserMessageContent::Blocks(blocks) = message.content else {
        panic!("expected content blocks");
    };
    assert!(matches!(
        &blocks[0],
        ContentBlock::ToolResult {
            tool_use_id,
            content: Some(content),
            is_error: None,
        } if tool_use_id == "toolu" && content == "updated"
    ));
}

#[test]
fn parse_user_message_with_string_content_and_tool_use_result() {
    let Message::User(message) = parse_required(json!({
        "type": "user",
        "message": {"content": "Simple string content"},
        "tool_use_result": {"filePath": "/path/to/file.py", "userModified": true},
    })) else {
        panic!("expected user message");
    };
    let UserMessageContent::Text(content) = message.content else {
        panic!("expected string content");
    };
    assert_eq!(content, "Simple string content");
    assert_eq!(message.tool_use_result.unwrap()["userModified"], true);
}

#[test]
fn parse_user_message_with_tool_use_tool_result_and_mixed_content() {
    let Message::User(message) = parse_required(json!({
        "type": "user",
        "message": {
            "content": [
                {"type": "text", "text": "Here's what I found:"},
                {"type": "tool_use", "id": "use_1", "name": "Search", "input": {"query": "test"}},
                {"type": "tool_result", "tool_use_id": "use_1", "content": "Search results", "is_error": true},
                {"type": "text", "text": "What do you think?"},
            ]
        },
    })) else {
        panic!("expected user message");
    };
    let UserMessageContent::Blocks(blocks) = message.content else {
        panic!("expected content blocks");
    };
    assert_eq!(blocks.len(), 4);
    assert_text(&blocks[0], "Here's what I found:");
    assert!(matches!(
        &blocks[1],
        ContentBlock::ToolUse { id, name, input }
            if id == "use_1" && name == "Search" && input["query"] == "test"
    ));
    assert!(matches!(
        &blocks[2],
        ContentBlock::ToolResult {
            tool_use_id,
            content: Some(content),
            is_error: Some(true),
        } if tool_use_id == "use_1" && content == "Search results"
    ));
    assert_text(&blocks[3], "What do you think?");
}

#[test]
fn parse_tool_result_preserves_inline_and_persisted_output_content() {
    const LAYER2_THRESHOLD_CHARS: usize = 50_000;
    let inline_content = "x".repeat(1000);
    let persisted_content = format!(
        "<persisted-output>\n\
         Output too large (73.0KB). Full output saved to: /tmp/.claude/tool-results/abc123.txt\n\n\
         Preview (first 2KB):\n{}\
         \n...\n</persisted-output>",
        "x".repeat(2000)
    );

    let ContentBlock::ToolResult {
        content: Some(content),
        is_error,
        ..
    } = tool_result_block(parse_required(user_message_with_tool_result(
        &inline_content,
        false,
    )))
    else {
        panic!("expected tool result");
    };
    assert_eq!(content.as_str().unwrap(), inline_content);
    assert_eq!(is_error, Some(false));
    assert!(!content.as_str().unwrap().starts_with("<persisted-output>"));

    let ContentBlock::ToolResult {
        content: Some(content),
        is_error,
        ..
    } = tool_result_block(parse_required(user_message_with_tool_result(
        &persisted_content,
        true,
    )))
    else {
        panic!("expected tool result");
    };
    let content = content.as_str().unwrap();
    assert!(content.starts_with("<persisted-output>"));
    assert!(content.len() < LAYER2_THRESHOLD_CHARS);
    assert_eq!(is_error, Some(true));
}

#[test]
fn parse_valid_assistant_message() {
    let Message::Assistant(message) = parse_required(json!({
        "type": "assistant",
        "message": {
            "content": [
                {"type": "text", "text": "Hello"},
                {"type": "tool_use", "id": "tool_123", "name": "Read", "input": {"file_path": "/test.txt"}},
            ],
            "model": "claude-opus-4-1-20250805",
        },
    })) else {
        panic!("expected assistant message");
    };
    assert_eq!(message.content.len(), 2);
    assert_text(&message.content[0], "Hello");
    assert!(matches!(&message.content[1], ContentBlock::ToolUse { name, .. } if name == "Read"));
}

#[test]
fn parse_assistant_message_with_thinking_and_usage() {
    let Message::Assistant(message) = parse_required(json!({
        "type": "assistant",
        "message": {
            "content": [
                {"type": "thinking", "thinking": "I'm thinking about the answer...", "signature": "sig-123"},
                {"type": "text", "text": "Here's my response"},
            ],
            "model": "claude-opus-4-5",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 50,
                "cache_read_input_tokens": 2000,
                "cache_creation_input_tokens": 500,
            },
        },
    })) else {
        panic!("expected assistant message");
    };
    assert!(matches!(
        &message.content[0],
        ContentBlock::Thinking { thinking, signature }
            if thinking == "I'm thinking about the answer..." && signature == "sig-123"
    ));
    assert_text(&message.content[1], "Here's my response");
    assert_eq!(message.usage.unwrap()["cache_read_input_tokens"], 2000);
}

#[test]
fn parse_assistant_message_with_server_tool_use_and_result() {
    let Message::Assistant(message) = parse_required(json!({
        "type": "assistant",
        "message": {
            "content": [
                {"type": "server_tool_use", "id": "srvtoolu_01ABC", "name": "advisor", "input": {}},
                {
                    "type": "advisor_tool_result",
                    "tool_use_id": "srvtoolu_01ABC",
                    "content": {"type": "advisor_result", "text": "Consider edge cases around empty input."},
                },
                {
                    "type": "advisor_tool_result",
                    "tool_use_id": "srvtoolu_01DEF",
                    "content": {"type": "advisor_redacted_result", "encrypted_content": "EuYDCioIDhgC..."},
                },
            ],
            "model": "claude-sonnet-4-5",
        },
    })) else {
        panic!("expected assistant message");
    };
    assert!(matches!(
        &message.content[0],
        ContentBlock::ServerToolUse { id, name, input }
            if id == "srvtoolu_01ABC" && name == "advisor" && input == &json!({})
    ));
    assert!(matches!(
        &message.content[1],
        ContentBlock::ServerToolResult { tool_use_id, content }
            if tool_use_id == "srvtoolu_01ABC" && content["type"] == "advisor_result"
    ));
    assert!(matches!(
        &message.content[2],
        ContentBlock::ServerToolResult { content, .. }
            if content["type"] == "advisor_redacted_result"
                && content["encrypted_content"] == "EuYDCioIDhgC..."
    ));
}

#[test]
fn parse_assistant_message_with_error_and_optional_fields() {
    let Message::Assistant(message) = parse_required(json!({
        "type": "assistant",
        "message": {
            "content": [{"type": "text", "text": "Invalid API key"}],
            "model": "<synthetic>",
            "id": "msg_01HRq7YZE3apPqSHydvG77Ve",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5},
        },
        "session_id": "fdf2d90a-fd9e-4736-ae35-806edd13643f",
        "uuid": "0dbd2453-1209-4fe9-bd51-4102f64e33df",
        "error": "authentication_failed",
    })) else {
        panic!("expected assistant message");
    };
    assert_eq!(message.error.as_deref(), Some("authentication_failed"));
    assert_eq!(
        message.message_id.as_deref(),
        Some("msg_01HRq7YZE3apPqSHydvG77Ve")
    );
    assert_eq!(message.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(
        message.session_id.as_deref(),
        Some("fdf2d90a-fd9e-4736-ae35-806edd13643f")
    );
    assert_eq!(
        message.uuid.as_deref(),
        Some("0dbd2453-1209-4fe9-bd51-4102f64e33df")
    );
}

#[test]
fn parse_assistant_message_optional_fields_absent() {
    let Message::Assistant(message) = parse_required(json!({
        "type": "assistant",
        "message": {
            "content": [{"type": "text", "text": "hi"}],
            "model": "claude-opus-4-5",
        },
    })) else {
        panic!("expected assistant message");
    };
    assert!(message.error.is_none());
    assert!(message.usage.is_none());
    assert!(message.message_id.is_none());
    assert!(message.stop_reason.is_none());
    assert!(message.session_id.is_none());
    assert!(message.uuid.is_none());
}

#[test]
fn parse_valid_system_and_task_messages() {
    let Message::System(system) = parse_required(json!({"type": "system", "subtype": "start"}))
    else {
        panic!("expected system message");
    };
    assert_eq!(system.subtype, "start");

    let started_data = json!({
        "type": "system",
        "subtype": "task_started",
        "task_id": "task-abc",
        "tool_use_id": "toolu_01",
        "description": "Reticulating splines",
        "task_type": "background",
        "uuid": "uuid-1",
        "session_id": "session-1",
    });
    let Message::TaskStarted(started) = parse_required(started_data.clone()) else {
        panic!("expected task_started message");
    };
    assert_eq!(started.task_id, "task-abc");
    assert_eq!(started.task_type.as_deref(), Some("background"));
    assert_eq!(started.subtype, "task_started");
    assert_eq!(started.data, started_data);

    let Message::TaskStarted(optional) = parse_required(json!({
        "type": "system",
        "subtype": "task_started",
        "task_id": "task-abc",
        "description": "Working",
        "uuid": "uuid-1",
        "session_id": "session-1",
    })) else {
        panic!("expected task_started message");
    };
    assert!(optional.tool_use_id.is_none());
    assert!(optional.task_type.is_none());
}

#[test]
fn parse_task_progress_and_notification_messages() {
    let Message::TaskProgress(progress) = parse_required(json!({
        "type": "system",
        "subtype": "task_progress",
        "task_id": "task-abc",
        "tool_use_id": "toolu_01",
        "description": "Halfway there",
        "usage": {"total_tokens": 1234, "tool_uses": 5, "duration_ms": 9876},
        "last_tool_name": "Read",
        "uuid": "uuid-2",
        "session_id": "session-1",
    })) else {
        panic!("expected task_progress message");
    };
    assert_eq!(progress.usage.total_tokens, 1234);
    assert_eq!(progress.usage.tool_uses, 5);
    assert_eq!(progress.last_tool_name.as_deref(), Some("Read"));

    let Message::TaskNotification(notification) = parse_required(json!({
        "type": "system",
        "subtype": "task_notification",
        "task_id": "task-abc",
        "tool_use_id": "toolu_01",
        "status": "completed",
        "output_file": "/tmp/out.md",
        "summary": "All done",
        "usage": {"total_tokens": 2000, "tool_uses": 7, "duration_ms": 12345},
        "uuid": "uuid-3",
        "session_id": "session-1",
    })) else {
        panic!("expected task_notification message");
    };
    assert_eq!(notification.status, "completed");
    assert_eq!(notification.output_file, "/tmp/out.md");
    assert_eq!(notification.usage.unwrap().tool_uses, 7);

    let Message::TaskNotification(optional) = parse_required(json!({
        "type": "system",
        "subtype": "task_notification",
        "task_id": "task-abc",
        "status": "failed",
        "output_file": "/tmp/out.md",
        "summary": "Boom",
        "uuid": "uuid-3",
        "session_id": "session-1",
    })) else {
        panic!("expected task_notification message");
    };
    assert!(optional.usage.is_none());
    assert!(optional.tool_use_id.is_none());
}

#[test]
fn unknown_system_subtype_yields_generic_system_message() {
    let data = json!({"type": "system", "subtype": "some_future_subtype", "foo": "bar"});
    let Message::System(message) = parse_required(data.clone()) else {
        panic!("expected generic system message");
    };
    assert_eq!(message.subtype, "some_future_subtype");
    assert_eq!(message.data, data);
}

#[test]
fn parse_assistant_message_inside_subagent() {
    let Message::Assistant(message) = parse_required(json!({
        "type": "assistant",
        "message": {
            "content": [{"type": "text", "text": "Hello"}],
            "model": "claude-opus-4-1-20250805",
        },
        "parent_tool_use_id": "toolu_01Xrwd5Y13sEHtzScxR77So8",
    })) else {
        panic!("expected assistant message");
    };
    assert_eq!(
        message.parent_tool_use_id.as_deref(),
        Some("toolu_01Xrwd5Y13sEHtzScxR77So8")
    );
}

#[test]
fn parse_valid_result_message_and_stop_reason_variants() {
    let Message::Result(message) = parse_required(json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1000,
        "duration_api_ms": 500,
        "is_error": false,
        "num_turns": 2,
        "session_id": "session_123",
    })) else {
        panic!("expected result message");
    };
    assert_eq!(message.subtype, "success");
    assert!(message.stop_reason.is_none());

    let Message::Result(with_stop) = parse_required(json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1000,
        "duration_api_ms": 500,
        "is_error": false,
        "num_turns": 2,
        "session_id": "session_123",
        "stop_reason": "end_turn",
        "result": "Done",
    })) else {
        panic!("expected result message");
    };
    assert_eq!(with_stop.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(with_stop.result.as_deref(), Some("Done"));

    let Message::Result(null_stop) = parse_required(json!({
        "type": "result",
        "subtype": "error_max_turns",
        "duration_ms": 1000,
        "duration_api_ms": 500,
        "is_error": true,
        "num_turns": 10,
        "session_id": "session_123",
        "stop_reason": null,
    })) else {
        panic!("expected result message");
    };
    assert!(null_stop.stop_reason.is_none());
}

#[test]
fn parse_result_message_with_model_usage_deferred_tool_use_and_errors() {
    let Message::Result(message) = parse_required(json!({
        "type": "result",
        "subtype": "error_during_execution",
        "duration_ms": 3000,
        "duration_api_ms": 2000,
        "is_error": true,
        "num_turns": 1,
        "session_id": "fdf2d90a-fd9e-4736-ae35-806edd13643f",
        "stop_reason": "end_turn",
        "total_cost_usd": 0.0106,
        "usage": {"input_tokens": 3, "output_tokens": 24},
        "result": "Hello",
        "modelUsage": {
            "claude-sonnet-4-5-20250929": {
                "costUSD": 0.0106,
                "contextWindow": 200000
            }
        },
        "permission_denials": [],
        "deferred_tool_use": {
            "id": "toolu_01abc",
            "name": "Bash",
            "input": {"command": "rm -rf /tmp/scratch"}
        },
        "errors": ["Tool execution failed: permission denied", "Unable to write to /etc/hosts"],
        "uuid": "d379c496-f33a-4ea4-b920-3c5483baa6f7",
    })) else {
        panic!("expected result message");
    };
    assert_eq!(
        message.model_usage.unwrap()["claude-sonnet-4-5-20250929"]["costUSD"],
        0.0106
    );
    assert_eq!(message.permission_denials.unwrap(), json!([]));
    let deferred = message.deferred_tool_use.unwrap();
    assert_eq!(deferred.id, "toolu_01abc");
    assert_eq!(deferred.name, "Bash");
    assert_eq!(deferred.input["command"], "rm -rf /tmp/scratch");
    assert_eq!(
        message.errors.unwrap(),
        vec![
            "Tool execution failed: permission denied".to_string(),
            "Unable to write to /etc/hosts".to_string()
        ]
    );
    assert_eq!(
        message.uuid.as_deref(),
        Some("d379c496-f33a-4ea4-b920-3c5483baa6f7")
    );
}

#[test]
fn parse_result_message_optional_fields_absent() {
    let Message::Result(message) = parse_required(json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1000,
        "duration_api_ms": 500,
        "is_error": false,
        "num_turns": 1,
        "session_id": "session_123",
    })) else {
        panic!("expected result message");
    };
    assert!(message.model_usage.is_none());
    assert!(message.permission_denials.is_none());
    assert!(message.deferred_tool_use.is_none());
    assert!(message.errors.is_none());
    assert!(message.uuid.is_none());

    let Message::Result(message) = parse_required(json!({
        "type": "result",
        "subtype": "success",
        "duration_ms": 1000,
        "duration_api_ms": 500,
        "is_error": false,
        "num_turns": 1,
        "session_id": "session_123",
        "deferred_tool_use": {},
    })) else {
        panic!("expected result message");
    };
    assert!(message.deferred_tool_use.is_none());
}

#[test]
fn parse_result_message_requires_complete_deferred_tool_use_like_python() {
    expect_parse_error(
        json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1000,
            "duration_api_ms": 500,
            "is_error": false,
            "num_turns": 1,
            "session_id": "session_123",
            "deferred_tool_use": {"id": "toolu"},
        }),
        "deferred_tool_use.name",
    );

    expect_parse_error(
        json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1000,
            "duration_api_ms": 500,
            "is_error": false,
            "num_turns": 1,
            "session_id": "session_123",
            "deferred_tool_use": {"id": "toolu", "name": "Bash"},
        }),
        "deferred_tool_use.input",
    );
}

#[test]
fn parse_rate_limit_event() {
    let Message::RateLimitEvent(message) = parse_required(json!({
        "type": "rate_limit_event",
        "rate_limit_info": {
            "status": "allowed_warning",
            "resetsAt": 1700000000,
            "rateLimitType": "five_hour",
            "utilization": 0.91,
            "isUsingOverage": false,
        },
        "uuid": "abc-123",
        "session_id": "session_xyz",
    })) else {
        panic!("expected rate limit event");
    };
    assert_eq!(message.uuid, "abc-123");
    assert_eq!(message.session_id, "session_xyz");
    assert_eq!(message.rate_limit_info.status, "allowed_warning");
    assert_eq!(message.rate_limit_info.resets_at, Some(1700000000));
    assert_eq!(
        message.rate_limit_info.rate_limit_type.as_deref(),
        Some("five_hour")
    );
    assert_eq!(message.rate_limit_info.utilization, Some(0.91));
    assert_eq!(message.rate_limit_info.raw["isUsingOverage"], false);
}

#[test]
fn parse_rate_limit_event_rejected_and_minimal_variants() {
    let Message::RateLimitEvent(message) = parse_required(json!({
        "type": "rate_limit_event",
        "rate_limit_info": {
            "status": "rejected",
            "resetsAt": 1700003600,
            "rateLimitType": "seven_day",
            "isUsingOverage": false,
            "overageStatus": "rejected",
            "overageDisabledReason": "out_of_credits",
        },
        "uuid": "660e8400-e29b-41d4-a716-446655440001",
        "session_id": "test-session-id",
    })) else {
        panic!("expected rate limit event");
    };
    assert_eq!(message.rate_limit_info.status, "rejected");
    assert_eq!(
        message.rate_limit_info.overage_status.as_deref(),
        Some("rejected")
    );
    assert_eq!(
        message.rate_limit_info.overage_disabled_reason.as_deref(),
        Some("out_of_credits")
    );

    let Message::RateLimitEvent(message) = parse_required(json!({
        "type": "rate_limit_event",
        "rate_limit_info": {"status": "allowed"},
        "uuid": "770e8400-e29b-41d4-a716-446655440002",
        "session_id": "test-session-id",
    })) else {
        panic!("expected rate limit event");
    };
    assert_eq!(message.rate_limit_info.status, "allowed");
    assert!(message.rate_limit_info.resets_at.is_none());
    assert!(message.rate_limit_info.rate_limit_type.is_none());
}

#[test]
fn parse_hook_event_messages() {
    let started_data = json!({
        "type": "system",
        "subtype": "hook_started",
        "hook_event": "PreToolUse",
        "hook_name": "PreToolUse",
        "session_id": "sess-123",
        "uuid": "uuid-456",
        "tool_name": "Bash",
        "tool_input": {"command": "ls"},
    });
    let Message::HookEvent(started) = parse_required(started_data.clone()) else {
        panic!("expected hook event message");
    };
    assert_eq!(started.subtype, "hook_started");
    assert_eq!(started.hook_event_name, "PreToolUse");
    assert_eq!(started.session_id.as_deref(), Some("sess-123"));
    assert_eq!(started.uuid.as_deref(), Some("uuid-456"));
    assert_eq!(started.data, started_data);

    let Message::HookEvent(response) = parse_required(json!({
        "type": "system",
        "subtype": "hook_response",
        "hook_event": "PostToolUse",
        "hook_name": "PostToolUse",
        "session_id": "sess-123",
        "uuid": "uuid-789",
        "output": "",
        "exit_code": 0,
        "outcome": "success",
    })) else {
        panic!("expected hook event message");
    };
    assert_eq!(response.subtype, "hook_response");
    assert_eq!(response.hook_event_name, "PostToolUse");
    assert_eq!(response.data["exit_code"], 0);

    let Message::HookEvent(minimal) = parse_required(json!({
        "type": "system",
        "subtype": "hook_started",
        "hook_name": "Stop",
    })) else {
        panic!("expected hook event message");
    };
    assert_eq!(minimal.hook_event_name, "Stop");
    assert!(minimal.session_id.is_none());
    assert!(minimal.uuid.is_none());
}

#[test]
fn invalid_data_and_missing_type_errors_match_python_semantics() {
    expect_parse_error(json!("not a dict"), "Invalid message data type");
    expect_parse_error(json!("not a dict"), "expected dict, got str");
    expect_parse_error(
        json!({"message": {"content": []}}),
        "Message missing 'type' field",
    );
    expect_parse_error(json!({"type": ""}), "Message missing 'type' field");
    assert!(
        parse_message(json!({"type": "unknown_type"}))
            .unwrap()
            .is_none()
    );
    assert!(parse_message(json!({"type": 123})).unwrap().is_none());
}

#[test]
fn missing_required_fields_report_context_and_preserve_data() {
    expect_parse_error(
        json!({"type": "user"}),
        "Missing required field in user message",
    );
    expect_parse_error(
        json!({"type": "assistant"}),
        "Missing required field in assistant message",
    );
    expect_parse_error(
        json!({"type": "system"}),
        "Missing required field in system message",
    );
    expect_parse_error(
        json!({"type": "result", "subtype": "success"}),
        "Missing required field in result message",
    );

    let data = json!({"type": "assistant"});
    let err = expect_parse_error(data.clone(), "Missing required field in assistant message");
    assert_eq!(err.data, Some(data));
}

#[test]
fn known_content_blocks_missing_required_fields_error_like_python() {
    expect_parse_error(
        json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "tool_use", "id": "toolu", "name": "Read"}]},
        }),
        "Missing required field in user message: input",
    );

    expect_parse_error(
        json!({
            "type": "assistant",
            "message": {
                "content": [{"type": "text"}],
                "model": "claude-opus-4-1-20250805",
            },
        }),
        "Missing required field in assistant message: text",
    );

    expect_parse_error(
        json!({
            "type": "assistant",
            "message": {
                "content": [{"payload": true}],
                "model": "claude-opus-4-1-20250805",
            },
        }),
        "Missing required field in assistant message: type",
    );

    let Message::Assistant(message) = parse_required(json!({
        "type": "assistant",
        "message": {
            "content": [
                {"type": 123, "payload": true},
                {"type": "future_block", "payload": true},
                {"type": "text", "text": "kept"}
            ],
            "model": "claude-opus-4-1-20250805",
        },
    })) else {
        panic!("expected assistant message");
    };
    assert_eq!(message.content.len(), 1);
    assert_text(&message.content[0], "kept");
}
