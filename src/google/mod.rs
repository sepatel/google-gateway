pub mod gmail;
pub mod oauth;
pub mod tokens;

use serde::Deserialize;

/// Google endpoints, overridable so tests can point the gateway at a mock server.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub gmail: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            gmail: "https://gmail.googleapis.com/gmail/v1".into(),
        }
    }
}

#[derive(Deserialize)]
struct GoogleErrorBody {
    error: GoogleErrorDetail,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum GoogleErrorDetail {
    Api { message: String },
    OAuth(String),
}

#[derive(Deserialize)]
struct OAuthErrorBody {
    error_description: Option<String>,
}

fn error_message(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<GoogleErrorBody>(body) {
        return match parsed.error {
            GoogleErrorDetail::Api { message } => message,
            GoogleErrorDetail::OAuth(code) => {
                match serde_json::from_str::<OAuthErrorBody>(body)
                    .ok()
                    .and_then(|b| b.error_description)
                {
                    Some(description) => format!("{code}: {description}"),
                    None => code,
                }
            }
        };
    }
    body.chars().take(300).collect()
}
