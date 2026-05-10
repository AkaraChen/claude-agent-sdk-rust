use serde::Serialize;
use serde_json::Value;

use crate::errors::{MessageParseError, Result};
use crate::types::{
    AssistantMessage, ContentBlock, DeferredToolUse, HookEventMessage, Message, MirrorErrorMessage,
    RateLimitEvent, RateLimitInfo, ResultMessage, StreamEvent, SystemMessage,
    TaskNotificationMessage, TaskProgressMessage, TaskStartedMessage, TaskUsage, UserMessage,
    UserMessageContent,
};

pub fn parse_message(data: impl Serialize) -> Result<Option<Message>> {
    let data = serde_json::to_value(data)?;
    let Value::Object(obj) = &data else {
        return Err(MessageParseError::new(
            format!(
                "Invalid message data type (expected dict, got {})",
                python_type_name(&data)
            ),
            Some(data),
        )
        .into());
    };

    if obj.get("type").and_then(Value::as_str) == Some("system")
        && matches!(
            obj.get("subtype").and_then(Value::as_str),
            Some("hook_started" | "hook_response")
        )
    {
        let subtype = required_str_in(&data, "subtype", "system message")?;
        let hook_event_name = obj
            .get("hook_event")
            .or_else(|| obj.get("hook_name"))
            .or_else(|| obj.get("hook_event_name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        return Ok(Some(Message::HookEvent(HookEventMessage {
            subtype,
            hook_event_name,
            data: data.clone(),
            session_id: opt_str(&data, "session_id"),
            uuid: opt_str(&data, "uuid"),
        })));
    }

    let Some(message_type_value) = obj.get("type") else {
        return Err(MessageParseError::new("Message missing 'type' field", Some(data)).into());
    };
    let Some(message_type) = message_type_value.as_str() else {
        if is_python_falsey_json(message_type_value) {
            return Err(MessageParseError::new("Message missing 'type' field", Some(data)).into());
        }
        tracing::debug!("Skipping unknown message type: {message_type_value}");
        return Ok(None);
    };
    if message_type.is_empty() {
        return Err(MessageParseError::new("Message missing 'type' field", Some(data)).into());
    }

    match message_type {
        "user" => parse_user(data).map(|m| Some(Message::User(m))),
        "assistant" => parse_assistant(data).map(|m| Some(Message::Assistant(m))),
        "system" => parse_system(data).map(Some),
        "result" => parse_result(data).map(|m| Some(Message::Result(m))),
        "stream_event" => parse_stream_event(data).map(|m| Some(Message::StreamEvent(m))),
        "rate_limit_event" => {
            parse_rate_limit_event(data).map(|m| Some(Message::RateLimitEvent(m)))
        }
        other => {
            tracing::debug!("Skipping unknown message type: {other}");
            Ok(None)
        }
    }
}

fn parse_user(data: Value) -> Result<UserMessage> {
    let content = data.pointer("/message/content").ok_or_else(|| {
        MessageParseError::new(
            "Missing required field in user message: message.content",
            Some(data.clone()),
        )
    })?;
    let content = match content {
        Value::String(s) => UserMessageContent::Text(s.clone()),
        Value::Array(blocks) => {
            UserMessageContent::Blocks(parse_content_blocks(blocks, "user message", &data)?)
        }
        _ => return Err(MessageParseError::new("Invalid user message content", Some(data)).into()),
    };
    Ok(UserMessage {
        content,
        uuid: opt_str(&data, "uuid"),
        parent_tool_use_id: opt_str(&data, "parent_tool_use_id"),
        tool_use_result: data.get("tool_use_result").cloned(),
    })
}

fn parse_assistant(data: Value) -> Result<AssistantMessage> {
    let blocks = data
        .pointer("/message/content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            MessageParseError::new(
                "Missing required field in assistant message: message.content",
                Some(data.clone()),
            )
        })?;
    Ok(AssistantMessage {
        content: parse_content_blocks(blocks, "assistant message", &data)?,
        model: data
            .pointer("/message/model")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                MessageParseError::new(
                    "Missing required field in assistant message: message.model",
                    Some(data.clone()),
                )
            })?
            .to_string(),
        parent_tool_use_id: opt_str(&data, "parent_tool_use_id"),
        error: opt_str(&data, "error"),
        usage: data.pointer("/message/usage").cloned(),
        message_id: data
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        stop_reason: data
            .pointer("/message/stop_reason")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        session_id: opt_str(&data, "session_id"),
        uuid: opt_str(&data, "uuid"),
    })
}

fn parse_system(data: Value) -> Result<Message> {
    let subtype = required_str_in(&data, "subtype", "system message")?;
    match subtype.as_str() {
        "task_started" => Ok(Message::TaskStarted(TaskStartedMessage {
            subtype,
            data: data.clone(),
            task_id: required_str_in(&data, "task_id", "system message")?,
            description: required_str_in(&data, "description", "system message")?,
            uuid: required_str_in(&data, "uuid", "system message")?,
            session_id: required_str_in(&data, "session_id", "system message")?,
            tool_use_id: opt_str(&data, "tool_use_id"),
            task_type: opt_str(&data, "task_type"),
        })),
        "task_progress" => Ok(Message::TaskProgress(TaskProgressMessage {
            subtype,
            data: data.clone(),
            task_id: required_str_in(&data, "task_id", "system message")?,
            description: required_str_in(&data, "description", "system message")?,
            usage: required_task_usage_in(&data, "usage", "system message")?,
            uuid: required_str_in(&data, "uuid", "system message")?,
            session_id: required_str_in(&data, "session_id", "system message")?,
            tool_use_id: opt_str(&data, "tool_use_id"),
            last_tool_name: opt_str(&data, "last_tool_name"),
        })),
        "task_notification" => Ok(Message::TaskNotification(TaskNotificationMessage {
            subtype,
            data: data.clone(),
            task_id: required_str_in(&data, "task_id", "system message")?,
            status: required_str_in(&data, "status", "system message")?,
            output_file: required_str_in(&data, "output_file", "system message")?,
            summary: required_str_in(&data, "summary", "system message")?,
            uuid: required_str_in(&data, "uuid", "system message")?,
            session_id: required_str_in(&data, "session_id", "system message")?,
            tool_use_id: opt_str(&data, "tool_use_id"),
            usage: data
                .get("usage")
                .cloned()
                .map(|usage| parse_task_usage(usage, &data))
                .transpose()?,
        })),
        "mirror_error" => Ok(Message::MirrorError(MirrorErrorMessage {
            subtype,
            data: data.clone(),
            key: data
                .get("key")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok()),
            error: opt_str(&data, "error").unwrap_or_default(),
        })),
        _ => Ok(Message::System(SystemMessage { subtype, data })),
    }
}

fn parse_result(data: Value) -> Result<ResultMessage> {
    let deferred_tool_use = match data.get("deferred_tool_use") {
        Some(Value::Object(obj)) if obj.is_empty() => None,
        Some(Value::Object(obj)) => Some(DeferredToolUse {
            id: obj
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    MessageParseError::new(
                        "Missing required field in result message: deferred_tool_use.id",
                        Some(data.clone()),
                    )
                })?
                .to_string(),
            name: obj
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    MessageParseError::new(
                        "Missing required field in result message: deferred_tool_use.name",
                        Some(data.clone()),
                    )
                })?
                .to_string(),
            input: obj.get("input").cloned().ok_or_else(|| {
                MessageParseError::new(
                    "Missing required field in result message: deferred_tool_use.input",
                    Some(data.clone()),
                )
            })?,
        }),
        _ => None,
    };

    Ok(ResultMessage {
        subtype: required_str_in(&data, "subtype", "result message")?,
        duration_ms: required_i64_in(&data, "duration_ms", "result message")?,
        duration_api_ms: required_i64_in(&data, "duration_api_ms", "result message")?,
        is_error: required_bool_in(&data, "is_error", "result message")?,
        num_turns: required_i64_in(&data, "num_turns", "result message")?,
        session_id: required_str_in(&data, "session_id", "result message")?,
        stop_reason: opt_str(&data, "stop_reason"),
        total_cost_usd: data.get("total_cost_usd").and_then(Value::as_f64),
        usage: data.get("usage").cloned(),
        result: opt_str(&data, "result"),
        structured_output: data.get("structured_output").cloned(),
        model_usage: data.get("modelUsage").cloned(),
        permission_denials: data.get("permission_denials").cloned(),
        deferred_tool_use,
        errors: data.get("errors").and_then(|v| {
            v.as_array().map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(ToString::to_string)
                    .collect()
            })
        }),
        uuid: opt_str(&data, "uuid"),
    })
}

fn parse_stream_event(data: Value) -> Result<StreamEvent> {
    Ok(StreamEvent {
        uuid: required_str_in(&data, "uuid", "stream_event message")?,
        session_id: required_str_in(&data, "session_id", "stream_event message")?,
        event: data.get("event").cloned().ok_or_else(|| {
            MessageParseError::new(
                "Missing required field in stream_event message: event",
                Some(data.clone()),
            )
        })?,
        parent_tool_use_id: opt_str(&data, "parent_tool_use_id"),
    })
}

fn parse_rate_limit_event(data: Value) -> Result<RateLimitEvent> {
    let info = data.get("rate_limit_info").cloned().ok_or_else(|| {
        MessageParseError::new(
            "Missing required field in rate_limit_event message: rate_limit_info",
            Some(data.clone()),
        )
    })?;
    let status = info
        .get("status")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            MessageParseError::new(
                "Missing required field in rate_limit_event message: rate_limit_info.status",
                Some(data.clone()),
            )
        })?
        .to_string();
    Ok(RateLimitEvent {
        rate_limit_info: RateLimitInfo {
            status,
            resets_at: info.get("resetsAt").and_then(Value::as_i64),
            rate_limit_type: info
                .get("rateLimitType")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            utilization: info.get("utilization").and_then(Value::as_f64),
            overage_status: info
                .get("overageStatus")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            overage_resets_at: info.get("overageResetsAt").and_then(Value::as_i64),
            overage_disabled_reason: info
                .get("overageDisabledReason")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            raw: info,
        },
        uuid: required_str_in(&data, "uuid", "rate_limit_event message")?,
        session_id: required_str_in(&data, "session_id", "rate_limit_event message")?,
    })
}

fn parse_content_blocks(blocks: &[Value], context: &str, raw: &Value) -> Result<Vec<ContentBlock>> {
    let mut out = Vec::new();
    for block in blocks {
        let Some(block_type_value) = block.get("type") else {
            return Err(MessageParseError::new(
                format!("Missing required field in {context}: type"),
                Some(raw.clone()),
            )
            .into());
        };
        let Some(block_type) = block_type_value.as_str() else {
            continue;
        };
        match block_type {
            "text" => out.push(ContentBlock::Text {
                text: required_block_str(block, "text", context, raw)?.to_string(),
            }),
            "thinking" => out.push(ContentBlock::Thinking {
                thinking: required_block_str(block, "thinking", context, raw)?.to_string(),
                signature: required_block_str(block, "signature", context, raw)?.to_string(),
            }),
            "tool_use" => out.push(ContentBlock::ToolUse {
                id: required_block_str(block, "id", context, raw)?.to_string(),
                name: required_block_str(block, "name", context, raw)?.to_string(),
                input: required_block_value(block, "input", context, raw)?,
            }),
            "tool_result" => out.push(ContentBlock::ToolResult {
                tool_use_id: required_block_str(block, "tool_use_id", context, raw)?.to_string(),
                content: block.get("content").cloned(),
                is_error: block.get("is_error").and_then(Value::as_bool),
            }),
            "server_tool_use" => out.push(ContentBlock::ServerToolUse {
                id: required_block_str(block, "id", context, raw)?.to_string(),
                name: required_block_str(block, "name", context, raw)?.to_string(),
                input: required_block_value(block, "input", context, raw)?,
            }),
            "advisor_tool_result" => out.push(ContentBlock::ServerToolResult {
                tool_use_id: required_block_str(block, "tool_use_id", context, raw)?.to_string(),
                content: required_block_value(block, "content", context, raw)?,
            }),
            _ => {}
        }
    }
    Ok(out)
}

fn required_block_str<'a>(
    block: &'a Value,
    key: &str,
    context: &str,
    raw: &Value,
) -> Result<&'a str> {
    block.get(key).and_then(Value::as_str).ok_or_else(|| {
        MessageParseError::new(
            format!("Missing required field in {context}: {key}"),
            Some(raw.clone()),
        )
        .into()
    })
}

fn required_block_value(block: &Value, key: &str, context: &str, raw: &Value) -> Result<Value> {
    block.get(key).cloned().ok_or_else(|| {
        MessageParseError::new(
            format!("Missing required field in {context}: {key}"),
            Some(raw.clone()),
        )
        .into()
    })
}

fn required_str_in(data: &Value, key: &str, context: &str) -> Result<String> {
    data.get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| {
            MessageParseError::new(
                format!("Missing required field in {context}: {key}"),
                Some(data.clone()),
            )
            .into()
        })
}

fn required_i64_in(data: &Value, key: &str, context: &str) -> Result<i64> {
    data.get(key).and_then(Value::as_i64).ok_or_else(|| {
        MessageParseError::new(
            format!("Missing required field in {context}: {key}"),
            Some(data.clone()),
        )
        .into()
    })
}

fn required_bool_in(data: &Value, key: &str, context: &str) -> Result<bool> {
    data.get(key).and_then(Value::as_bool).ok_or_else(|| {
        MessageParseError::new(
            format!("Missing required field in {context}: {key}"),
            Some(data.clone()),
        )
        .into()
    })
}

fn required_task_usage_in(data: &Value, key: &str, context: &str) -> Result<TaskUsage> {
    let usage = data.get(key).cloned().ok_or_else(|| {
        MessageParseError::new(
            format!("Missing required field in {context}: {key}"),
            Some(data.clone()),
        )
    })?;
    parse_task_usage(usage, data)
}

fn parse_task_usage(usage: Value, raw: &Value) -> Result<TaskUsage> {
    serde_json::from_value(usage).map_err(|err| {
        MessageParseError::new(
            format!("Invalid task usage in system message: {err}"),
            Some(raw.clone()),
        )
        .into()
    })
}

fn opt_str(data: &Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn python_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "None",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn is_python_falsey_json(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(value) => !value,
        Value::Number(number) => {
            number.as_i64() == Some(0) || number.as_u64() == Some(0) || number.as_f64() == Some(0.0)
        }
        Value::String(value) => value.is_empty(),
        Value::Array(value) => value.is_empty(),
        Value::Object(value) => value.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentBlock, Message, UserMessageContent};
    use serde_json::json;

    #[test]
    fn parses_valid_user_message() {
        let message = parse_message(json!({
            "type": "user",
            "uuid": "msg-abc123-def456",
            "message": {"content": [{"type": "text", "text": "Hello"}]},
        }))
        .unwrap()
        .unwrap();

        let Message::User(user) = message else {
            panic!("expected user message");
        };
        assert_eq!(user.uuid.as_deref(), Some("msg-abc123-def456"));
        let UserMessageContent::Blocks(blocks) = user.content else {
            panic!("expected content blocks");
        };
        assert!(matches!(&blocks[0], ContentBlock::Text { text } if text == "Hello"));
    }

    #[test]
    fn parses_user_tool_result_and_metadata() {
        let message = parse_message(json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tool_error",
                    "content": "File not found",
                    "is_error": true,
                }]
            },
            "tool_use_result": {"filePath": "/path/to/file.py"},
        }))
        .unwrap()
        .unwrap();

        let Message::User(user) = message else {
            panic!("expected user message");
        };
        assert_eq!(
            user.tool_use_result.unwrap()["filePath"],
            "/path/to/file.py"
        );
        let UserMessageContent::Blocks(blocks) = user.content else {
            panic!("expected content blocks");
        };
        assert!(matches!(
            &blocks[0],
            ContentBlock::ToolResult {
                tool_use_id,
                content: Some(content),
                is_error: Some(true),
            } if tool_use_id == "tool_error" && content == "File not found"
        ));
    }

    #[test]
    fn parses_assistant_thinking_server_tool_and_usage() {
        let message = parse_message(json!({
            "type": "assistant",
            "session_id": "sess",
            "uuid": "uuid",
            "message": {
                "id": "msg-id",
                "model": "claude-opus-4-1-20250805",
                "usage": {"input_tokens": 1},
                "stop_reason": "end_turn",
                "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "server_tool_use", "id": "srv", "name": "web_search", "input": {"query": "x"}},
                    {"type": "advisor_tool_result", "tool_use_id": "srv", "content": {"type": "ok"}}
                ]
            }
        }))
        .unwrap()
        .unwrap();

        let Message::Assistant(assistant) = message else {
            panic!("expected assistant message");
        };
        assert_eq!(assistant.message_id.as_deref(), Some("msg-id"));
        assert_eq!(assistant.session_id.as_deref(), Some("sess"));
        assert!(
            matches!(&assistant.content[0], ContentBlock::Thinking { thinking, signature } if thinking == "hmm" && signature == "sig")
        );
        assert!(
            matches!(&assistant.content[1], ContentBlock::ServerToolUse { name, .. } if name == "web_search")
        );
        assert!(
            matches!(&assistant.content[2], ContentBlock::ServerToolResult { tool_use_id, .. } if tool_use_id == "srv")
        );
    }

    #[test]
    fn parses_result_with_deferred_tool_use() {
        let message = parse_message(json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 10,
            "duration_api_ms": 7,
            "is_error": false,
            "num_turns": 1,
            "session_id": "sess",
            "deferred_tool_use": {"id": "toolu", "name": "Bash", "input": {"command": "pwd"}},
            "modelUsage": {"claude": 1}
        }))
        .unwrap()
        .unwrap();

        let Message::Result(result) = message else {
            panic!("expected result message");
        };
        assert_eq!(result.deferred_tool_use.unwrap().name, "Bash");
        assert_eq!(result.model_usage.unwrap()["claude"], 1);
    }

    #[test]
    fn skips_unknown_message_types() {
        assert!(parse_message(json!({"type": "future"})).unwrap().is_none());
    }

    #[test]
    fn rejects_missing_type() {
        let err = parse_message(json!({"message": {}})).unwrap_err();
        assert!(err.to_string().contains("Message missing 'type' field"));
    }
}
