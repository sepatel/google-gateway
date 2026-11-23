use std::sync::Arc;

use reqwest::{Client, Method, StatusCode};
use serde_json::Value;

use super::tokens::TokenManager;
use super::{Endpoints, error_message};
use crate::error::{GatewayError, GatewayResult};

#[derive(Debug, Clone, Copy)]
pub enum Api {
    Gmail,
    Calendar,
    People,
}

pub struct GoogleApi {
    http: Client,
    endpoints: Endpoints,
    tokens: Arc<TokenManager>,
}

impl GoogleApi {
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
        api: Api,
        path: &[&str],
        query: &[(&str, &str)],
    ) -> GatewayResult<Value> {
        self.call(account, Method::GET, api, path, query, None)
            .await
    }

    pub async fn post(
        &self,
        account: &str,
        api: Api,
        path: &[&str],
        body: &Value,
    ) -> GatewayResult<Value> {
        self.call(account, Method::POST, api, path, &[], Some(body))
            .await
    }

    pub async fn gmail_address(&self, access_token: &str) -> GatewayResult<String> {
        let profile = self
            .send(
                access_token,
                Method::GET,
                Api::Gmail,
                &["users", "me", "profile"],
                &[],
                None,
            )
            .await?;
        profile["emailAddress"]
            .as_str()
            .map(str::to_lowercase)
            .ok_or_else(|| {
                GatewayError::Internal(anyhow::anyhow!("Gmail profile has no emailAddress"))
            })
    }

    async fn call(
        &self,
        account: &str,
        method: Method,
        api: Api,
        path: &[&str],
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> GatewayResult<Value> {
        let token = self.tokens.access_token(account).await?;
        match self
            .send(&token, method.clone(), api, path, query, body)
            .await
        {
            // A cached token can be revoked before it expires; retry once with a fresh one.
            Err(GatewayError::Upstream { status: 401, .. }) => {
                self.tokens.invalidate(account).await;
                let token = self.tokens.access_token(account).await?;
                self.send(&token, method, api, path, query, body).await
            }
            other => other,
        }
    }

    async fn send(
        &self,
        access_token: &str,
        method: Method,
        api: Api,
        path: &[&str],
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> GatewayResult<Value> {
        let base = match api {
            Api::Gmail => &self.endpoints.gmail,
            Api::Calendar => &self.endpoints.calendar,
            Api::People => &self.endpoints.people,
        };
        let mut url = url::Url::parse(base)
            .map_err(|e| GatewayError::Internal(anyhow::anyhow!("bad endpoint {base}: {e}")))?;
        // Pushing segments percent-encodes them, so an ID can never add a path level.
        url.path_segments_mut()
            .map_err(|()| GatewayError::Internal(anyhow::anyhow!("endpoint {base} has no path")))?
            .extend(path);

        let mut request = self
            .http
            .request(method, url)
            .bearer_auth(access_token)
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        if status == StatusCode::NOT_FOUND {
            return Err(GatewayError::NotFound(error_message(&text)));
        }
        if !status.is_success() {
            return Err(GatewayError::Upstream {
                status: status.as_u16(),
                message: error_message(&text),
            });
        }
        serde_json::from_str(&text)
            .map_err(|e| GatewayError::Internal(anyhow::anyhow!("Google returned bad JSON: {e}")))
    }
}
