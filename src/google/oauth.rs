use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{Endpoints, Service, error_message};
use crate::config::GoogleClient;
use crate::error::{GatewayError, GatewayResult};

#[derive(Debug, Deserialize)]
pub struct TokenGrant {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: u64,
    /// Space-separated scopes the user actually granted, which can be fewer than requested.
    pub scope: Option<String>,
}

pub enum RefreshError {
    /// Google no longer honours the refresh token; only a fresh consent fixes it.
    InvalidGrant,
    Failed(GatewayError),
}

pub fn random_token() -> String {
    let mut buf = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn authorize_url(
    endpoints: &Endpoints,
    client: &GoogleClient,
    redirect_uri: &str,
    state: &str,
    verifier: &str,
) -> GatewayResult<String> {
    let mut url = url::Url::parse(&endpoints.authorize)
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("bad authorize endpoint: {e}")))?;
    let scopes = Service::ALL
        .iter()
        .map(|s| s.scope())
        .collect::<Vec<_>>()
        .join(" ");
    url.query_pairs_mut()
        .append_pair("client_id", &client.client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &scopes)
        .append_pair("include_granted_scopes", "true")
        // Both are needed for Google to issue a refresh token on every link, not just the first.
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent select_account")
        .append_pair("state", state)
        .append_pair("code_challenge", &pkce_challenge(verifier))
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
}

pub async fn exchange_code(
    http: &Client,
    endpoints: &Endpoints,
    client: &GoogleClient,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> GatewayResult<TokenGrant> {
    let response = http
        .post(&endpoints.token)
        .form(&[
            ("client_id", client.client_id.as_str()),
            ("client_secret", client.client_secret.as_str()),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ])
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(GatewayError::BadRequest(format!(
            "Google rejected the authorization code: {}",
            error_message(&body)
        )));
    }
    serde_json::from_str(&body)
        .map_err(|e| GatewayError::Internal(anyhow::anyhow!("unexpected token response: {e}")))
}

pub async fn refresh(
    http: &Client,
    endpoints: &Endpoints,
    client: &GoogleClient,
    refresh_token: &str,
) -> Result<TokenGrant, RefreshError> {
    let response = http
        .post(&endpoints.token)
        .form(&[
            ("client_id", client.client_id.as_str()),
            ("client_secret", client.client_secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await
        .map_err(|e| RefreshError::Failed(e.into()))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| RefreshError::Failed(e.into()))?;
    if status.is_success() {
        return serde_json::from_str(&body).map_err(|e| {
            RefreshError::Failed(GatewayError::Internal(anyhow::anyhow!(
                "unexpected token response: {e}"
            )))
        });
    }
    if body.contains("\"invalid_grant\"") {
        return Err(RefreshError::InvalidGrant);
    }
    Err(RefreshError::Failed(GatewayError::Upstream {
        status: status.as_u16(),
        message: error_message(&body),
    }))
}
