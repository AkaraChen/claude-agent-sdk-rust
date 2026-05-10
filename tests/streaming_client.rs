#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

use claude_agent_sdk::{ClaudeAgentOptions, InputMessage, Message, Prompt, query};
use futures::StreamExt;
use futures::stream;

#[tokio::test]
async fn query_with_stream_prompt_writes_all_messages_to_subprocess_stdin() {
    if Command::new("python3").arg("--version").output().is_err() {
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("mock_claude.py");
    std::fs::write(
        &script,
        r#"#!/usr/bin/env python3
import json
import sys

if len(sys.argv) > 1 and sys.argv[1] == "-v":
    print("2.1.131 (Claude Code)")
    raise SystemExit(0)

users = []
for line in sys.stdin:
    if not line.strip():
        continue
    msg = json.loads(line)
    if msg.get("type") == "control_request" and msg.get("request", {}).get("subtype") == "initialize":
        print(json.dumps({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": msg.get("request_id"),
                "response": {"commands": [], "output_style": "default"},
            },
        }), flush=True)
    elif msg.get("type") == "user":
        users.append(msg)

if len(users) != 2:
    print(f"expected 2 user messages, got {len(users)}", file=sys.stderr)
    raise SystemExit(2)
if users[0]["message"]["content"] != "First":
    print("missing First user message", file=sys.stderr)
    raise SystemExit(3)
if users[1]["message"]["content"] != "Second":
    print("missing Second user message", file=sys.stderr)
    raise SystemExit(4)

print(json.dumps({
    "type": "result",
    "subtype": "success",
    "duration_ms": 100,
    "duration_api_ms": 50,
    "is_error": False,
    "num_turns": 1,
    "session_id": "test",
    "total_cost_usd": 0.001,
}), flush=True)
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();

    let prompt = stream::iter(vec![
        InputMessage::user("First"),
        InputMessage::user("Second"),
    ])
    .boxed();

    let mut messages = query(
        Prompt::Stream(prompt),
        Some(ClaudeAgentOptions {
            cli_path: Some(PathBuf::from(&script)),
            ..Default::default()
        }),
        None,
    );
    let mut collected = Vec::new();
    while let Some(message) = messages.next().await {
        collected.push(message.unwrap());
    }

    assert_eq!(collected.len(), 1);
    let Message::Result(result) = &collected[0] else {
        panic!("expected result message, got {:?}", collected[0]);
    };
    assert_eq!(result.subtype, "success");
    assert_eq!(result.session_id, "test");
    assert_eq!(result.total_cost_usd, Some(0.001));
}
