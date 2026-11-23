use futures::{StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{account_for, compact, path_id, run};
use crate::app::AppState;
use crate::error::{GatewayError, GatewayResult};
use crate::google::Service;
use crate::google::api::Api;
use crate::mail::{header, render_message};

const MESSAGE_BODY_LIMIT: usize = 50_000;
const THREAD_BODY_BUDGET: usize = 100_000;
const MAX_MESSAGES_PER_CALL: usize = 1000;
const MAX_THREADS_PER_CALL: usize = 100;

fn ids(description: &str) -> Value {
    json!({ "type": "array", "items": {"type": "string"}, "description": description })
}

fn targets_tool(name: &str, description: &str, account: &Value, destructive: bool) -> Value {
    json!({
        "name": name,
        "description": format!("{description} Pass message_ids, thread_ids, or both."),
        "inputSchema": {
            "type": "object",
            "properties": {
                "account": account,
                "message_ids": ids("Message IDs, up to 1000."),
                "thread_ids": ids("Thread IDs, up to 100."),
            },
        },
        "annotations": {"readOnlyHint": false, "destructiveHint": destructive, "idempotentHint": true},
    })
}

fn label_list_visibility() -> Value {
    json!({
        "type": "string",
        "enum": ["show", "hide", "show_if_unread"],
        "description": "Visibility in the label list. Default show.",
    })
}

fn message_list_visibility() -> Value {
    json!({
        "type": "string",
        "enum": ["show", "hide"],
        "description": "Whether the label is shown on messages. Default show.",
    })
}

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
        json!({
            "name": "gmail_modify",
            "description": "Add or remove labels on messages or whole threads. Labels may be given by name or ID. Common uses: archive = remove INBOX, mark read = remove UNREAD, mark unread = add UNREAD, star = add STARRED, important = add IMPORTANT. Use gmail_trash and gmail_mark_spam for trash and spam.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "message_ids": ids("Message IDs, up to 1000."),
                    "thread_ids": ids("Thread IDs, up to 100; applies to every message in each thread."),
                    "add_labels": ids("Label names or IDs to add."),
                    "remove_labels": ids("Label names or IDs to remove."),
                },
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true},
        }),
        targets_tool(
            "gmail_trash",
            "Move messages or whole threads to Trash. Gmail deletes them for good after 30 days; gmail_untrash restores them until then.",
            account,
            true,
        ),
        targets_tool(
            "gmail_untrash",
            "Restore messages or whole threads from Trash.",
            account,
            false,
        ),
        targets_tool(
            "gmail_mark_spam",
            "Report messages or whole threads as spam, moving them out of the inbox into Spam.",
            account,
            false,
        ),
        targets_tool(
            "gmail_mark_not_spam",
            "Move messages or whole threads out of Spam and back into the inbox.",
            account,
            false,
        ),
        json!({
            "name": "gmail_create_label",
            "description": "Create a label. Use / in the name to nest it, e.g. Receipts/2026.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "name": {"type": "string"},
                    "label_list": label_list_visibility(),
                    "message_list": message_list_visibility(),
                },
                "required": ["name"],
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false},
        }),
        json!({
            "name": "gmail_update_label",
            "description": "Rename a label or change where it is shown. System labels cannot be changed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "label": {"type": "string", "description": "Label name or ID."},
                    "new_name": {"type": "string"},
                    "label_list": label_list_visibility(),
                    "message_list": message_list_visibility(),
                },
                "required": ["label"],
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true},
        }),
        json!({
            "name": "gmail_delete_label",
            "description": "Delete a user label. Messages keep everything else; only this label is removed from them. System labels cannot be deleted.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "label": {"type": "string", "description": "Label name or ID."},
                },
                "required": ["label"],
            },
            "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": true},
        }),
    ]
}

pub async fn call(state: &AppState, name: &str, args: Value) -> Option<GatewayResult<Value>> {
    Some(match name {
        "gmail_search_threads" => run(args, |a| search_threads(state, a)).await,
        "gmail_get_thread" => run(args, |a| get_thread(state, a)).await,
        "gmail_get_message" => run(args, |a| get_message(state, a)).await,
        "gmail_list_labels" => run(args, |a| list_labels(state, a)).await,
        "gmail_modify" => run(args, |a| modify(state, a)).await,
        "gmail_trash" => run(args, |a| trash(state, a, true)).await,
        "gmail_untrash" => run(args, |a| trash(state, a, false)).await,
        "gmail_mark_spam" => run(args, |a| mark_spam(state, a, true)).await,
        "gmail_mark_not_spam" => run(args, |a| mark_spam(state, a, false)).await,
        "gmail_create_label" => run(args, |a| create_label(state, a)).await,
        "gmail_update_label" => run(args, |a| update_label(state, a)).await,
        "gmail_delete_label" => run(args, |a| delete_label(state, a)).await,
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

#[derive(Deserialize)]
struct TargetArgs {
    account: Option<String>,
    #[serde(default)]
    message_ids: Vec<String>,
    #[serde(default)]
    thread_ids: Vec<String>,
}

impl TargetArgs {
    fn validate(&self) -> GatewayResult<()> {
        if self.message_ids.is_empty() && self.thread_ids.is_empty() {
            return Err(GatewayError::BadRequest(
                "pass message_ids, thread_ids, or both".into(),
            ));
        }
        if self.message_ids.len() > MAX_MESSAGES_PER_CALL {
            return Err(GatewayError::BadRequest(format!(
                "at most {MAX_MESSAGES_PER_CALL} message_ids per call"
            )));
        }
        if self.thread_ids.len() > MAX_THREADS_PER_CALL {
            return Err(GatewayError::BadRequest(format!(
                "at most {MAX_THREADS_PER_CALL} thread_ids per call"
            )));
        }
        for id in self.message_ids.iter().chain(&self.thread_ids) {
            gmail_id(id)?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Outcome {
    succeeded: usize,
    failed: Vec<Value>,
}

impl Outcome {
    fn record(&mut self, id: &str, result: GatewayResult<Value>) {
        match result {
            Ok(_) => self.succeeded += 1,
            Err(e) => self
                .failed
                .push(json!({ "id": id, "error": e.to_string() })),
        }
    }

    fn into_json(self) -> Value {
        if self.failed.is_empty() {
            json!({ "succeeded": self.succeeded })
        } else {
            json!({ "succeeded": self.succeeded, "failed": self.failed })
        }
    }
}

fn report(account: &str, messages: Option<Outcome>, threads: Option<Outcome>) -> Value {
    compact(json!({
        "account": account,
        "messages": messages.map(Outcome::into_json),
        "threads": threads.map(Outcome::into_json),
    }))
}

/// Run one request per ID, a few at a time, recording each result.
async fn per_item<F, Fut>(ids: &[String], request: F) -> Outcome
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = GatewayResult<Value>>,
{
    let results: Vec<(String, GatewayResult<Value>)> = futures::stream::iter(ids.iter().cloned())
        .map(|id| {
            let fut = request(id.clone());
            async move { (id, fut.await) }
        })
        .buffered(5)
        .collect()
        .await;
    let mut outcome = Outcome::default();
    for (id, result) in results {
        outcome.record(&id, result);
    }
    outcome
}

/// Apply label changes to messages in one batch request and to each thread separately.
async fn apply_labels(
    state: &AppState,
    account: &str,
    targets: &TargetArgs,
    add: &[String],
    remove: &[String],
) -> Value {
    let messages = if targets.message_ids.is_empty() {
        None
    } else {
        let body = json!({
            "ids": targets.message_ids,
            "addLabelIds": add,
            "removeLabelIds": remove,
        });
        let result = state
            .google
            .post(
                account,
                Api::Gmail,
                &["users", "me", "messages", "batchModify"],
                &body,
            )
            .await;
        // Gmail applies a batch to every message or to none.
        let mut outcome = Outcome::default();
        match result {
            Ok(_) => outcome.succeeded = targets.message_ids.len(),
            Err(e) => {
                let error = e.to_string();
                outcome.failed = targets
                    .message_ids
                    .iter()
                    .map(|id| json!({ "id": id, "error": error }))
                    .collect();
            }
        }
        Some(outcome)
    };

    let threads = if targets.thread_ids.is_empty() {
        None
    } else {
        let body = json!({ "addLabelIds": add, "removeLabelIds": remove });
        Some(
            per_item(&targets.thread_ids, |id| {
                let body = &body;
                async move {
                    state
                        .google
                        .post(
                            account,
                            Api::Gmail,
                            &["users", "me", "threads", &id, "modify"],
                            body,
                        )
                        .await
                }
            })
            .await,
        )
    };
    report(account, messages, threads)
}

async fn labels(state: &AppState, account: &str) -> GatewayResult<Vec<Value>> {
    let response = state
        .google
        .get(account, Api::Gmail, &["users", "me", "labels"], &[])
        .await?;
    Ok(response["labels"].as_array().cloned().unwrap_or_default())
}

/// Find a label by exact ID, else by case-insensitive name.
fn find_label<'a>(labels: &'a [Value], wanted: &str) -> GatewayResult<&'a Value> {
    labels
        .iter()
        .find(|l| l["id"] == wanted)
        .or_else(|| {
            labels.iter().find(|l| {
                l["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(wanted))
            })
        })
        .ok_or_else(|| {
            GatewayError::BadRequest(format!(
                "no label named {wanted:?}; gmail_list_labels shows them, gmail_create_label makes one"
            ))
        })
}

#[derive(Deserialize)]
struct ModifyArgs {
    #[serde(flatten)]
    targets: TargetArgs,
    #[serde(default)]
    add_labels: Vec<String>,
    #[serde(default)]
    remove_labels: Vec<String>,
}

async fn modify(state: &AppState, args: ModifyArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.targets.account.clone(), Service::Gmail).await?;
    args.targets.validate()?;
    if args.add_labels.is_empty() && args.remove_labels.is_empty() {
        return Err(GatewayError::BadRequest(
            "pass add_labels, remove_labels, or both".into(),
        ));
    }
    let all = labels(state, &account).await?;
    let resolve = |names: &[String]| -> GatewayResult<Vec<String>> {
        names
            .iter()
            .map(|n| Ok(find_label(&all, n)?["id"].as_str().unwrap_or(n).to_string()))
            .collect()
    };
    let add = resolve(&args.add_labels)?;
    let remove = resolve(&args.remove_labels)?;
    Ok(apply_labels(state, &account, &args.targets, &add, &remove).await)
}

async fn mark_spam(state: &AppState, args: TargetArgs, spam: bool) -> GatewayResult<Value> {
    let account = account_for(state, args.account.clone(), Service::Gmail).await?;
    args.validate()?;
    let (spam_label, inbox) = (vec!["SPAM".to_string()], vec!["INBOX".to_string()]);
    let (add, remove) = if spam {
        (spam_label, inbox)
    } else {
        (inbox, spam_label)
    };
    Ok(apply_labels(state, &account, &args, &add, &remove).await)
}

async fn trash(state: &AppState, args: TargetArgs, to_trash: bool) -> GatewayResult<Value> {
    let account = account_for(state, args.account.clone(), Service::Gmail).await?;
    args.validate()?;
    let action = if to_trash { "trash" } else { "untrash" };
    let run_for = |kind: &'static str, ids: &[String]| {
        let account = &account;
        let ids = ids.to_vec();
        async move {
            if ids.is_empty() {
                return None;
            }
            Some(
                per_item(&ids, |id| async move {
                    state
                        .google
                        .post(
                            account,
                            Api::Gmail,
                            &["users", "me", kind, &id, action],
                            &json!({}),
                        )
                        .await
                })
                .await,
            )
        }
    };
    let messages = run_for("messages", &args.message_ids).await;
    let threads = run_for("threads", &args.thread_ids).await;
    Ok(report(&account, messages, threads))
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum LabelListVisibility {
    Show,
    Hide,
    ShowIfUnread,
}

impl LabelListVisibility {
    fn api(self) -> &'static str {
        match self {
            Self::Show => "labelShow",
            Self::Hide => "labelHide",
            Self::ShowIfUnread => "labelShowIfUnread",
        }
    }
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum MessageListVisibility {
    Show,
    Hide,
}

impl MessageListVisibility {
    fn api(self) -> &'static str {
        match self {
            Self::Show => "show",
            Self::Hide => "hide",
        }
    }
}

fn render_label(label: &Value) -> Value {
    compact(json!({ "id": label["id"], "name": label["name"], "type": label["type"] }))
}

#[derive(Deserialize)]
struct CreateLabelArgs {
    account: Option<String>,
    name: String,
    label_list: Option<LabelListVisibility>,
    message_list: Option<MessageListVisibility>,
}

async fn create_label(state: &AppState, args: CreateLabelArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.account, Service::Gmail).await?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(GatewayError::BadRequest("name must not be empty".into()));
    }
    let body = compact(json!({
        "name": name,
        "labelListVisibility": args.label_list.map(LabelListVisibility::api),
        "messageListVisibility": args.message_list.map(MessageListVisibility::api),
    }));
    let label = state
        .google
        .post(&account, Api::Gmail, &["users", "me", "labels"], &body)
        .await?;
    Ok(json!({ "account": account, "label": render_label(&label) }))
}

#[derive(Deserialize)]
struct UpdateLabelArgs {
    account: Option<String>,
    label: String,
    new_name: Option<String>,
    label_list: Option<LabelListVisibility>,
    message_list: Option<MessageListVisibility>,
}

async fn user_label(state: &AppState, account: &str, wanted: &str) -> GatewayResult<String> {
    let all = labels(state, account).await?;
    let label = find_label(&all, wanted)?;
    if label["type"] == "system" {
        return Err(GatewayError::BadRequest(format!(
            "{wanted} is a system label and cannot be changed"
        )));
    }
    label["id"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| GatewayError::Internal(anyhow::anyhow!("label without an id")))
}

async fn update_label(state: &AppState, args: UpdateLabelArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.account, Service::Gmail).await?;
    let id = user_label(state, &account, &args.label).await?;
    let body = compact(json!({
        "name": args.new_name.as_deref().map(str::trim),
        "labelListVisibility": args.label_list.map(LabelListVisibility::api),
        "messageListVisibility": args.message_list.map(MessageListVisibility::api),
    }));
    if body.as_object().is_none_or(serde_json::Map::is_empty) {
        return Err(GatewayError::BadRequest(
            "pass new_name, label_list or message_list".into(),
        ));
    }
    let label = state
        .google
        .patch(
            &account,
            Api::Gmail,
            &["users", "me", "labels", path_id("label ID", &id)?],
            &body,
        )
        .await?;
    Ok(json!({ "account": account, "label": render_label(&label) }))
}

#[derive(Deserialize)]
struct DeleteLabelArgs {
    account: Option<String>,
    label: String,
}

async fn delete_label(state: &AppState, args: DeleteLabelArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.account, Service::Gmail).await?;
    let id = user_label(state, &account, &args.label).await?;
    state
        .google
        .delete(
            &account,
            Api::Gmail,
            &["users", "me", "labels", path_id("label ID", &id)?],
        )
        .await?;
    Ok(json!({ "account": account, "deleted": id }))
}
