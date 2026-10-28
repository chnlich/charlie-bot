//! The MCP streamable-HTTP JSON-RPC surface: `initialize`, `notifications/initialized`, `ping`, `tools/list` and
//! `tools/call` answer, and every other method is method-not-found.
//!
//! Claude Code 2.1.310 sends `server/discover` before `initialize`, and the method-not-found answer makes it fall back
//! to `initialize`.

use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use charliebot_compat::responses;
use serde_json::{Value, json};

use crate::endpoint::{MCP_SERVER_NAME, WebSearch};
use crate::search;

/// The protocol version answered when the client names none: the version Claude Code 2.1.310 initializes with.
const PROTOCOL_VERSION: &str = "2025-11-25";
const TOOL_NAME: &str = "web_search";
const TOOL_DESCRIPTION: &str = "Search the web and return an answer with its source URLs. Use this tool when the \
                                answer depends on recent events or a URL you do not know. Use WebFetch on a source URL \
                                to read that page in detail.";
const QUERY_DESCRIPTION: &str = "The search query.";
const MAX_BODY_BYTES: usize = 1 << 20;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

pub async fn post(State(server): State<WebSearch>, body: Body) -> Response {
  let body = match to_bytes(body, MAX_BODY_BYTES).await {
    Ok(body) => body,
    Err(error) => {
      return json(&error_response(Value::Null, PARSE_ERROR, format!("the request body is unreadable: {error}")));
    }
  };
  let request: Value = match serde_json::from_slice(&body) {
    Ok(request) => request,
    Err(error) => return json(&error_response(Value::Null, PARSE_ERROR, format!("the request is not JSON: {error}"))),
  };
  let Some(object) = request.as_object() else {
    return json(&error_response(Value::Null, INVALID_REQUEST, "the request must be an object".to_string()));
  };
  let Some(method) = object.get("method").and_then(Value::as_str) else {
    return json(&error_response(Value::Null, INVALID_REQUEST, "the request has no method".to_string()));
  };
  let Some(id) = object.get("id").cloned() else {
    // JSON-RPC answers no notification, so a notification gets an empty 202.
    return StatusCode::ACCEPTED.into_response();
  };
  let result = match method {
    "initialize" => Ok(initialize(object.get("params"))),
    "notifications/initialized" | "notifications/cancelled" => Ok(json!({})),
    "ping" => Ok(json!({})),
    "tools/list" => Ok(tools_list()),
    "tools/call" => Ok(call_tool(server, object.get("params")).await),
    _ => Err((METHOD_NOT_FOUND, "Method not found".to_string())),
  };
  match result {
    Ok(result) => json(&json!({"jsonrpc": "2.0", "id": id, "result": result})),
    Err((code, message)) => json(&error_response(id, code, message)),
  }
}

/// The initialization result, echoing the client's protocol version when it names one.
fn initialize(params: Option<&Value>) -> Value {
  let version =
    params.and_then(|params| params.get("protocolVersion")).and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
  json!({
    "protocolVersion": version,
    "capabilities": {"tools": {}},
    "serverInfo": {"name": MCP_SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
  })
}

fn tools_list() -> Value {
  json!({"tools": [{
    "name": TOOL_NAME,
    "description": TOOL_DESCRIPTION,
    "inputSchema": {
      "type": "object",
      "properties": {"query": {"type": "string", "description": QUERY_DESCRIPTION}},
      "required": ["query"],
    },
  }]})
}

async fn call_tool(server: WebSearch, params: Option<&Value>) -> Value {
  let name = params.and_then(|params| params.get("name")).and_then(Value::as_str).unwrap_or("");
  if name != TOOL_NAME {
    return tool_result(format!("unknown tool: {name}"), true);
  }
  let query =
    params.and_then(|params| params.pointer("/arguments/query")).and_then(Value::as_str).map(str::trim).unwrap_or("");
  if query.is_empty() {
    return tool_result("the web_search tool needs a non-empty query argument".to_string(), true);
  }
  let credentials = match server.credentials.get() {
    Ok(credentials) => credentials,
    Err(error) => return tool_result(format!("reading credentials.yaml failed: {error}"), true),
  };
  let client = match server.client() {
    Ok(client) => client,
    Err(error) => return tool_result(error, true),
  };
  match search::search(&credentials, &client, &server.endpoint, query).await {
    Ok(found) => tool_result(found.render(), false),
    Err(error) => tool_result(error, true),
  }
}

fn tool_result(text: String, is_error: bool) -> Value {
  json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn error_response(id: Value, code: i64, message: String) -> Value {
  json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn json(value: &Value) -> Response {
  responses::json(StatusCode::OK, value)
}
