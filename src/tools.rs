use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::error::{GatewayError, GatewayResult};
use crate::linking;
use crate::mail::{header, render_message};

const MESSAGE_BODY_LIMIT: usize = 50_000;
const THREAD_BODY_BUDGET: usize = 100_000;

pub enum ToolError {
    UnknownTool(String),
    Failed(GatewayError),
}

impl From<GatewayError> for ToolError {
    fn from(e: GatewayError) -> Self {
        Self::Failed(e)
    }
}

pub fn descriptors() -> Value {
    let account = json!({
        "type": "string",
        "description": "Linked account email. Optional when exactly one account is linked."
    });
    json!([
        {
            "name": "list_accounts",
            "description": "List the Gmail accounts linked to this gateway.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "link_account",
            "description": "Start linking a Gmail account. Returns a Google sign-in URL for the user to open in a browser; it expires after 10 minutes. Also use this to re-link an account whose authorization was revoked.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "search_threads",
            "description": "Search email threads using Gmail search syntax (from:, to:, subject:, label:, is:unread, has:attachment, after:YYYY/MM/DD, before:, newer_than:2d, OR, -term, \"exact phrase\"). Returns thread summaries; use get_thread for full content.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "query": {"type": "string", "description": "Gmail search query."},
                    "max_results": {"type": "integer", "description": "1-50, default 20."},
                    "page_token": {"type": "string", "description": "next_page_token from a previous call."},
                },
                "required": ["query"],
            },
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "get_thread",
            "description": "Get every message in a thread, oldest first, with plain-text bodies. Bodies of the oldest messages are omitted when a long thread exceeds the size budget.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "thread_id": {"type": "string"},
                    "include_bodies": {"type": "boolean", "description": "Default true. False returns headers and snippets only."},
                },
                "required": ["thread_id"],
            },
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "get_message",
            "description": "Get one message with its plain-text body and attachment list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "message_id": {"type": "string"},
                },
                "required": ["message_id"],
            },
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "list_labels",
            "description": "List the account's labels with their IDs.",
            "inputSchema": {"type": "object", "properties": {"account": account}},
            "annotations": {"readOnlyHint": true},
        },
    ])
}

pub async fn call(
    state: &AppState,
    client: &str,
    name: &str,
    args: Value,
) -> Result<Value, ToolError> {
    let result = match name {
        "list_accounts" => Ok(json!({ "accounts": state.store.list().await })),
        "link_account" => linking::start(state, client).await,
        "search_threads" => search_threads(state, parse(args)?).await,
        "get_thread" => get_thread(state, parse(args)?).await,
        "get_message" => get_message(state, parse(args)?).await,
        "list_labels" => list_labels(state, parse(args)?).await,
        other => return Err(ToolError::UnknownTool(other.to_string())),
    };
    Ok(result?)
}

fn parse<T: DeserializeOwned>(args: Value) -> GatewayResult<T> {
    serde_json::from_value(args)
        .map_err(|e| GatewayError::BadRequest(format!("bad arguments: {e}")))
}

async fn resolve_account(state: &AppState, requested: Option<String>) -> GatewayResult<String> {
    let linked: Vec<String> = state
        .store
        .list()
        .await
        .into_iter()
        .map(|a| a.email)
        .collect();
    match requested {
        Some(email) => {
            let email = email.trim().to_lowercase();
            if linked.contains(&email) {
                Ok(email)
            } else {
                Err(GatewayError::BadRequest(format!(
                    "{email} is not linked; linked accounts: {}",
                    describe(&linked)
                )))
            }
        }
        None if linked.len() == 1 => Ok(linked[0].clone()),
        None if linked.is_empty() => Err(GatewayError::BadRequest(
            "no accounts are linked yet; call link_account first".into(),
        )),
        None => Err(GatewayError::BadRequest(format!(
            "several accounts are linked, pass account as one of: {}",
            describe(&linked)
        ))),
    }
}

fn describe(accounts: &[String]) -> String {
    if accounts.is_empty() {
        "none".into()
    } else {
        accounts.join(", ")
    }
}

/// Reject IDs that could reshape the request path; Gmail IDs are alphanumeric.
fn gmail_id(id: &str) -> GatewayResult<&str> {
    if !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Ok(id)
    } else {
        Err(GatewayError::BadRequest(format!(
            "{id:?} is not a Gmail ID"
        )))
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    account: Option<String>,
    query: String,
    max_results: Option<u32>,
    page_token: Option<String>,
}

async fn search_threads(state: &AppState, args: SearchArgs) -> GatewayResult<Value> {
    let account = resolve_account(state, args.account).await?;
    let max_results = args.max_results.unwrap_or(20).clamp(1, 50).to_string();
    let mut query = vec![("q", args.query.as_str()), ("maxResults", &max_results)];
    if let Some(token) = args.page_token.as_deref() {
        query.push(("pageToken", token));
    }
    let list = state.gmail.get(&account, "threads", &query).await?;

    let ids: Vec<String> = list["threads"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["id"].as_str().map(str::to_string))
        .collect();

    let threads: Vec<Value> = futures::stream::iter(ids)
        .map(|id| {
            let account = &account;
            async move {
                let path = format!("threads/{}", gmail_id(&id)?);
                let metadata = [
                    ("format", "metadata"),
                    ("metadataHeaders", "Subject"),
                    ("metadataHeaders", "From"),
                    ("metadataHeaders", "Date"),
                ];
                match state.gmail.get(account, &path, &metadata).await {
                    Ok(thread) => Ok(Some(summarize_thread(&thread))),
                    // Deleted between the search and the fetch.
                    Err(GatewayError::NotFound(_)) => Ok(None),
                    Err(e) => Err(e),
                }
            }
        })
        .buffered(5)
        .try_filter_map(|t| async move { Ok(t) })
        .try_collect()
        .await?;

    Ok(json!({
        "account": account,
        "threads": threads,
        "next_page_token": list["nextPageToken"],
    }))
}

fn summarize_thread(thread: &Value) -> Value {
    let messages = thread["messages"].as_array().map_or(&[][..], Vec::as_slice);
    let (Some(first), Some(last)) = (messages.first(), messages.last()) else {
        return json!({ "id": thread["id"] });
    };
    let unread = messages.iter().any(|m| {
        m["labelIds"]
            .as_array()
            .is_some_and(|labels| labels.iter().any(|l| l == "UNREAD"))
    });
    json!({
        "id": thread["id"],
        "message_count": messages.len(),
        "subject": header(&first["payload"], "Subject"),
        "from": header(&last["payload"], "From"),
        "date": header(&last["payload"], "Date"),
        "snippet": last["snippet"],
        "labels": last["labelIds"],
        "unread": unread,
    })
}

#[derive(Deserialize)]
struct ThreadArgs {
    account: Option<String>,
    thread_id: String,
    include_bodies: Option<bool>,
}

async fn get_thread(state: &AppState, args: ThreadArgs) -> GatewayResult<Value> {
    let account = resolve_account(state, args.account).await?;
    let path = format!("threads/{}", gmail_id(&args.thread_id)?);
    let thread = state
        .gmail
        .get(&account, &path, &[("format", "full")])
        .await?;
    let messages = thread["messages"].as_array().map_or(&[][..], Vec::as_slice);
    let include_bodies = args.include_bodies.unwrap_or(true);

    // Spend the budget newest-first so the latest replies always keep their bodies.
    let mut budget = THREAD_BODY_BUDGET;
    let mut rendered: Vec<Value> = messages
        .iter()
        .rev()
        .map(|message| {
            if !include_bodies {
                return render_message(message, None);
            }
            if budget == 0 {
                let mut r = render_message(message, None);
                r["body_omitted"] = json!(true);
                return r;
            }
            let r = render_message(message, Some(budget.min(MESSAGE_BODY_LIMIT)));
            let used = r["body"].as_str().map_or(0, |b| b.chars().count());
            budget = budget.saturating_sub(used);
            r
        })
        .collect();
    rendered.reverse();

    Ok(json!({
        "account": account,
        "id": thread["id"],
        "messages": rendered,
    }))
}

#[derive(Deserialize)]
struct MessageArgs {
    account: Option<String>,
    message_id: String,
}

async fn get_message(state: &AppState, args: MessageArgs) -> GatewayResult<Value> {
    let account = resolve_account(state, args.account).await?;
    let path = format!("messages/{}", gmail_id(&args.message_id)?);
    let message = state
        .gmail
        .get(&account, &path, &[("format", "full")])
        .await?;
    let mut rendered = render_message(&message, Some(MESSAGE_BODY_LIMIT));
    rendered["account"] = json!(account);
    Ok(rendered)
}

#[derive(Deserialize)]
struct LabelsArgs {
    account: Option<String>,
}

async fn list_labels(state: &AppState, args: LabelsArgs) -> GatewayResult<Value> {
    let account = resolve_account(state, args.account).await?;
    let response = state.gmail.get(&account, "labels", &[]).await?;
    let mut labels: Vec<Value> = response["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| json!({ "id": l["id"], "name": l["name"], "type": l["type"] }))
        .collect();
    labels.sort_by_key(|l| {
        (
            l["type"].as_str() != Some("system"),
            l["name"].as_str().unwrap_or_default().to_lowercase(),
        )
    });
    Ok(json!({ "account": account, "labels": labels }))
}
