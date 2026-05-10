use std::sync::Arc;

use claude_agent_sdk::{
    ClaudeSdkError, SdkMcpTool, SdkMcpToolHandler, ToolAnnotations, create_sdk_mcp_server, tool,
};
use futures::FutureExt;
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct GreetArgs {
    name: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct AddArgs {
    a: f64,
    b: f64,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct EmptyArgs {}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct SearchArgs {
    query: String,
    tags: Vec<String>,
    limit: Option<i64>,
}

fn text_result(text: impl Into<String>) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}]})
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
async fn sdk_mcp_server_lists_and_calls_registered_tools() {
    let executions = Arc::new(Mutex::new(Vec::<Value>::new()));
    let executions_for_greet = executions.clone();
    let greet = tool::<GreetArgs, _, _, _>(
        "greet_user",
        "Greets a user by name",
        move |args| {
            let executions = executions_for_greet.clone();
            async move {
                executions
                    .lock()
                    .await
                    .push(json!({"name": "greet_user", "args": {"name": args.name}}));
                Ok(text_result(format!(
                    "Hello, {}!",
                    executions.lock().await[0]["args"]["name"].as_str().unwrap()
                )))
            }
        },
        None,
    );
    let executions_for_add = executions.clone();
    let add = tool::<AddArgs, _, _, _>(
        "add_numbers",
        "Adds two numbers",
        move |args| {
            let executions = executions_for_add.clone();
            async move {
                let result = args.a + args.b;
                executions
                    .lock()
                    .await
                    .push(json!({"name": "add_numbers", "args": {"a": args.a, "b": args.b}}));
                Ok(text_result(format!("The sum is {result}")))
            }
        },
        None,
    );
    let server = create_sdk_mcp_server("test-sdk-server", "1.0.0", vec![greet, add]);

    assert_eq!(
        server.serializable_config(),
        json!({"type": "sdk", "name": "test-sdk-server"})
    );

    let listed = server
        .handle_json_rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "greet_user");
    assert_eq!(tools[1]["name"], "add_numbers");
    let names = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert!(names.contains("greet_user"));
    assert!(names.contains("add_numbers"));

    let greet_result = call_tool(&server, "greet_user", json!({"name": "Alice"})).await;
    assert_eq!(
        greet_result["result"]["content"][0]["text"],
        "Hello, Alice!"
    );
    assert!(greet_result["result"].get("isError").is_none());
    let add_result = call_tool(&server, "add_numbers", json!({"a": 5, "b": 3})).await;
    assert!(
        add_result["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("8")
    );

    let executions = executions.lock().await;
    assert_eq!(executions.len(), 2);
    assert_eq!(executions[0]["name"], "greet_user");
    assert_eq!(executions[1]["args"]["a"], 5.0);
}

#[tokio::test]
async fn sdk_mcp_tool_direct_handler_and_server_error_result_semantics() {
    let echo = tool::<GreetArgs, _, _, _>(
        "echo",
        "Echo input",
        |args| async move { Ok(json!({"output": args.name})) },
        None,
    );
    assert_eq!(echo.name(), "echo");
    assert_eq!(echo.description(), "Echo input");
    let direct = echo.call_raw(json!({"name": "test"})).await.unwrap();
    assert_eq!(direct, json!({"output": "test"}));

    let fail = tool::<EmptyArgs, _, _, _>(
        "fail",
        "Always fails",
        |_args| async move {
            Err::<serde_json::Value, _>(ClaudeSdkError::Runtime("Expected error".to_string()))
        },
        None,
    );
    let server = create_sdk_mcp_server("error-test", "1.0.0", vec![fail]);
    let result = call_tool(&server, "fail", json!({})).await;
    assert_eq!(result["result"]["isError"], true);
    assert!(
        result["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Expected error")
    );

    let missing = call_tool(&server, "missing", json!({})).await;
    assert_eq!(missing["result"]["isError"], true);
    assert!(
        missing["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Tool 'missing' not found")
    );

    let invalid = tool::<EmptyArgs, _, _, _>(
        "invalid",
        "Returns a non-object result",
        |_args| async move { Ok(json!("not an object")) },
        None,
    );
    let server = create_sdk_mcp_server("invalid-result-test", "1.0.0", vec![invalid]);
    let result = call_tool(&server, "invalid", json!({})).await;
    assert_eq!(result["result"]["isError"], true);
    assert!(
        result["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Tool result must be a JSON object")
    );
}

#[tokio::test]
async fn empty_server_tools_methods_match_python_missing_handlers() {
    let server = create_sdk_mcp_server("test-server", "2.0.0", vec![]);

    assert_eq!(
        server.serializable_config(),
        json!({"type": "sdk", "name": "test-server"})
    );
    assert_eq!(server.name, "test-server");
    assert_eq!(server.version, "2.0.0");

    let listed = server
        .handle_json_rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    assert_eq!(listed["error"]["code"], -32601);
    assert_eq!(listed["error"]["message"], "Method 'tools/list' not found");
}

#[tokio::test]
async fn initialized_notification_response_matches_python_bridge_shape() {
    let server = create_sdk_mcp_server("notification-test", "1.0.0", vec![]);
    let response = server
        .handle_json_rpc(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {},
        }))
        .await;

    assert_eq!(response, json!({"jsonrpc": "2.0", "result": {}}));
    assert!(response.get("id").is_none());
}

#[tokio::test]
async fn is_error_images_resources_and_unknown_content_are_converted_like_python() {
    let png_data = "iVBORw0KGgo=".to_string();
    let mixed = tool::<EmptyArgs, _, _, _>(
        "mixed",
        "Returns mixed content",
        move |_args| {
            let png_data = png_data.clone();
            async move {
                Ok(json!({
                    "content": [
                        {"type": "text", "text": "Here is the document:"},
                        {"type": "image", "data": png_data, "mimeType": "image/png"},
                        {
                            "type": "resource_link",
                            "name": "Report",
                            "uri": "https://example.com/report",
                            "description": "A test report"
                        },
                        {
                            "type": "resource",
                            "resource": {"uri": "file:///test.txt", "text": "File contents here"}
                        },
                        {
                            "type": "resource",
                            "resource": {"uri": "file:///image.png", "blob": "iVBORw0KGgo="}
                        },
                        {"type": "custom_widget", "data": "skipped"}
                    ],
                    "is_error": true,
                }))
            }
        },
        None,
    );
    let server = create_sdk_mcp_server("content-test", "1.0.0", vec![mixed]);
    let result = call_tool(&server, "mixed", json!({})).await;

    assert_eq!(result["result"]["isError"], true);
    let content = result["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 4);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "Here is the document:");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["data"], "iVBORw0KGgo=");
    assert_eq!(content[1]["mimeType"], "image/png");
    assert_eq!(content[2]["type"], "text");
    assert!(content[2]["text"].as_str().unwrap().contains("Report"));
    assert!(
        content[2]["text"]
            .as_str()
            .unwrap()
            .contains("https://example.com/report")
    );
    assert_eq!(content[3]["text"], "File contents here");
}

#[tokio::test]
async fn structured_content_is_not_forwarded_by_python_bridge() {
    let structured = tool::<EmptyArgs, _, _, _>(
        "structured",
        "Returns structured content",
        |_args| async move {
            Ok(json!({
                "content": [{"type": "text", "text": "ok"}],
                "structuredContent": {"value": 1},
                "structured_content": {"value": 2},
            }))
        },
        None,
    );
    let server = create_sdk_mcp_server("structured-test", "1.0.0", vec![structured]);
    let result = call_tool(&server, "structured", json!({})).await;

    assert_eq!(result["result"]["content"][0]["text"], "ok");
    assert!(result["result"].get("structuredContent").is_none());
    assert!(result["result"].get("structured_content").is_none());
}

#[tokio::test]
async fn annotations_and_large_result_meta_flow_through_tools_list() {
    let read_only = tool::<GreetArgs, _, _, _>(
        "read_data",
        "Read data from source",
        |args| async move { Ok(text_result(format!("Data from {}", args.name))) },
        Some(ToolAnnotations::new().read_only(true).open_world(false)),
    );
    let destructive = tool::<GreetArgs, _, _, _>(
        "delete_item",
        "Delete an item",
        |args| async move { Ok(text_result(format!("Deleted {}", args.name))) },
        Some(ToolAnnotations::new().destructive(true).idempotent(true)),
    );
    let large: SdkMcpTool = tool::<EmptyArgs, _, _, _>(
        "get_large_schema",
        "Returns a large DB schema that may exceed 50K chars.",
        |_args| async move { Ok(text_result("schema")) },
        None,
    )
    .with_max_result_size_chars(500_000);
    let plain = tool::<EmptyArgs, _, _, _>(
        "plain_tool",
        "A tool without annotations",
        |_args| async move { Ok(text_result("ok")) },
        None,
    );
    let server = create_sdk_mcp_server(
        "annotations-test",
        "1.0.0",
        vec![read_only, destructive, large, plain],
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
        tools_by_name["read_data"]["annotations"]["readOnlyHint"],
        true
    );
    assert_eq!(
        tools_by_name["read_data"]["annotations"]["openWorldHint"],
        false
    );
    assert_eq!(
        tools_by_name["delete_item"]["annotations"]["destructiveHint"],
        true
    );
    assert_eq!(
        tools_by_name["delete_item"]["annotations"]["idempotentHint"],
        true
    );
    assert!(tools_by_name["plain_tool"].get("annotations").is_none());
    assert_eq!(
        tools_by_name["get_large_schema"]["_meta"]["anthropic/maxResultSizeChars"],
        500_000
    );
    assert!(
        tools_by_name["plain_tool"]
            .get("_meta")
            .and_then(|meta| meta.get("anthropic/maxResultSizeChars"))
            .is_none()
    );
}

#[tokio::test]
async fn raw_tool_escape_hatch_can_passthrough_custom_json_schema() {
    let handler: SdkMcpToolHandler = Arc::new(|arguments| {
        async move {
            Ok(text_result(format!(
                "Hello {}",
                arguments["name"].as_str().unwrap()
            )))
        }
        .boxed()
    });
    let raw = SdkMcpTool::new_raw(
        "validate",
        "Validate input",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "minLength": 1},
                "age": {"type": "integer", "minimum": 0},
            },
            "required": ["name"],
        }),
        handler,
        None,
    );
    let server = create_sdk_mcp_server("raw-schema-test", "1.0.0", vec![raw]);
    let listed = server
        .handle_json_rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    assert_eq!(
        listed["result"]["tools"][0]["inputSchema"]["properties"]["name"]["minLength"],
        1
    );
    let result = call_tool(&server, "validate", json!({"name": "Alice"})).await;
    assert_eq!(result["result"]["content"][0]["text"], "Hello Alice");
}

#[tokio::test]
async fn derived_json_schema_covers_lists_and_optional_fields() {
    let search = tool::<SearchArgs, _, _, _>(
        "search",
        "Search for tagged items",
        |args| async move {
            Ok(text_result(format!(
                "{}:{}:{}",
                args.query,
                args.tags.join(","),
                args.limit.unwrap_or_default()
            )))
        },
        None,
    );
    let server = create_sdk_mcp_server("schema-test", "1.0.0", vec![search]);
    let listed = server
        .handle_json_rpc(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}))
        .await;
    let schema = &listed["result"]["tools"][0]["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["properties"]["query"]["type"], "string");
    assert_eq!(schema["properties"]["tags"]["type"], "array");
    assert_eq!(schema["properties"]["tags"]["items"]["type"], "string");
    let required = schema["required"].as_array().unwrap();
    assert!(required.iter().any(|field| field == "query"));
    assert!(required.iter().any(|field| field == "tags"));
    assert!(!required.iter().any(|field| field == "limit"));

    let result = call_tool(
        &server,
        "search",
        json!({"query": "rust", "tags": ["sdk", "mcp"]}),
    )
    .await;
    assert_eq!(result["result"]["content"][0]["text"], "rust:sdk,mcp:0");
}
