use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use url::Url;

use crate::auth::ClientToken;

#[derive(Debug, Clone)]
pub struct GoogleClient {
    pub client_id: String,
    pub client_secret: String,
}

#[derive(Clone)]
pub struct AppConfig {
    pub google: GoogleClient,
    pub public_url: Url,
    pub clients: Vec<ClientToken>,
    pub encryption_key: [u8; 32],
    pub data_dir: PathBuf,
    pub port: u16,
}

impl AppConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let required = |name: &str| {
            lookup(name)
                .filter(|v| !v.trim().is_empty())
                .with_context(|| format!("{name} must be set"))
        };

        let public_url = Url::parse(required("PUBLIC_URL")?.trim_end_matches('/'))
            .context("PUBLIC_URL must be an absolute URL")?;
        if public_url.scheme() != "https" && public_url.host_str() != Some("localhost") {
            bail!("PUBLIC_URL must use https (http is only accepted for localhost)");
        }

        let port = match lookup("PORT") {
            Some(p) => p.parse().context("PORT must be a port number")?,
            None => 8080,
        };

        Ok(Self {
            google: GoogleClient {
                client_id: required("GOOGLE_CLIENT_ID")?,
                client_secret: required("GOOGLE_CLIENT_SECRET")?,
            },
            public_url,
            clients: parse_clients(&required("GATEWAY_CLIENTS")?)?,
            encryption_key: parse_key(&required("GATEWAY_ENCRYPTION_KEY")?)?,
            data_dir: lookup("DATA_DIR").map_or_else(|| PathBuf::from("/data"), PathBuf::from),
            port,
        })
    }

    pub fn redirect_uri(&self) -> String {
        format!(
            "{}/oauth/google/callback",
            self.public_url.as_str().trim_end_matches('/')
        )
    }
}

fn parse_clients(raw: &str) -> Result<Vec<ClientToken>> {
    let clients = raw
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (name, token) = entry
                .split_once('=')
                .with_context(|| "GATEWAY_CLIENTS entries must look like name=token")?;
            let (name, token) = (name.trim(), token.trim());
            if name.is_empty() {
                bail!("GATEWAY_CLIENTS has an entry with no name");
            }
            // Short tokens make a public endpoint guessable.
            if token.len() < 32 {
                bail!("GATEWAY_CLIENTS token for {name} must be at least 32 characters");
            }
            Ok(ClientToken::new(name, token))
        })
        .collect::<Result<Vec<_>>>()?;

    if clients.is_empty() {
        bail!("GATEWAY_CLIENTS must name at least one client");
    }
    Ok(clients)
}

fn parse_key(raw: &str) -> Result<[u8; 32]> {
    STANDARD
        .decode(raw.trim())
        .context("GATEWAY_ENCRYPTION_KEY must be base64")?
        .try_into()
        .map_err(|_| anyhow::anyhow!("GATEWAY_ENCRYPTION_KEY must decode to exactly 32 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(overrides: &[(&str, &str)]) -> Result<AppConfig> {
        let mut vars: HashMap<String, String> = [
            ("GOOGLE_CLIENT_ID", "id"),
            ("GOOGLE_CLIENT_SECRET", "secret"),
            ("PUBLIC_URL", "https://mail.example.com/"),
            (
                "GATEWAY_CLIENTS",
                "opencode=0123456789abcdef0123456789abcdef",
            ),
            (
                "GATEWAY_ENCRYPTION_KEY",
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in overrides {
            vars.insert(k.to_string(), v.to_string());
        }
        AppConfig::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn redirect_uri_ignores_trailing_slash_on_public_url() {
        assert_eq!(
            env(&[]).unwrap().redirect_uri(),
            "https://mail.example.com/oauth/google/callback"
        );
    }

    #[test]
    fn rejects_plain_http_public_url_outside_localhost() {
        assert!(env(&[("PUBLIC_URL", "http://mail.example.com")]).is_err());
        assert!(env(&[("PUBLIC_URL", "http://localhost:8080")]).is_ok());
    }

    #[test]
    fn rejects_short_client_tokens() {
        assert!(env(&[("GATEWAY_CLIENTS", "opencode=short")]).is_err());
    }
}
