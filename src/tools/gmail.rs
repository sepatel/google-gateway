use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{account_for, run};
use crate::app::AppState;
use crate::error::{GatewayError, GatewayResult};
use crate::google::Service;
use crate::google::api::Api;
use crate::mail::{header, render_message};

const MESSAGE_BODY_LIMIT: usize = 50_000;
const THREAD_BODY_BUDGET: usize = 100_000;

pub fn descriptors(account: &Value) -> Vec<Value> {
    vec![
        json!({
            "name": "gmail_search_threads",
            "description": "Search email threads using Gmail search syntax (from:, to:, subject:, label:, is:unread, has:attachment, after:YYYY/MM/DD, before:, newer_than:2d, OR, -term, \"exact phrase\"). Returns thread summaries; use gmail_get_thread for full content.",
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
        }),
        json!({
            "name": "gmail_get_thread",
            "description": "Get every message in an email thread, oldest first, with plain-text bodies. Bodies of the oldest messages are omitted when a long thread exceeds the size budget.",
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
        }),
        json!({
            "name": "gmail_get_message",
            "description": "Get one email message with its plain-text body and attachment list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "message_id": {"type": "string"},
                },
                "required": ["message_id"],
            },
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "gmail_list_labels",
            "description": "List the mailbox's labels with their IDs.",
            "inputSchema": {"type": "object", "properties": {"account": account}},
            "annotations": {"readOnlyHint": true},
        }),
    ]
}

pub async fn call(state: &AppState, name: &str, args: Value) -> Option<GatewayResult<Value>> {
    Some(match name {
        "gmail_search_threads" => run(args, |a| search_threads(state, a)).await,
        "gmail_get_thread" => run(args, |a| get_thread(state, a)).await,
        "gmail_get_message" => run(args, |a| get_message(state, a)).await,
        "gmail_list_labels" => run(args, |a| list_labels(state, a)).await,
        _ => return None,
    })
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
    let account = account_for(state, args.account, Service::Gmail).await?;
    let max_results = args.max_results.unwrap_or(20).clamp(1, 50).to_string();
    let mut query = vec![("q", args.query.as_str()), ("maxResults", &max_results)];
    if let Some(token) = args.page_token.as_deref() {
        query.push(("pageToken", token));
    }
    let list = state
        .google
        .get(&account, Api::Gmail, &["users", "me", "threads"], &query)
        .await?;

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
                let path = ["users", "me", "threads", gmail_id(&id)?];
                let metadata = [
                    ("format", "metadata"),
                    ("metadataHeaders", "Subject"),
                    ("metadataHeaders", "From"),
                    ("metadataHeaders", "Date"),
                ];
                match state
                    .google
                    .get(account, Api::Gmail, &path, &metadata)
                    .await
                {
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
    let account = account_for(state, args.account, Service::Gmail).await?;
    let path = ["users", "me", "threads", gmail_id(&args.thread_id)?];
    let thread = state
        .google
        .get(&account, Api::Gmail, &path, &[("format", "full")])
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
    let account = account_for(state, args.account, Service::Gmail).await?;
    let path = ["users", "me", "messages", gmail_id(&args.message_id)?];
    let message = state
        .google
        .get(&account, Api::Gmail, &path, &[("format", "full")])
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
    let account = account_for(state, args.account, Service::Gmail).await?;
    let response = state
        .google
        .get(&account, Api::Gmail, &["users", "me", "labels"], &[])
        .await?;
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
