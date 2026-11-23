use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct ClientToken {
    name: String,
    digest: [u8; 32],
}

impl ClientToken {
    pub fn new(name: &str, token: &str) -> Self {
        Self {
            name: name.to_string(),
            digest: Sha256::digest(token.as_bytes()).into(),
        }
    }
}

impl std::fmt::Debug for ClientToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientToken")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub struct Client(pub String);

pub fn authenticate(clients: &[ClientToken], authorization: Option<&str>) -> Option<Client> {
    let token = authorization?.strip_prefix("Bearer ")?.trim();
    // Compare digests so every comparison is the same length and constant time.
    let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    clients
        .iter()
        .find(|c| bool::from(c.digest.ct_eq(&digest)))
        .map(|c| Client(c.name.clone()))
}

pub async fn require_client(
    State(clients): State<Arc<[ClientToken]>>,
    mut request: Request,
    next: Next,
) -> Response {
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    match authenticate(&clients, authorization) {
        Some(client) => {
            request.extensions_mut().insert(client);
            next.run(request).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer realm=\"google-gateway\""),
            )],
            "missing or invalid bearer token",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_which_client_presented_the_token() {
        let clients = [
            ClientToken::new("opencode", "token-a"),
            ClientToken::new("post-office", "token-b"),
        ];
        let client = authenticate(&clients, Some("Bearer token-b")).unwrap();
        assert_eq!(client.0, "post-office");
        assert!(authenticate(&clients, Some("Bearer token-c")).is_none());
        assert!(authenticate(&clients, Some("token-a")).is_none());
        assert!(authenticate(&clients, None).is_none());
    }
}
