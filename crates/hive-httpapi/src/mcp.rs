//! MCP over HTTP: one POST endpoint speaking JSON-RPC 2.0, stateless, on the
//! same router as everything else, so it is reachable over the unix socket as
//! well as the port (invariant 13). The server it fronts is `hive_mcp::Server`,
//! which owns the rule that tools/list and tools/call agree; this module only
//! maps the wire.
//!
//! Authentication is the platform's, not MCP's: the credential arrives the way
//! every other request's does and fails with THE 401. A client that has no
//! credential learns nothing, including that this endpoint exists.

use axum::body::Body;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use hive_httpauth::Authed;
use hive_mcp::McpError;
use http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;

/// The protocol revision answered to `initialize`. The client proposes one;
/// this server speaks this one and says so, as the spec asks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// A JSON-RPC body has to be bounded like any other body a stranger can send.
const MAX_BODY: usize = 1 << 20;

#[derive(Deserialize)]
struct RpcRequest {
    #[serde(default)]
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    #[serde(default)]
    method: String,
    #[serde(default)]
    params: Value,
}

fn rpc_result(id: Value, result: Value) -> Response {
    axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
}

fn rpc_error(id: Value, code: i64, message: impl Into<String>) -> Response {
    axum::Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message.into()},
    }))
    .into_response()
}

/// POST /mcp.
pub(crate) async fn rpc(State(s): State<AppState>, Authed(cred): Authed, body: Body) -> Response {
    let Some(server) = s.mcp.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(bytes) = axum::body::to_bytes(body, MAX_BODY).await else {
        return rpc_error(Value::Null, -32600, "request too large");
    };
    let req: RpcRequest = match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => return rpc_error(Value::Null, -32700, format!("parse error: {e}")),
    };
    if req.jsonrpc != "2.0" {
        return rpc_error(
            req.id.unwrap_or(Value::Null),
            -32600,
            "jsonrpc must be \"2.0\"",
        );
    }
    // A notification has no id and expects no body. The only one a client
    // sends us is `notifications/initialized`; anything else is accepted and
    // ignored, which is what the spec says to do with an unknown notification.
    if req.id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }
    let id = req.id.unwrap_or(Value::Null);

    match req.method.as_str() {
        "initialize" => rpc_result(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "hive-sandbox", "version": s.version},
            }),
        ),
        "ping" => rpc_result(id, json!({})),
        "tools/list" => match server.list_tools(&cred).await {
            Ok(tools) => {
                let tools: Vec<Value> = tools
                    .into_iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema
                                .map(Value::Object)
                                .unwrap_or_else(|| json!({"type": "object"})),
                        })
                    })
                    .collect();
                rpc_result(id, json!({"tools": tools}))
            }
            Err(e) => rpc_error(id, -32603, e.to_string()),
        },
        "tools/call" => {
            let name = req.params.get("name").and_then(Value::as_str).unwrap_or("");
            if name.is_empty() {
                return rpc_error(id, -32602, "params.name is required");
            }
            let args = req
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let input = serde_json::to_vec(&args).unwrap_or_default();
            match server.call_tool(&cred, name, input).await {
                Ok(res) => {
                    let text = String::from_utf8_lossy(&res.output).into_owned();
                    // The output is JSON by the ABI's contract; when it parses
                    // to an object it also travels as structuredContent, which
                    // is the field a typed client reads.
                    let structured = serde_json::from_slice::<Value>(&res.output)
                        .ok()
                        .filter(Value::is_object);
                    let mut result = json!({
                        "content": [{"type": "text", "text": text}],
                        "isError": false,
                        // Trust is structural in the ABI (invariant 12) and
                        // it does not stop being so at the wire: a client that
                        // feeds this back to a model can see what it is
                        // feeding.
                        "_meta": {"trust": res.trust.as_str(), "taintedBy": res.tainted_by},
                    });
                    if let Some(sc) = structured {
                        result["structuredContent"] = sc;
                    }
                    rpc_result(id, result)
                }
                // Not yours and does not exist are one answer (hive_mcp says
                // why), and on the wire that answer is "invalid params".
                Err(McpError::UnknownTool(_)) | Err(McpError::MalformedName(_)) => {
                    rpc_error(id, -32602, format!("unknown tool: {name}"))
                }
                // The tool ran, or tried to, and said no: a tool result with
                // isError, which is how MCP distinguishes "your call failed"
                // from "the protocol failed".
                Err(McpError::Dispatch(_, msg)) => rpc_result(
                    id,
                    json!({
                        "content": [{"type": "text", "text": msg}],
                        "isError": true,
                    }),
                ),
                Err(e) => rpc_error(id, -32603, e.to_string()),
            }
        }
        other => rpc_error(id, -32601, format!("method not found: {other}")),
    }
}
