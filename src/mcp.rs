use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::auth::Client;
use crate::tools::{self, Scope, ToolError};

const SUPPORTED_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i32 = -32_700;
const INVALID_REQUEST: i32 = -32_600;
const METHOD_NOT_FOUND: i32 = -32_601;
const INVALID_PARAMS: i32 = -32_602;

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

pub async fn handle_all(
    State(state): State<Arc<AppState>>,
    Extension(client): Extension<Client>,
    body: Bytes,
) -> Response {
    handle(&state, &client, Scope::All, &body).await
}

pub async fn handle_account(
    State(state): State<Arc<AppState>>,
    Extension(client): Extension<Client>,
    Path(account): Path<String>,
    body: Bytes,
) -> Response {
    let account = account.to_lowercase();
    // Fail the connection, not each call, so a client shows the server as broken.
    if !state.store.list().await.iter().any(|a| a.email == account) {
        return (
            StatusCode::NOT_FOUND,
            format!("{account} is not linked to this gateway"),
        )
            .into_response();
    }
    handle(&state, &client, Scope::Account(account), &body).await
}

async fn handle(state: &AppState, client: &Client, scope: Scope, body: &[u8]) -> Response {
    let request: Request = match serde_json::from_slice(body) {
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
    Json(
        dispatch(
            state,
            &client.0,
            &scope,
            id,
            &request.method,
            request.params,
        )
        .await,
    )
    .into_response()
}

async fn dispatch(
    state: &AppState,
    client: &str,
    scope: &Scope,
    id: Value,
    method: &str,
    params: Value,
) -> Value {
    match method {
        "initialize" => ok(id, initialize(scope, &params)),
        "ping" => ok(id, json!({})),
        "tools/list" => ok(id, json!({ "tools": tools::descriptors(scope) })),
        "tools/call" => call_tool(state, client, scope, id, params).await,
        other => err(id, METHOD_NOT_FOUND, format!("unknown method: {other}")),
    }
}

fn initialize(scope: &Scope, params: &Value) -> Value {
    let requested = params["protocolVersion"].as_str().unwrap_or_default();
    let version = SUPPORTED_VERSIONS
        .into_iter()
        .find(|v| *v == requested)
        .unwrap_or(SUPPORTED_VERSIONS[0]);
    let (name, instructions) = match scope {
        Scope::All => (
            "google-gateway".to_string(),
            "Gmail access for every account linked to this gateway. Use search_threads with \
             Gmail search syntax to find mail, then get_thread or get_message to read it. Pass \
             `account` when more than one account is linked; list_accounts shows them."
                .to_string(),
        ),
        Scope::Account(email) => (
            format!("google-gateway:{email}"),
            format!(
                "Gmail access for {email}. Use search_threads with Gmail search syntax to find \
                 mail, then get_thread or get_message to read it."
            ),
        ),
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": name, "version": env!("CARGO_PKG_VERSION") },
        "instructions": instructions,
    })
}

async fn call_tool(
    state: &AppState,
    client: &str,
    scope: &Scope,
    id: Value,
    params: Value,
) -> Value {
    let Some(name) = params["name"].as_str() else {
        return err(id, INVALID_PARAMS, "tools/call needs params.name");
    };
    let arguments = match &params["arguments"] {
        Value::Null => json!({}),
        args => args.clone(),
    };

    let started = Instant::now();
    let outcome = tools::call(state, client, scope, name, arguments).await;
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
            initialize(&Scope::All, &json!({"protocolVersion": "2025-03-26"}))["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(
            initialize(&Scope::All, &json!({"protocolVersion": "1999-01-01"}))["protocolVersion"],
            SUPPORTED_VERSIONS[0]
        );
    }
}
