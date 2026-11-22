use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::app::AppState;
use crate::error::{GatewayError, GatewayResult};
use crate::google::oauth;

const LINK_TTL: Duration = Duration::from_secs(600);

pub struct PendingLink {
    verifier: String,
    client: String,
    created: Instant,
}

pub async fn start(state: &AppState, client: &str) -> GatewayResult<Value> {
    let link_state = oauth::random_token();
    let verifier = oauth::random_token();
    let url = oauth::authorize_url(
        &state.endpoints,
        &state.config.google,
        &state.config.redirect_uri(),
        &link_state,
        &verifier,
    )?;

    let mut pending = state.pending_links.lock().await;
    pending.retain(|_, p| p.created.elapsed() < LINK_TTL);
    pending.insert(
        link_state,
        PendingLink {
            verifier,
            client: client.to_string(),
            created: Instant::now(),
        },
    );
    tracing::info!(client, "account link started");

    Ok(json!({
        "url": url,
        "expires_in_seconds": LINK_TTL.as_secs(),
        "instructions": "Open the URL in a browser and approve access for the Gmail account to link.",
    }))
}

pub async fn complete(state: &AppState, link_state: &str, code: &str) -> GatewayResult<String> {
    let pending = state
        .pending_links
        .lock()
        .await
        .remove(link_state)
        .filter(|p| p.created.elapsed() < LINK_TTL)
        .ok_or_else(|| {
            GatewayError::BadRequest(
                "this sign-in link is unknown, already used, or expired; start linking again"
                    .into(),
            )
        })?;

    let grant = oauth::exchange_code(
        &state.http,
        &state.endpoints,
        &state.config.google,
        &state.config.redirect_uri(),
        code,
        &pending.verifier,
    )
    .await?;
    let refresh_token = grant.refresh_token.ok_or_else(|| {
        GatewayError::BadRequest(
            "Google did not return a refresh token; remove the app's access at \
             https://myaccount.google.com/permissions and link again"
                .into(),
        )
    })?;

    let email = state.gmail.profile_email(&grant.access_token).await?;
    state.store.upsert(&email, &refresh_token).await?;
    state.tokens.invalidate(&email).await;
    tracing::info!(client = %pending.client, account = %email, "account linked");
    Ok(email)
}
