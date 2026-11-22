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

        let public_url = Url::parse(required("GMAIL_GATEWAY_PUBLIC_URL")?.trim_end_matches('/'))
            .context("GMAIL_GATEWAY_PUBLIC_URL must be an absolute URL")?;
        if public_url.scheme() != "https" && public_url.host_str() != Some("localhost") {
            bail!("GMAIL_GATEWAY_PUBLIC_URL must use https (http is only accepted for localhost)");
        }

        let port = match lookup("GMAIL_GATEWAY_PORT") {
            Some(p) => p
                .parse()
                .context("GMAIL_GATEWAY_PORT must be a port number")?,
            None => 47613,
        };

        Ok(Self {
            google: GoogleClient {
                client_id: required("GMAIL_GATEWAY_GOOGLE_CLIENT_ID")?,
                client_secret: required("GMAIL_GATEWAY_GOOGLE_CLIENT_SECRET")?,
            },
            public_url,
            clients: parse_clients(&required("GMAIL_GATEWAY_CLIENTS")?)?,
            encryption_key: parse_key(&required("GMAIL_GATEWAY_ENCRYPTION_KEY")?)?,
            data_dir: lookup("GMAIL_GATEWAY_DATA_DIR")
                .map_or_else(|| PathBuf::from("/data"), PathBuf::from),
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
                .with_context(|| "GMAIL_GATEWAY_CLIENTS entries must look like name=token")?;
            let (name, token) = (name.trim(), token.trim());
            if name.is_empty() {
                bail!("GMAIL_GATEWAY_CLIENTS has an entry with no name");
            }
            // Short tokens make a public endpoint guessable.
            if token.len() < 32 {
                bail!("GMAIL_GATEWAY_CLIENTS token for {name} must be at least 32 characters");
            }
            Ok(ClientToken::new(name, token))
        })
        .collect::<Result<Vec<_>>>()?;

    if clients.is_empty() {
        bail!("GMAIL_GATEWAY_CLIENTS must name at least one client");
    }
    Ok(clients)
}

fn parse_key(raw: &str) -> Result<[u8; 32]> {
    STANDARD
        .decode(raw.trim())
        .context("GMAIL_GATEWAY_ENCRYPTION_KEY must be base64")?
        .try_into()
        .map_err(|_| {
            anyhow::anyhow!("GMAIL_GATEWAY_ENCRYPTION_KEY must decode to exactly 32 bytes")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(overrides: &[(&str, &str)]) -> Result<AppConfig> {
        let mut vars: HashMap<String, String> = [
            ("GMAIL_GATEWAY_GOOGLE_CLIENT_ID", "id"),
            ("GMAIL_GATEWAY_GOOGLE_CLIENT_SECRET", "secret"),
            ("GMAIL_GATEWAY_PUBLIC_URL", "https://mail.example.com/"),
            (
                "GMAIL_GATEWAY_CLIENTS",
                "opencode=0123456789abcdef0123456789abcdef",
            ),
            (
                "GMAIL_GATEWAY_ENCRYPTION_KEY",
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
        assert!(env(&[("GMAIL_GATEWAY_PUBLIC_URL", "http://mail.example.com")]).is_err());
        assert!(env(&[("GMAIL_GATEWAY_PUBLIC_URL", "http://localhost:47613")]).is_ok());
    }

    #[test]
    fn rejects_short_client_tokens() {
        assert!(env(&[("GMAIL_GATEWAY_CLIENTS", "opencode=short")]).is_err());
    }
}
