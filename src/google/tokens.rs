use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use tokio::sync::Mutex;

use super::Endpoints;
use super::oauth::{self, RefreshError};
use crate::config::GoogleClient;
use crate::error::{GatewayError, GatewayResult};
use crate::store::AccountStore;

struct Cached {
    token: String,
    expires_at: Instant,
}

pub struct TokenManager {
    http: Client,
    endpoints: Endpoints,
    client: GoogleClient,
    store: Arc<AccountStore>,
    cache: Mutex<HashMap<String, Cached>>,
}

impl TokenManager {
    pub fn new(
        http: Client,
        endpoints: Endpoints,
        client: GoogleClient,
        store: Arc<AccountStore>,
    ) -> Self {
        Self {
            http,
            endpoints,
            client,
            store,
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub async fn access_token(&self, email: &str) -> GatewayResult<String> {
        let now = Instant::now();
        if let Some(cached) = self.cache.lock().await.get(email)
            && cached.expires_at > now + Duration::from_secs(60)
        {
            return Ok(cached.token.clone());
        }

        let refresh_token = self
            .store
            .refresh_token(email)
            .await?
            .ok_or_else(|| GatewayError::NotFound(format!("{email} is not linked")))?;

        let grant =
            match oauth::refresh(&self.http, &self.endpoints, &self.client, &refresh_token).await {
                Ok(grant) => grant,
                Err(RefreshError::InvalidGrant) => return Err(GatewayError::Relink(email.into())),
                Err(RefreshError::Failed(e)) => return Err(e),
            };

        self.cache.lock().await.insert(
            email.to_string(),
            Cached {
                token: grant.access_token.clone(),
                expires_at: now + Duration::from_secs(grant.expires_in),
            },
        );
        Ok(grant.access_token)
    }

    pub async fn invalidate(&self, email: &str) {
        self.cache.lock().await.remove(email);
    }
}
