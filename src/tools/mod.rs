mod calendar;
mod contacts;
mod gmail;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::app::AppState;
use crate::error::{GatewayError, GatewayResult};
use crate::google::Service;
use crate::linking;

pub enum ToolError {
    UnknownTool(String),
    Failed(GatewayError),
}

impl From<GatewayError> for ToolError {
    fn from(e: GatewayError) -> Self {
        Self::Failed(e)
    }
}

/// Which accounts an MCP endpoint exposes.
pub enum Scope {
    All,
    Account(String),
}

const ALL_SCOPE_ONLY: [&str; 2] = ["list_accounts", "link_account"];

pub fn descriptors(scope: &Scope) -> Value {
    let account = json!({
        "type": "string",
        "description": "Linked account email. Optional when exactly one account is linked."
    });
    let mut tools = vec![
        json!({
            "name": "list_accounts",
            "description": "List the Google accounts linked to this gateway and the services each one granted.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "link_account",
            "description": "Start linking a Google account. Returns a Google sign-in URL for the user to open in a browser; it expires after 10 minutes. Also use this to re-link an account whose authorization was revoked or is missing a service.",
            "inputSchema": {"type": "object", "properties": {}},
        }),
    ];
    tools.extend(gmail::descriptors(&account));
    tools.extend(calendar::descriptors(&account));
    tools.extend(contacts::descriptors(&account));

    if let Scope::Account(_) = scope {
        tools.retain(|t| !ALL_SCOPE_ONLY.iter().any(|n| t["name"] == *n));
        for tool in &mut tools {
            if let Some(props) = tool["inputSchema"]["properties"].as_object_mut() {
                props.remove("account");
            }
        }
    }
    Value::Array(tools)
}

pub async fn call(
    state: &AppState,
    client: &str,
    scope: &Scope,
    name: &str,
    mut args: Value,
) -> Result<Value, ToolError> {
    if let Scope::Account(email) = scope {
        if ALL_SCOPE_ONLY.contains(&name) {
            return Err(ToolError::UnknownTool(name.to_string()));
        }
        let Some(fields) = args.as_object_mut() else {
            return Err(GatewayError::BadRequest("arguments must be an object".into()).into());
        };
        if let Some(requested) = fields.get("account").and_then(Value::as_str)
            && !requested.eq_ignore_ascii_case(email)
        {
            return Err(
                GatewayError::BadRequest(format!("this endpoint only serves {email}")).into(),
            );
        }
        fields.insert("account".into(), json!(email));
    }

    let result = match name {
        "list_accounts" => Ok(json!({ "accounts": state.store.list().await })),
        "link_account" => linking::start(state, client).await,
        _ => {
            let outcome = match name.split_once('_').map(|(service, _)| service) {
                Some("gmail") => gmail::call(state, name, args).await,
                Some("calendar") => calendar::call(state, name, args).await,
                Some("contacts") => contacts::call(state, name, args).await,
                _ => None,
            };
            outcome.ok_or_else(|| ToolError::UnknownTool(name.to_string()))?
        }
    };
    Ok(result?)
}

async fn run<T, F, Fut>(args: Value, tool: F) -> GatewayResult<Value>
where
    T: DeserializeOwned,
    F: FnOnce(T) -> Fut,
    Fut: Future<Output = GatewayResult<Value>>,
{
    let args = serde_json::from_value(args)
        .map_err(|e| GatewayError::BadRequest(format!("bad arguments: {e}")))?;
    tool(args).await
}

/// The linked account a call targets, provided it granted `service`.
async fn account_for(
    state: &AppState,
    requested: Option<String>,
    service: Service,
) -> GatewayResult<String> {
    let linked: Vec<String> = state
        .store
        .list()
        .await
        .into_iter()
        .map(|a| a.email)
        .collect();
    let describe = || {
        if linked.is_empty() {
            "none".to_string()
        } else {
            linked.join(", ")
        }
    };
    let account = match requested {
        Some(email) => {
            let email = email.trim().to_lowercase();
            if !linked.contains(&email) {
                return Err(GatewayError::BadRequest(format!(
                    "{email} is not linked; linked accounts: {}",
                    describe()
                )));
            }
            email
        }
        None if linked.len() == 1 => linked[0].clone(),
        None if linked.is_empty() => {
            return Err(GatewayError::BadRequest(
                "no accounts are linked yet; call link_account first".into(),
            ));
        }
        None => {
            return Err(GatewayError::BadRequest(format!(
                "several accounts are linked, pass account as one of: {}",
                describe()
            )));
        }
    };
    if !state.store.has_scope(&account, service).await {
        return Err(GatewayError::MissingScope {
            account,
            service: service.label(),
        });
    }
    Ok(account)
}

/// Reject IDs that are empty or a dot segment; percent-encoding handles every other character.
fn path_id<'a>(kind: &str, id: &'a str) -> GatewayResult<&'a str> {
    if id.is_empty() || id == "." || id == ".." || id.chars().any(char::is_control) {
        return Err(GatewayError::BadRequest(format!(
            "{id:?} is not a valid {kind}"
        )));
    }
    Ok(id)
}

/// Drop null fields so responses spend no tokens on absent values.
fn compact(mut value: Value) -> Value {
    if let Some(map) = value.as_object_mut() {
        map.retain(|_, v| !v.is_null());
    }
    value
}
