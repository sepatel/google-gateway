use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::crypto::Cipher;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAccount {
    sealed_refresh_token: String,
    linked_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    accounts: BTreeMap<String, StoredAccount>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountSummary {
    pub email: String,
    pub linked_at: u64,
}

/// Linked Google accounts, keyed by lowercase email, with refresh tokens sealed at rest.
pub struct AccountStore {
    path: PathBuf,
    cipher: Cipher,
    accounts: RwLock<BTreeMap<String, StoredAccount>>,
}

impl AccountStore {
    pub async fn open(data_dir: PathBuf, cipher: Cipher) -> Result<Self> {
        tokio::fs::create_dir_all(&data_dir)
            .await
            .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
        // Probe now so an unwritable folder fails the start, not a sign-in minutes later.
        let probe = data_dir.join(".write-test");
        async {
            tokio::fs::write(&probe, b"").await?;
            tokio::fs::remove_file(&probe).await
        }
        .await
        .with_context(|| {
            format!(
                "cannot write to {}: make sure the host folder exists and is owned by \
                     GMAIL_GATEWAY_UID:GMAIL_GATEWAY_GID",
                data_dir.display()
            )
        })?;
        let path = data_dir.join("accounts.json");
        let accounts = match tokio::fs::read(&path).await {
            Ok(bytes) => {
                serde_json::from_slice::<StoreFile>(&bytes)
                    .with_context(|| format!("{} is corrupt", path.display()))?
                    .accounts
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        // Fail at startup, not on first use, when the key no longer matches the stored data.
        for (email, account) in &accounts {
            cipher
                .open(&account.sealed_refresh_token)
                .with_context(|| format!("cannot unseal the token for {email}"))?;
        }
        Ok(Self {
            path,
            cipher,
            accounts: RwLock::new(accounts),
        })
    }

    pub async fn list(&self) -> Vec<AccountSummary> {
        self.accounts
            .read()
            .await
            .iter()
            .map(|(email, a)| AccountSummary {
                email: email.clone(),
                linked_at: a.linked_at,
            })
            .collect()
    }

    pub async fn refresh_token(&self, email: &str) -> Result<Option<String>> {
        let accounts = self.accounts.read().await;
        accounts
            .get(&email.to_lowercase())
            .map(|a| self.cipher.open(&a.sealed_refresh_token))
            .transpose()
    }

    pub async fn upsert(&self, email: &str, refresh_token: &str) -> Result<()> {
        let mut accounts = self.accounts.write().await;
        let linked_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        accounts.insert(
            email.to_lowercase(),
            StoredAccount {
                sealed_refresh_token: self.cipher.seal(refresh_token)?,
                linked_at,
            },
        );
        self.persist(&accounts).await
    }

    pub async fn remove(&self, email: &str) -> Result<bool> {
        let mut accounts = self.accounts.write().await;
        let removed = accounts.remove(&email.to_lowercase()).is_some();
        if removed {
            self.persist(&accounts).await?;
        }
        Ok(removed)
    }

    async fn persist(&self, accounts: &BTreeMap<String, StoredAccount>) -> Result<()> {
        let body = serde_json::to_vec_pretty(&StoreFile {
            accounts: accounts.clone(),
        })?;
        // Write then rename so a crash mid-write can never truncate the only copy.
        let tmp = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp, body)
            .await
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .with_context(|| format!("cannot replace {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn linked_accounts_survive_a_restart_and_match_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::open(dir.path().into(), Cipher::new(&[7; 32]))
            .await
            .unwrap();
        store.upsert("Me@Gmail.com", "rt-1").await.unwrap();

        let reopened = AccountStore::open(dir.path().into(), Cipher::new(&[7; 32]))
            .await
            .unwrap();
        assert_eq!(
            reopened.refresh_token("me@gmail.com").await.unwrap(),
            Some("rt-1".to_string())
        );
        let raw = std::fs::read_to_string(dir.path().join("accounts.json")).unwrap();
        assert!(
            !raw.contains("rt-1"),
            "refresh token must not be stored in clear"
        );
    }

    // Regression: a root-owned bind mount let the gateway start, then failed the first sign-in.
    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_to_start_when_the_data_folder_is_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let opened = AccountStore::open(dir.path().into(), Cipher::new(&[7; 32])).await;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = format!(
            "{:#}",
            opened.err().expect("must refuse an unwritable folder")
        );
        assert!(error.contains("cannot write to"), "{error}");
    }

    #[tokio::test]
    async fn refuses_to_start_with_the_wrong_encryption_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = AccountStore::open(dir.path().into(), Cipher::new(&[7; 32]))
            .await
            .unwrap();
        store.upsert("me@gmail.com", "rt-1").await.unwrap();
        assert!(
            AccountStore::open(dir.path().into(), Cipher::new(&[8; 32]))
                .await
                .is_err()
        );
    }
}
