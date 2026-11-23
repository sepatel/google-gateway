use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{account_for, compact, path_id, run};
use crate::app::AppState;
use crate::error::{GatewayError, GatewayResult};
use crate::google::Service;
use crate::google::api::Api;

const SEARCH_FIELDS: &str = "names,emailAddresses,phoneNumbers,organizations";
const OTHER_CONTACT_FIELDS: &str = "names,emailAddresses,phoneNumbers";
const PERSON_FIELDS: &str = "names,nicknames,emailAddresses,phoneNumbers,organizations,addresses,birthdays,biographies,urls,relations";

pub fn descriptors(account: &Value) -> Vec<Value> {
    vec![
        json!({
            "name": "contacts_search",
            "description": "Find people by name, email or phone across saved contacts, people Gmail saved automatically from email, and, for Workspace accounts, the organisation directory.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "query": {"type": "string", "description": "Prefix of a name, email address or phone number."},
                    "max_results": {"type": "integer", "description": "Per source, 1-30, default 10."},
                },
                "required": ["query"],
            },
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "contacts_get",
            "description": "Get one saved contact or directory person in full (addresses, birthday, notes, relations). Takes a resource_name from contacts_search that starts with people/.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "resource_name": {"type": "string", "description": "e.g. people/c1234567890"},
                },
                "required": ["resource_name"],
            },
            "annotations": {"readOnlyHint": true},
        }),
    ]
}

pub async fn call(state: &AppState, name: &str, args: Value) -> Option<GatewayResult<Value>> {
    Some(match name {
        "contacts_search" => run(args, |a| search(state, a)).await,
        "contacts_get" => run(args, |a| get(state, a)).await,
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum Source {
    Contacts,
    OtherContacts,
    Directory,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Contacts => "contact",
            Self::OtherContacts => "other_contact",
            Self::Directory => "directory",
        }
    }

    fn service(self) -> Service {
        match self {
            Self::Contacts => Service::Contacts,
            Self::OtherContacts => Service::OtherContacts,
            Self::Directory => Service::Directory,
        }
    }
}

#[derive(Deserialize)]
struct SearchArgs {
    account: Option<String>,
    query: String,
    max_results: Option<u32>,
}

async fn search(state: &AppState, args: SearchArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.account, Service::Contacts).await?;
    let page_size = args.max_results.unwrap_or(10).clamp(1, 30).to_string();
    // Consumer accounts have no organisation directory to search.
    let has_directory = !matches!(
        account.rsplit_once('@').map(|(_, domain)| domain),
        Some("gmail.com" | "googlemail.com")
    );

    let mut sources = vec![Source::Contacts, Source::OtherContacts];
    if has_directory {
        sources.push(Source::Directory);
    }
    let mut warnings = Vec::new();
    let mut granted = Vec::new();
    for source in sources {
        if state.store.has_scope(&account, source.service()).await {
            granted.push(source);
        } else {
            warnings.push(format!(
                "{} not searched: re-link the account to grant it",
                source.label()
            ));
        }
    }

    warm_up(state, &account, &granted).await;
    let results = futures::future::join_all(
        granted
            .iter()
            .map(|&source| search_source(state, &account, source, &args.query, &page_size)),
    )
    .await;

    // Saved contacts come first, so a person found in several places keeps their richest entry.
    let mut seen = HashSet::new();
    let mut people = Vec::new();
    for (source, result) in granted.iter().zip(results) {
        match result {
            Ok(found) => {
                for person in found {
                    let key = person["emails"][0]
                        .as_str()
                        .map(str::to_lowercase)
                        .or_else(|| person["resource_name"].as_str().map(str::to_string));
                    if key.is_none_or(|k| seen.insert(k)) {
                        people.push(person);
                    }
                }
            }
            Err(e) => warnings.push(format!("{} search failed: {e}", source.label())),
        }
    }

    let mut response = json!({ "account": account, "people": people });
    if !warnings.is_empty() {
        response["warnings"] = json!(warnings);
    }
    Ok(response)
}

/// Google serves contact searches from a cache that an empty query refreshes; without it,
/// the first searches can miss recent contacts.
async fn warm_up(state: &AppState, account: &str, sources: &[Source]) {
    if !state
        .contacts_warmed
        .lock()
        .await
        .insert(account.to_string())
    {
        return;
    }
    for source in sources {
        let request = match source {
            Source::Contacts => Some(("people:searchContacts", SEARCH_FIELDS)),
            Source::OtherContacts => Some(("otherContacts:search", OTHER_CONTACT_FIELDS)),
            Source::Directory => None,
        };
        if let Some((method, fields)) = request {
            let query = [("query", ""), ("readMask", fields)];
            if let Err(e) = state
                .google
                .get(account, Api::People, &[method], &query)
                .await
            {
                tracing::debug!(error = %e, method, "contacts warm-up failed");
            }
        }
    }
}

async fn search_source(
    state: &AppState,
    account: &str,
    source: Source,
    query: &str,
    page_size: &str,
) -> GatewayResult<Vec<Value>> {
    let (method, fields, list_key) = match source {
        Source::Contacts => ("people:searchContacts", SEARCH_FIELDS, "results"),
        Source::OtherContacts => ("otherContacts:search", OTHER_CONTACT_FIELDS, "results"),
        Source::Directory => ("people:searchDirectoryPeople", SEARCH_FIELDS, "people"),
    };
    let mut params = vec![
        ("query", query),
        ("readMask", fields),
        ("pageSize", page_size),
    ];
    if let Source::Directory = source {
        params.push(("sources", "DIRECTORY_SOURCE_TYPE_DOMAIN_PROFILE"));
        params.push(("sources", "DIRECTORY_SOURCE_TYPE_DOMAIN_CONTACT"));
    }
    let response = state
        .google
        .get(account, Api::People, &[method], &params)
        .await?;
    Ok(response[list_key]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| {
            let person = if entry["person"].is_object() {
                &entry["person"]
            } else {
                entry
            };
            let mut rendered = summarize(person);
            rendered["source"] = json!(source.label());
            rendered
        })
        .collect())
}

#[derive(Deserialize)]
struct GetArgs {
    account: Option<String>,
    resource_name: String,
}

async fn get(state: &AppState, args: GetArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.account, Service::Contacts).await?;
    let Some(id) = args.resource_name.strip_prefix("people/") else {
        return Err(GatewayError::BadRequest(
            "resource_name must start with people/; other contacts have no further details".into(),
        ));
    };
    let person = state
        .google
        .get(
            &account,
            Api::People,
            &["people", path_id("contact ID", id)?],
            &[("personFields", PERSON_FIELDS)],
        )
        .await?;

    let mut rendered = summarize(&person);
    let values = |field: &str, key: &str| -> Vec<Value> {
        person[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v[key].as_str().map(|s| json!(s)))
            .collect()
    };
    let birthday = person["birthdays"]
        .as_array()
        .and_then(|b| b.first())
        .and_then(|b| {
            let d = &b["date"];
            let (month, day) = (d["month"].as_u64()?, d["day"].as_u64()?);
            Some(match d["year"].as_u64() {
                Some(year) => format!("{year:04}-{month:02}-{day:02}"),
                None => format!("--{month:02}-{day:02}"),
            })
        });
    let relations: Vec<Value> = person["relations"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| compact(json!({ "person": r["person"], "type": r["formattedType"] })))
        .collect();
    let details = compact(json!({
        "nicknames": non_empty(values("nicknames", "value")),
        "addresses": non_empty(values("addresses", "formattedValue")),
        "urls": non_empty(values("urls", "value")),
        "notes": person["biographies"][0]["value"],
        "birthday": birthday,
        "relations": non_empty(relations),
    }));
    if let (Some(target), Some(extra)) = (rendered.as_object_mut(), details.as_object()) {
        target.extend(extra.clone());
    }
    rendered["account"] = json!(account);
    Ok(rendered)
}

fn summarize(person: &Value) -> Value {
    let list = |field: &str| -> Vec<Value> {
        person[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v["value"].as_str().map(|s| json!(s)))
            .collect()
    };
    let organization = &person["organizations"][0];
    compact(json!({
        "resource_name": person["resourceName"],
        "name": person["names"][0]["displayName"],
        "emails": non_empty(list("emailAddresses")),
        "phones": non_empty(list("phoneNumbers")),
        "organization": organization["name"],
        "title": organization["title"],
    }))
}

fn non_empty(values: Vec<Value>) -> Value {
    if values.is_empty() {
        Value::Null
    } else {
        Value::Array(values)
    }
}
