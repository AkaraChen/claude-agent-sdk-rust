# Claude Agent SDK for Rust

Rust SDK for Claude Agent. This crate mirrors the Python
`claude_agent_sdk` package where practical, while exposing idiomatic Rust
types, async streams, and `tokio`-based APIs.

For Agent SDK concepts and Claude Code behavior, see the
[Claude Agent SDK documentation](https://platform.claude.com/docs/en/agent-sdk).

## Installation

```bash
cargo add claude-agent-sdk-rust
```

Or add it manually:

```toml
[dependencies]
claude-agent-sdk-rust = "0.1.75"
futures = "0.3"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

**Prerequisites:**

- Rust 1.85+ for edition 2024
- A `tokio` runtime
- Claude Code CLI 2.0.0+

The SDK looks for a bundled Claude Code CLI first, then for `claude` on `PATH`
and common install locations. To install Claude Code separately:

```bash
npm install -g @anthropic-ai/claude-code
```

To use a specific binary, set `ClaudeAgentOptions::cli_path`.

## Quick Start

```rust
use claude_agent_sdk::{query, ContentBlock, Message, Prompt};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let mut messages = query(Prompt::from("What is 2 + 2?"), None, None);

    while let Some(message) = messages.next().await {
        if let Message::Assistant(assistant) = message? {
            for block in assistant.content {
                if let ContentBlock::Text { text } = block {
                    println!("{text}");
                }
            }
        }
    }

    Ok(())
}
```

## Basic Usage: query()

`query()` is an async-stream API for one-shot Claude Code requests. It returns a
`Stream<Item = Result<Message>>`. See [src/query.rs](src/query.rs).

```rust
use claude_agent_sdk::{
    query, ClaudeAgentOptions, ContentBlock, Message, Prompt, SystemPrompt,
};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let options = ClaudeAgentOptions {
        system_prompt: Some(SystemPrompt::Text("You are a helpful assistant".into())),
        max_turns: Some(1),
        ..Default::default()
    };

    let mut messages = query(Prompt::from("Tell me a joke"), Some(options), None);
    while let Some(message) = messages.next().await {
        match message? {
            Message::Assistant(assistant) => {
                for block in assistant.content {
                    if let ContentBlock::Text { text } = block {
                        println!("{text}");
                    }
                }
            }
            Message::Result(result) if result.is_error => {
                eprintln!("Claude Code returned an error: {:?}", result.errors);
            }
            _ => {}
        }
    }

    Ok(())
}
```

### Using Tools

By default, Claude has access to the Claude Code toolset. `allowed_tools` is a
permission allowlist: listed tools are auto-approved. It does not remove tools
from Claude's toolset. To block specific tools, use `disallowed_tools`.

```rust
use claude_agent_sdk::{ClaudeAgentOptions, PermissionMode};

let options = ClaudeAgentOptions {
    allowed_tools: vec!["Read".into(), "Write".into(), "Bash".into()],
    permission_mode: Some(PermissionMode::AcceptEdits),
    ..Default::default()
};
```

### Working Directory

```rust
use std::path::PathBuf;
use claude_agent_sdk::ClaudeAgentOptions;

let options = ClaudeAgentOptions {
    cwd: Some(PathBuf::from("/path/to/project")),
    ..Default::default()
};
```

## ClaudeSdkClient

`ClaudeSdkClient` supports bidirectional, interactive conversations with Claude
Code. See [src/client.rs](src/client.rs).

Unlike `query()`, `ClaudeSdkClient` supports sending multiple prompts over the
same connection, receiving one response at a time, control methods, custom
tools, and hooks.

```rust
use claude_agent_sdk::{ClaudeSdkClient, Message, Prompt};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let mut client = ClaudeSdkClient::new(None, None);
    client.connect(None).await?;

    client.send_query(Prompt::from("Hello Claude"), None).await?;

    let mut response = client.receive_response()?;
    while let Some(message) = response.next().await {
        println!("{:?}", message?);
    }
    drop(response);

    client.disconnect().await?;
    Ok(())
}
```

Useful client methods include:

- `send_query()` - send another prompt, optionally with a session ID
- `receive_response()` - stream messages until the next `Result` message
- `receive_messages()` - stream all messages until disconnect
- `interrupt()` - interrupt the active turn
- `set_permission_mode()` and `set_model()` - update runtime settings
- `get_mcp_status()` and `get_context_usage()` - inspect runtime state
- `reconnect_mcp_server()` and `toggle_mcp_server()` - control MCP servers

## Custom Tools

Custom tools are implemented as in-process SDK MCP servers. They run in the
same Rust process as your application, avoiding subprocess management for tools
you own.

Add the usual serialization dependencies for typed tool arguments:

```toml
[dependencies]
schemars = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

```rust
use claude_agent_sdk::{
    create_sdk_mcp_server, tool, ClaudeAgentOptions, ClaudeSdkClient, Prompt,
};
use futures::StreamExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct GreetArgs {
    name: String,
}

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let greet = tool::<GreetArgs, _, _>(
        "greet",
        "Greet a user",
        |args| async move {
            Ok(json!({
                "content": [
                    {"type": "text", "text": format!("Hello, {}!", args.name)}
                ]
            }))
        },
        None,
    );

    let server = create_sdk_mcp_server("my-tools", "1.0.0", vec![greet]);
    let mut options = ClaudeAgentOptions::default().with_sdk_mcp_server("tools", server);
    options.allowed_tools = vec!["mcp__tools__greet".into()];

    let mut client = ClaudeSdkClient::new(Some(options), None);
    client.connect(None).await?;
    client.send_query(Prompt::from("Greet Alice"), None).await?;

    let mut response = client.receive_response()?;
    while let Some(message) = response.next().await {
        println!("{:?}", message?);
    }
    drop(response);

    client.disconnect().await?;
    Ok(())
}
```

You can also define SDK tools with the `#[sdk_tool]` macro:

```rust
use claude_agent_sdk::{create_sdk_mcp_server, sdk_tool};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct GreetArgs {
    name: String,
}

#[sdk_tool(name = "greet_user", description = "Greets a user by name")]
async fn greet(args: GreetArgs) -> claude_agent_sdk::Result<Value> {
    Ok(json!({
        "content": [{"type": "text", "text": format!("Hello, {}!", args.name)}]
    }))
}

let server = create_sdk_mcp_server("attribute-tools", "1.0.0", vec![greet_tool()]);
```

### External MCP Servers

You can combine SDK MCP servers and external MCP servers:

```rust
use claude_agent_sdk::ClaudeAgentOptions;
use serde_json::json;

let options = ClaudeAgentOptions {
    mcp_servers: Some(json!({
        "external": {
            "type": "stdio",
            "command": "external-server",
            "args": ["--flag"]
        }
    })),
    ..Default::default()
};
```

## Hooks

A hook is a Rust callback invoked by the Claude Code application at specific
points in the agent loop. Hooks can provide deterministic validation,
additional context, or permission decisions.

```rust
use std::{collections::HashMap, sync::Arc};

use claude_agent_sdk::{ClaudeAgentOptions, HookCallback, HookMatcher};
use serde_json::json;

let check_bash: HookCallback = Arc::new(|input, _tool_use_id, _context| {
    Box::pin(async move {
        let command = input["tool_input"]["command"].as_str().unwrap_or("");
        if command.contains("foo.sh") {
            return Ok(json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": "Command contains invalid pattern: foo.sh"
                }
            }));
        }
        Ok(json!({}))
    })
});

let mut hooks = HashMap::new();
hooks.insert(
    "PreToolUse".to_string(),
    vec![HookMatcher {
        matcher: Some("Bash".to_string()),
        hooks: vec![check_bash],
        timeout: Some(2.0),
    }],
);

let options = ClaudeAgentOptions {
    allowed_tools: vec!["Bash".into()],
    hooks: Some(hooks),
    ..Default::default()
};
```

## Permission Callback

`can_use_tool` lets your application approve, deny, or modify individual tool
uses at runtime. In this Rust port, `can_use_tool` requires streaming input
mode because the callback is served over the SDK control channel.

```rust
use std::sync::Arc;

use claude_agent_sdk::{
    CanUseTool, ClaudeAgentOptions, PermissionResult, Prompt,
};
use futures::{stream, StreamExt};
use serde_json::json;

let can_use_tool: CanUseTool = Arc::new(|tool_name, input, _context| {
    Box::pin(async move {
        let command = input["command"].as_str().unwrap_or("");
        if tool_name == "Bash" && command.contains("rm -rf") {
            return Ok(PermissionResult::Deny {
                message: "Refusing destructive command".into(),
                interrupt: true,
            });
        }

        Ok(PermissionResult::Allow {
            updated_input: None,
            updated_permissions: None,
        })
    })
});

let prompt = stream::iter(vec![json!({
    "type": "user",
    "message": {"role": "user", "content": "List files in this repo"}
})])
.boxed();

let options = ClaudeAgentOptions {
    can_use_tool: Some(can_use_tool),
    ..Default::default()
};

let initial_prompt = Prompt::Stream(prompt);
```

## Session Stores

The SDK can mirror Claude Code transcripts into custom storage. Built-in store
implementations include:

- `InMemorySessionStore`
- `RedisSessionStore`
- `PostgresSessionStore`
- `S3SessionStore`

```rust
use std::{path::Path, sync::Arc};

use claude_agent_sdk::{
    get_session_messages_from_store, list_sessions_from_store, query, ClaudeAgentOptions,
    InMemorySessionStore, Prompt, SessionStoreFlushMode,
};
use futures::StreamExt;

#[tokio::main]
async fn main() -> claude_agent_sdk::Result<()> {
    let store = Arc::new(InMemorySessionStore::new());
    let options = ClaudeAgentOptions {
        session_store: Some(store.clone()),
        session_store_flush: SessionStoreFlushMode::Batched,
        ..Default::default()
    };

    let mut messages = query(Prompt::from("Summarize this project"), Some(options), None);
    while let Some(message) = messages.next().await {
        println!("{:?}", message?);
    }

    let sessions = list_sessions_from_store(store.as_ref(), Some(Path::new(".")), None, 0).await?;
    if let Some(session) = sessions.first() {
        let transcript = get_session_messages_from_store(
            store.as_ref(),
            &session.session_id,
            Some(Path::new(".")),
            None,
            0,
        )
        .await?;
        println!("loaded {} transcript messages", transcript.len());
    }

    Ok(())
}
```

Session helper APIs include:

- `list_sessions()` and `list_sessions_from_store()`
- `get_session_info()` and `get_session_info_from_store()`
- `get_session_messages()` and `get_session_messages_from_store()`
- `list_subagents()` and `list_subagents_from_store()`
- `rename_session()`, `tag_session()`, `delete_session()`, and store variants
- `fork_session()` and `fork_session_via_store()`
- `import_session_to_store()`

Custom stores can implement the `SessionStore` trait. See
[src/types.rs](src/types.rs) and [src/session_stores.rs](src/session_stores.rs).

## Types

See [src/types.rs](src/types.rs) for complete type definitions:

- `ClaudeAgentOptions` - SDK and Claude Code configuration
- `Message` - top-level stream message enum
- `AssistantMessage`, `UserMessage`, `SystemMessage`, `ResultMessage` - message
  variants
- `ContentBlock`, `TextBlock`, `ToolUseBlock`, `ToolResultBlock` - content
  blocks
- `HookMatcher`, `HookCallback`, `HookInput`, `HookJsonOutput` - hooks
- `SessionStore`, `SessionKey`, `SdkSessionInfo` - session storage
- `McpStatusResponse`, `ContextUsageResponse` - runtime inspection responses

## Error Handling

```rust
use claude_agent_sdk::{query, ClaudeSdkError, Message, Prompt};
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), ClaudeSdkError> {
    let mut messages = query(Prompt::from("Hello"), None, None);
    while let Some(message) = messages.next().await {
        match message {
            Ok(Message::Assistant(msg)) => println!("{msg:?}"),
            Ok(_) => {}
            Err(ClaudeSdkError::CliNotFound(err)) => {
                eprintln!("Claude Code not found: {}", err);
            }
            Err(ClaudeSdkError::Process(err)) => {
                eprintln!("Process failed with exit code: {:?}", err.exit_code);
            }
            Err(ClaudeSdkError::CliJsonDecode(err)) => {
                eprintln!("Failed to parse response: {}", err);
            }
            Err(err) => return Err(err),
        }
    }

    Ok(())
}
```

Main error types are defined in [src/errors.rs](src/errors.rs):

- `ClaudeSdkError`
- `CliNotFoundError`
- `CliConnectionError`
- `ProcessError`
- `CliJsonDecodeError`
- `MessageParseError`

## Available Tools

See the [Claude Code documentation](https://code.claude.com/docs/en/settings#tools-available-to-claude)
for the complete list of built-in Claude Code tools.

## Development

```bash
cargo fmt --all
cargo test
```

The Rust package keeps version metadata aligned with the vendored Python SDK in
`claude-agent-sdk-python/`.

## License and Terms

Use of this SDK is governed by Anthropic's
[Commercial Terms of Service](https://www.anthropic.com/legal/commercial-terms),
including when you use it to power products and services that you make
available to your own customers and end users, except to the extent a specific
component or dependency is covered by a different license as indicated in that
component's LICENSE file.
