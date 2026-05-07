use claude_agent_sdk::{ToolAnnotations, create_sdk_mcp_server, sdk_tool};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct GreetArgs {
    name: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct EmptyArgs {}

#[sdk_tool(name = "greet_user", description = "Greets a user by name")]
async fn greet(args: GreetArgs) -> claude_agent_sdk::Result<Value> {
    Ok(json!({
        "content": [{"type": "text", "text": format!("Hello, {}!", args.name)}]
    }))
}

#[sdk_tool(
    name = "read_data",
    description = "Reads cached data",
    annotations = ToolAnnotations::new().read_only(true).open_world(false),
    max_result_size_chars = 500000
)]
async fn read_data(_args: EmptyArgs) -> claude_agent_sdk::Result<Value> {
    Ok(json!({"content": [{"type": "text", "text": "cached"}]}))
}

#[sdk_tool(
    name = "custom",
    description = "Custom factory name",
    tool_fn = "custom_factory"
)]
async fn custom_named(_args: EmptyArgs) -> claude_agent_sdk::Result<Value> {
    Ok(json!({"content": [{"type": "text", "text": "custom"}]}))
}

async fn call_tool(server: &claude_agent_sdk::SdkMcpServer, name: &str, arguments: Value) -> Value {
    server
        .handle_json_rpc(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments},
        }))
        .await
}

#[tokio::test]
async fn sdk_tool_attribute_generates_tool_factories() {
    let server = create_sdk_mcp_server(
        "attribute-test",
        "1.0.0",
        vec![greet_tool(), read_data_tool(), custom_factory()],
    );

    let listed = server
        .handle_json_rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let tools_by_name = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| (tool["name"].as_str().unwrap().to_string(), tool.clone()))
        .collect::<std::collections::HashMap<_, _>>();

    assert_eq!(
        tools_by_name["greet_user"]["description"],
        "Greets a user by name"
    );
    assert!(tools_by_name["greet_user"]["inputSchema"]["properties"]["name"].is_object());
    assert_eq!(
        tools_by_name["read_data"]["annotations"]["readOnlyHint"],
        true
    );
    assert_eq!(
        tools_by_name["read_data"]["_meta"]["anthropic/maxResultSizeChars"],
        500000
    );
    assert_eq!(
        tools_by_name["custom"]["description"],
        "Custom factory name"
    );

    let greet_result = call_tool(&server, "greet_user", json!({"name": "Alice"})).await;
    assert_eq!(
        greet_result["result"]["content"][0]["text"],
        "Hello, Alice!"
    );
}
