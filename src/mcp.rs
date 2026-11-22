use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::auth::Client;
use crate::tools::{self, ToolError};

const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i32 = -32_700;
const INVALID_REQUEST: i32 = -32_600;
const METHOD_NOT_FOUND: i32 = -32_601;
const INVALID_PARAMS: i32 = -32_602;

const INSTRUCTIONS: &str = "Gmail access for the accounts linked to this gateway. \
Use search_threads with Gmail search syntax to find mail, then get_thread or get_message to read it. \
Pass `account` when more than one account is linked; list_accounts shows them.";

#[derive(Deserialize)]
struct Request {
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message.into() } })
}

pub async fn handle_post(
    State(state): State<Arc<AppState>>,
    Extension(client): Extension<Client>,
    body: Bytes,
) -> Response {
    let request: Request = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(err(Value::Null, PARSE_ERROR, format!("parse error: {e}"))),
            )
                .into_response();
        }
    };
    if request.jsonrpc.as_deref() != Some("2.0") {
        return (
            StatusCode::BAD_REQUEST,
            Json(err(
                request.id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "jsonrpc must be 2.0",
            )),
        )
            .into_response();
    }
    let Some(id) = request.id.filter(|id| !id.is_null()) else {
        return StatusCode::ACCEPTED.into_response();
    };
    Json(dispatch(&state, &client.0, id, &request.method, request.params).await).into_response()
}

async fn dispatch(state: &AppState, client: &str, id: Value, method: &str, params: Value) -> Value {
    match method {
        "initialize" => ok(id, initialize(&params)),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tools::descriptors() })),
        "tools/call" => call_tool(state, client, id, params).await,
        other => err(id, METHOD_NOT_FOUND, format!("unknown method: {other}")),
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params["protocolVersion"].as_str().unwrap_or_default();
    let version = SUPPORTED_VERSIONS
        .into_iter()
        .find(|v| *v == requested)
        .unwrap_or(SUPPORTED_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "gmail-gateway", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

async fn call_tool(state: &AppState, client: &str, id: Value, params: Value) -> Value {
    let Some(name) = params["name"].as_str() else {
        return err(id, INVALID_PARAMS, "tools/call needs params.name");
    };
    let arguments = match &params["arguments"] {
        Value::Null => json!({}),
        args => args.clone(),
    };

    let started = Instant::now();
    let outcome = tools::call(state, client, name, arguments).await;
    let elapsed_ms = started.elapsed().as_millis();

    match outcome {
        Ok(value) => {
            tracing::info!(client, tool = name, elapsed_ms, "tool call");
            ok(
                id,
                json!({
                    "content": [{ "type": "text", "text": value.to_string() }],
                    "structuredContent": value,
                    "isError": false,
                }),
            )
        }
        Err(ToolError::UnknownTool(tool)) => {
            err(id, INVALID_PARAMS, format!("unknown tool: {tool}"))
        }
        // Tool failures go back as results so the model can read them and adjust.
        Err(ToolError::Failed(e)) => {
            tracing::warn!(client, tool = name, elapsed_ms, error = %e, "tool call failed");
            ok(
                id,
                json!({
                    "content": [{ "type": "text", "text": e.to_string() }],
                    "isError": true,
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiates_the_client_version_or_falls_back_to_latest() {
        assert_eq!(
            initialize(&json!({"protocolVersion": "2025-03-26"}))["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(
            initialize(&json!({"protocolVersion": "1999-01-01"}))["protocolVersion"],
            SUPPORTED_VERSIONS[0]
        );
    }
}
