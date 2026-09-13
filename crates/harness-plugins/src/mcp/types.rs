//! MCP wire types: JSON-RPC 2.0 responses and MCP tool descriptions.

use serde::Deserialize;
use serde_json::{json, Value};

/// One JSON-RPC response (or error) from an MCP server.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcResponse {
    #[serde(default)]
    pub id: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// A tool advertised by `tools/list`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpTool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "default_input_schema", rename = "inputSchema")]
    pub input_schema: Value,
}

fn default_input_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tools_list_result() {
        let resp: JsonRpcResponse = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[
                {"name":"get_forecast","description":"Weather forecast",
                 "inputSchema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}},
                {"name":"noop"}]}}"#,
        )
        .expect("parses");
        assert_eq!(resp.id, json!(7));
        assert!(resp.error.is_none());
        let result = resp.result.expect("result");
        let tools: Vec<McpTool> = serde_json::from_value(result["tools"].clone()).expect("tools");
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "get_forecast");
        assert_eq!(tools[0].input_schema["required"][0], "city");
        // Missing inputSchema defaults to an empty object schema.
        assert!(tools[1].input_schema.is_object());
        assert_eq!(tools[1].input_schema["type"], "object");
    }

    #[test]
    fn parses_rpc_error_response() {
        let resp: JsonRpcResponse = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"Method not found"}}"#,
        )
        .expect("parses");
        let err = resp.error.expect("error");
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }
}
