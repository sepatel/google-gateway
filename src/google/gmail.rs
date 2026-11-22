use std::sync::Arc;

use reqwest::{Client, StatusCode};
use serde_json::Value;

use super::tokens::TokenManager;
use super::{Endpoints, error_message};
use crate::error::{GatewayError, GatewayResult};

pub struct Gmail {
    http: Client,
    endpoints: Endpoints,
    tokens: Arc<TokenManager>,
}

impl Gmail {
    pub fn new(http: Client, endpoints: Endpoints, tokens: Arc<TokenManager>) -> Self {
        Self {
            http,
            endpoints,
            tokens,
        }
    }

    pub async fn get(
        &self,
        account: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> GatewayResult<Value> {
        let token = self.tokens.access_token(account).await?;
        match self.send(&token, path, query).await {
            // A cached token can be revoked before it expires; retry once with a fresh one.
            Err(GatewayError::Upstream { status: 401, .. }) => {
                self.tokens.invalidate(account).await;
                let token = self.tokens.access_token(account).await?;
                self.send(&token, path, query).await
            }
            other => other,
        }
    }

    pub async fn profile_email(&self, access_token: &str) -> GatewayResult<String> {
        let profile = self.send(access_token, "profile", &[]).await?;
        profile["emailAddress"]
            .as_str()
            .map(str::to_lowercase)
            .ok_or_else(|| {
                GatewayError::Internal(anyhow::anyhow!("Gmail profile has no emailAddress"))
            })
    }

    async fn send(
        &self,
        access_token: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> GatewayResult<Value> {
        let url = format!("{}/users/me/{path}", self.endpoints.gmail);
        let response = self
            .http
            .get(url)
            .bearer_auth(access_token)
            .query(query)
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if status == StatusCode::NOT_FOUND {
            return Err(GatewayError::NotFound(error_message(&body)));
        }
        if !status.is_success() {
            return Err(GatewayError::Upstream {
                status: status.as_u16(),
                message: error_message(&body),
            });
        }
        serde_json::from_str(&body)
            .map_err(|e| GatewayError::Internal(anyhow::anyhow!("Gmail returned bad JSON: {e}")))
    }
}
