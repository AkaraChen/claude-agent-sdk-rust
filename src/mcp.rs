use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use futures::FutureExt;
use rmcp::model::{CallToolResult, Content, JsonObject, Meta, Tool, ToolAnnotations};
use rmcp::schemars::{JsonSchema, schema_for};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::errors::{ClaudeSdkError, Result};
use crate::types::BoxFutureResult;

pub type SdkMcpToolHandler = Arc<dyn Fn(Value) -> BoxFutureResult<Result<Value>> + Send + Sync>;

#[derive(Clone)]
pub struct SdkMcpTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub handler: SdkMcpToolHandler,
    pub annotations: Option<ToolAnnotations>,
    pub max_result_size_chars: Option<i64>,
}

impl std::fmt::Debug for SdkMcpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SdkMcpTool")
            .field("name", &self.name)
            .field("description", &self.description)
            .field("input_schema", &self.input_schema)
            .field("annotations", &self.annotations)
            .field("max_result_size_chars", &self.max_result_size_chars)
            .finish_non_exhaustive()
    }
}

impl SdkMcpTool {
    pub fn new_raw(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        handler: SdkMcpToolHandler,
        annotations: Option<ToolAnnotations>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            handler,
            annotations,
            max_result_size_chars: None,
        }
    }

    pub fn with_max_result_size_chars(mut self, max_result_size_chars: i64) -> Self {
        self.max_result_size_chars = Some(max_result_size_chars);
        self
    }

    pub fn rmcp_tool(&self) -> Tool {
        let schema = value_to_json_object(self.input_schema.clone());
        let mut tool = Tool::new(
            self.name.clone(),
            self.description.clone(),
            Arc::new(schema),
        );
        tool.annotations = self.annotations.clone();
        if let Some(max_size) = self.max_result_size_chars {
            let mut meta = JsonObject::new();
            meta.insert(
                "anthropic/maxResultSizeChars".to_string(),
                Value::Number(max_size.into()),
            );
            tool.meta = Some(Meta(meta));
        }
        tool
    }

    pub async fn call(&self, arguments: Value) -> Result<Value> {
        let result = (self.handler)(arguments).await?;
        Ok(normalize_tool_result(result)?)
    }
}

#[derive(Debug, Clone)]
pub struct SdkMcpServer {
    pub name: String,
    pub version: String,
    tools: Vec<SdkMcpTool>,
    tool_map: HashMap<String, SdkMcpTool>,
}

impl SdkMcpServer {
    pub fn new(
        name: impl Into<String>,
        version: impl Into<String>,
        tools: Vec<SdkMcpTool>,
    ) -> Self {
        let tool_map = tools
            .iter()
            .map(|tool| (tool.name.clone(), tool.clone()))
            .collect();
        Self {
            name: name.into(),
            version: version.into(),
            tools,
            tool_map,
        }
    }

    pub fn serializable_config(&self) -> Value {
        json!({
            "type": "sdk",
            "name": self.name,
        })
    }

    pub async fn handle_json_rpc(&self, message: Value) -> Value {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "notifications/initialized" {
            return json!({"jsonrpc": "2.0", "result": {}});
        }
        let response = match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": self.name, "version": self.version},
            })),
            "tools/list" => {
                if self.tools.is_empty() {
                    return method_not_found_response(id, method);
                }
                let tools: Result<Vec<Value>> = self
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::to_value(tool.rmcp_tool()).map_err(ClaudeSdkError::from)
                    })
                    .collect();
                tools.map(|tools| json!({ "tools": tools }))
            }
            "tools/call" => {
                if self.tools.is_empty() {
                    return method_not_found_response(id, method);
                }
                let params = message.get("params").cloned().unwrap_or(Value::Null);
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match self.tool_map.get(&name) {
                    Some(tool) => match tool.call(arguments).await {
                        Ok(result) => Ok(result),
                        Err(error) => error_tool_result(error.to_string()),
                    },
                    None => error_tool_result(format!("Tool '{name}' not found")),
                }
            }
            other => Err(ClaudeSdkError::Runtime(format!(
                "Method '{other}' not found"
            ))),
        };

        match response {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32603, "message": error.to_string()},
            }),
        }
    }
}

fn method_not_found_response(id: Value, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": -32601, "message": format!("Method '{method}' not found")},
    })
}

pub fn create_sdk_mcp_server(
    name: impl Into<String>,
    version: impl Into<String>,
    tools: Vec<SdkMcpTool>,
) -> Arc<SdkMcpServer> {
    Arc::new(SdkMcpServer::new(name, version, tools))
}

pub fn tool<T, F, Fut>(
    name: impl Into<String>,
    description: impl Into<String>,
    handler: F,
    annotations: Option<ToolAnnotations>,
) -> SdkMcpTool
where
    T: DeserializeOwned + JsonSchema + Send + 'static,
    F: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value>> + Send + 'static,
{
    let schema = serde_json::to_value(schema_for!(T)).unwrap_or_else(|_| {
        json!({
            "type": "object",
            "properties": {}
        })
    });
    let handler = Arc::new(handler);
    let wrapped: SdkMcpToolHandler = Arc::new(move |arguments: Value| {
        let handler = handler.clone();
        async move {
            let args: T = serde_json::from_value(arguments)?;
            handler(args).await
        }
        .boxed()
    });
    SdkMcpTool::new_raw(name, description, schema, wrapped, annotations)
}

fn normalize_tool_result(result: Value) -> Result<Value> {
    let Value::Object(obj) = result else {
        return Err(ClaudeSdkError::Runtime(
            "Tool result must be a JSON object".to_string(),
        ));
    };

    let mut contents = Vec::new();
    if let Some(Value::Array(items)) = obj.get("content") {
        for item in items {
            contents.extend(content_item_to_rmcp(item));
        }
    }
    let is_error = obj
        .get("is_error")
        .or_else(|| obj.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let call_result = if is_error {
        CallToolResult::error(contents)
    } else {
        CallToolResult::success(contents)
    };
    call_result_to_bridge_value(call_result)
}

fn call_result_to_bridge_value(call_result: CallToolResult) -> Result<Value> {
    let mut value = serde_json::to_value(call_result)?;
    if value.get("isError") == Some(&Value::Bool(false)) {
        if let Value::Object(obj) = &mut value {
            obj.remove("isError");
        }
    }
    Ok(value)
}

fn content_item_to_rmcp(item: &Value) -> Vec<Content> {
    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
    match item_type {
        "text" => item
            .get("text")
            .and_then(Value::as_str)
            .map(|text| vec![Content::text(text.to_string())])
            .unwrap_or_default(),
        "image" => {
            let data = item.get("data").and_then(Value::as_str).unwrap_or_default();
            let mime_type = item
                .get("mimeType")
                .or_else(|| item.get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            vec![Content::image(data.to_string(), mime_type.to_string())]
        }
        "resource_link" => {
            let mut parts = Vec::new();
            for key in ["name", "uri", "description"] {
                if let Some(value) = item
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                {
                    parts.push(value.to_string());
                }
            }
            vec![Content::text(if parts.is_empty() {
                "Resource link".to_string()
            } else {
                parts.join("\n")
            })]
        }
        "resource" => match item
            .get("resource")
            .and_then(|resource| resource.get("text"))
            .and_then(Value::as_str)
        {
            Some(text) => vec![Content::text(text.to_string())],
            None => {
                tracing::warn!("Binary embedded resource cannot be converted to text, skipping");
                Vec::new()
            }
        },
        _ => {
            tracing::warn!(
                "Unsupported content type {:?} in tool result, skipping",
                item_type
            );
            Vec::new()
        }
    }
}

fn value_to_json_object(value: Value) -> JsonObject {
    match value {
        Value::Object(obj) => obj,
        _ => JsonObject::new(),
    }
}

fn error_tool_result(message: String) -> Result<Value> {
    call_result_to_bridge_value(CallToolResult::error(vec![Content::text(message)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, JsonSchema)]
    struct AddArgs {
        a: i64,
        b: i64,
    }

    #[tokio::test]
    async fn sdk_mcp_server_lists_and_calls_tools() {
        let add = tool::<AddArgs, _, _>(
            "add",
            "Add two numbers",
            |args| async move {
                Ok(json!({
                    "content": [{"type": "text", "text": format!("{}", args.a + args.b)}]
                }))
            },
            None,
        );
        let server = create_sdk_mcp_server("calculator", "2.0.0", vec![add]);

        let listed = server
            .handle_json_rpc(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            }))
            .await;
        assert_eq!(listed["result"]["tools"][0]["name"], "add");
        assert_eq!(
            listed["result"]["tools"][0]["description"],
            "Add two numbers"
        );
        assert!(listed["result"]["tools"][0]["inputSchema"]["properties"]["a"].is_object());

        let called = server
            .handle_json_rpc(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {"name": "add", "arguments": {"a": 2, "b": 5}}
            }))
            .await;
        assert_eq!(called["result"]["content"][0]["type"], "text");
        assert_eq!(called["result"]["content"][0]["text"], "7");
        assert!(called["result"].get("isError").is_none());
    }

    #[tokio::test]
    async fn empty_sdk_mcp_server_matches_python_missing_handlers() {
        let server = create_sdk_mcp_server("empty", "1.0.0", vec![]);
        let listed = server
            .handle_json_rpc(json!({
                "jsonrpc": "2.0",
                "id": "list",
                "method": "tools/list"
            }))
            .await;
        assert_eq!(listed["id"], "list");
        assert_eq!(listed["error"]["code"], -32601);
        assert_eq!(listed["error"]["message"], "Method 'tools/list' not found");

        let response = server
            .handle_json_rpc(json!({
                    "jsonrpc": "2.0",
                    "id": "x",
                    "method": "tools/call",
                    "params": {"name": "missing", "arguments": {}}
            }))
            .await;
        assert_eq!(response["id"], "x");
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(
            response["error"]["message"],
            "Method 'tools/call' not found"
        );
    }
}
