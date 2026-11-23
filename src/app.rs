use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, post};
use axum::{Json, Router, middleware};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::auth::{self, Client, ClientToken};
use crate::config::AppConfig;
use crate::crypto::Cipher;
use crate::error::{GatewayError, GatewayResult};
use crate::google::Endpoints;
use crate::google::gmail::Gmail;
use crate::google::tokens::TokenManager;
use crate::linking::{self, PendingLink};
use crate::mcp;
use crate::store::AccountStore;

pub struct AppState {
    pub config: AppConfig,
    pub endpoints: Endpoints,
    pub http: reqwest::Client,
    pub store: Arc<AccountStore>,
    pub tokens: Arc<TokenManager>,
    pub gmail: Gmail,
    pub pending_links: Mutex<HashMap<String, PendingLink>>,
}

impl AppState {
    pub async fn new(config: AppConfig, endpoints: Endpoints) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("cannot build HTTP client")?;
        let store = Arc::new(
            AccountStore::open(config.data_dir.clone(), Cipher::new(&config.encryption_key))
                .await?,
        );
        let tokens = Arc::new(TokenManager::new(
            http.clone(),
            endpoints.clone(),
            config.google.clone(),
            store.clone(),
        ));
        let gmail = Gmail::new(http.clone(), endpoints.clone(), tokens.clone());
        Ok(Arc::new(Self {
            config,
            endpoints,
            http,
            store,
            tokens,
            gmail,
            pending_links: Mutex::new(HashMap::new()),
        }))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let clients: Arc<[ClientToken]> = state.config.clients.clone().into();
    let protected = Router::new()
        .route(
            "/mcp",
            post(mcp::handle_all).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .route(
            "/mcp/{account}",
            post(mcp::handle_account).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .route("/accounts", get(list_accounts))
        .route("/accounts/link", post(start_link))
        .route("/accounts/{email}", delete(unlink_account))
        .route_layer(middleware::from_fn_with_state(
            clients,
            auth::require_client,
        ));

    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/oauth/google/callback", get(oauth_callback))
        .merge(protected)
        .with_state(state)
}

async fn list_accounts(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({ "accounts": state.store.list().await }))
}

async fn start_link(
    State(state): State<Arc<AppState>>,
    axum::Extension(client): axum::Extension<Client>,
) -> GatewayResult<Json<Value>> {
    linking::start(&state, &client.0).await.map(Json)
}

async fn unlink_account(
    State(state): State<Arc<AppState>>,
    axum::Extension(client): axum::Extension<Client>,
    Path(email): Path<String>,
) -> GatewayResult<StatusCode> {
    if !state.store.remove(&email).await? {
        return Err(GatewayError::NotFound(format!("{email} is not linked")));
    }
    state.tokens.invalidate(&email.to_lowercase()).await;
    tracing::info!(client = %client.0, account = %email, "account unlinked");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct CallbackParams {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

async fn oauth_callback(
    State(state): State<Arc<AppState>>,
    Query(params): Query<CallbackParams>,
) -> impl IntoResponse {
    let outcome = match (params.error, params.state, params.code) {
        (Some(error), _, _) => Err(GatewayError::BadRequest(format!(
            "Google sign-in was not completed: {error}"
        ))),
        (None, Some(link_state), Some(code)) => linking::complete(&state, &link_state, &code).await,
        _ => Err(GatewayError::BadRequest(
            "the callback is missing its state or code".into(),
        )),
    };
    match outcome {
        Ok(email) => (
            StatusCode::OK,
            page(
                "Account linked",
                &format!("{email} is now linked. You can close this tab."),
            ),
        ),
        Err(e) => {
            tracing::warn!(error = %e, "account link failed");
            let status = match e {
                GatewayError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
                _ => StatusCode::BAD_REQUEST,
            };
            (status, page("Linking failed", &e.to_string()))
        }
    }
}

fn page(title: &str, message: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><meta charset=utf-8><title>{t}</title>\
         <body style=\"font-family:sans-serif;max-width:40em;margin:4em auto\">\
         <h1>{t}</h1><p>{m}</p></body>",
        t = escape(title),
        m = escape(message)
    ))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();

    let config = AppConfig::from_env()?;
    let port = config.port;
    tracing::info!(
        redirect_uri = %config.redirect_uri(),
        clients = config.clients.len(),
        "starting google-gateway"
    );
    let state = AppState::new(config, Endpoints::default()).await?;

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("cannot listen on port {port}"))?;
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server failed")
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing a SIGTERM handler cannot fail on unix");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}
