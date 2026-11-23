pub mod api;
pub mod oauth;
pub mod tokens;

use serde::Deserialize;

/// Google endpoints, overridable so tests can point the gateway at a mock server.
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub authorize: String,
    pub token: String,
    pub gmail: String,
    pub calendar: String,
    pub people: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            authorize: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: "https://oauth2.googleapis.com/token".into(),
            gmail: "https://gmail.googleapis.com/gmail/v1".into(),
            calendar: "https://www.googleapis.com/calendar/v3".into(),
            people: "https://people.googleapis.com/v1".into(),
        }
    }
}

/// A Google permission an account can grant, and the scope that carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Gmail,
    Calendar,
    Contacts,
    OtherContacts,
    Directory,
}

impl Service {
    pub const ALL: [Self; 5] = [
        Self::Gmail,
        Self::Calendar,
        Self::Contacts,
        Self::OtherContacts,
        Self::Directory,
    ];

    pub fn scope(self) -> &'static str {
        match self {
            // Full mailbox access, including permanent delete.
            Self::Gmail => "https://mail.google.com/",
            Self::Calendar => "https://www.googleapis.com/auth/calendar.readonly",
            Self::Contacts => "https://www.googleapis.com/auth/contacts.readonly",
            Self::OtherContacts => "https://www.googleapis.com/auth/contacts.other.readonly",
            Self::Directory => "https://www.googleapis.com/auth/directory.readonly",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Gmail => "gmail",
            Self::Calendar => "calendar",
            Self::Contacts => "contacts",
            Self::OtherContacts => "other_contacts",
            Self::Directory => "directory",
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
