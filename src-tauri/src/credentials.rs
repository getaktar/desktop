//! Secrets (Access Key ID, Secret Access Key, session token, the Cloudflare
//! and link shortener tokens, and the local API token) live only in Windows
//! Credential Manager, never in the JSON config or the SQLite history.

use serde::{Deserialize, Serialize};

use crate::t;

const CREDENTIALS_SERVICE: &str = "com.getaktar.windows.credentials";
const LOCAL_API_SERVICE: &str = "com.getaktar.windows.local-api";
const LOCAL_API_ACCOUNT: &str = "token";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    #[serde(default)]
    pub session_token: Option<String>,
    /// A Cloudflare API token that can purge the destination's zone cache,
    /// for replaced files; see `cloudflare`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare_token: Option<String>,
    /// The API key, signature or token of the destination's link shortener
    /// (`DestinationConfig::short_links`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_link_token: Option<String>,
    /// SFTP: the passphrase of an encrypted private key in
    /// `secret_access_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_passphrase: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[cfg_attr(not(target_os = "linux"), error("{}", t!("No credentials found in Credential Manager for this destination.")))]
    #[cfg_attr(target_os = "linux", error("{}", t!("No credentials found in the system keyring for this destination.")))]
    NotFound,
    /// The Secret Service is locked and its unlock prompt was dismissed
    /// (a desktop that logged in without a password leaves it locked).
    #[cfg(target_os = "linux")]
    #[error("{}", t!("The system keyring is locked. Unlock it with your login password when it asks, then try again."))]
    Locked,
    #[cfg_attr(not(target_os = "linux"), error("{}", t!("Credential Manager error ({0}).", .0)))]
    #[cfg_attr(target_os = "linux", error("{}", t!("System keyring error ({0}).", .0)))]
    Store(String),
}

impl From<keyring::Error> for CredentialError {
    fn from(error: keyring::Error) -> Self {
        match error {
            keyring::Error::NoEntry => CredentialError::NotFound,
            #[cfg(target_os = "linux")]
            keyring::Error::NoStorageAccess(_) => CredentialError::Locked,
            other => CredentialError::Store(other.to_string()),
        }
    }
}

fn entry(service: &str, account: &str) -> Result<keyring::Entry, CredentialError> {
    Ok(keyring::Entry::new(service, account)?)
}

/// The most one entry holds: Credential Manager caps a secret at 2560
/// bytes, which an RSA private key (SFTP) can pass.
const ENTRY_LIMIT: usize = 2400;

/// The entry that holds part `index` (from 1) of a destination's
/// credentials when they don't fit in one.
fn chunk_account(destination_id: &str, index: usize) -> String {
    format!("{destination_id}#{index}")
}

pub fn save(credentials: &StorageCredentials, destination_id: &str) -> Result<(), CredentialError> {
    let json = serde_json::to_vec(credentials).map_err(|error| CredentialError::Store(error.to_string()))?;
    // Raw UTF-8 bytes: Credential Manager caps a secret at 2560 bytes, and
    // storing it as a password (UTF-16) would halve the room a long STS
    // session token needs. What doesn't fit (a long private key) goes in
    // numbered entries next to it, and the entry itself says how many:
    // the JSON is the same, only split.
    let chunks: Vec<&[u8]> = if json.len() > ENTRY_LIMIT { json.chunks(ENTRY_LIMIT).collect() } else { Vec::new() };
    for (index, chunk) in chunks.iter().enumerate() {
        entry(CREDENTIALS_SERVICE, &chunk_account(destination_id, index + 1))?.set_secret(chunk)?;
    }
    if chunks.is_empty() {
        entry(CREDENTIALS_SERVICE, destination_id)?.set_secret(&json)?;
    } else {
        let index = serde_json::json!({ "aktarChunks": chunks.len() });
        entry(CREDENTIALS_SERVICE, destination_id)?.set_secret(index.to_string().as_bytes())?;
    }
    delete_chunks(destination_id, chunks.len() + 1);
    Ok(())
}

/// Removes the numbered entries from `from` on.
fn delete_chunks(destination_id: &str, from: usize) {
    for index in from.. {
        let Ok(entry) = entry(CREDENTIALS_SERVICE, &chunk_account(destination_id, index)) else { return };
        if entry.delete_credential().is_err() {
            return;
        }
    }
}

pub fn load(destination_id: &str) -> Result<StorageCredentials, CredentialError> {
    let json = entry(CREDENTIALS_SERVICE, destination_id)?.get_secret()?;
    let json = match chunk_count(&json) {
        None => json,
        Some(count) => {
            let mut joined = Vec::new();
            for index in 1..=count {
                joined.extend(entry(CREDENTIALS_SERVICE, &chunk_account(destination_id, index))?.get_secret()?);
            }
            joined
        }
    };
    serde_json::from_slice(&json).map_err(|error| CredentialError::Store(error.to_string()))
}

/// How many numbered entries hold the credentials, when the entry is the
/// index `save` writes for ones that don't fit.
fn chunk_count(json: &[u8]) -> Option<usize> {
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let count = value.as_object().filter(|object| object.len() == 1)?.get("aktarChunks")?.as_u64()?;
    usize::try_from(count).ok().filter(|count| (1..=64).contains(count))
}

pub fn delete(destination_id: &str) -> Result<(), CredentialError> {
    delete_chunks(destination_id, 1);
    match entry(CREDENTIALS_SERVICE, destination_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn load_api_token() -> Option<String> {
    entry(LOCAL_API_SERVICE, LOCAL_API_ACCOUNT)
        .ok()?
        .get_password()
        .ok()
        .filter(|token| !token.is_empty())
}

pub fn save_api_token(token: &str) {
    if let Ok(entry) = entry(LOCAL_API_SERVICE, LOCAL_API_ACCOUNT) {
        let _ = entry.set_password(token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tells_a_split_entry_from_credentials() {
        assert_eq!(chunk_count(br#"{"aktarChunks":2}"#), Some(2));
        assert_eq!(chunk_count(br#"{"aktarChunks":0}"#), None);
        assert_eq!(chunk_count(br#"{"aktarChunks":2,"accessKeyId":"a"}"#), None);
        assert_eq!(chunk_count(br#"{"accessKeyId":"a","secretAccessKey":"b"}"#), None);
        assert_eq!(chunk_count(b"not json"), None);
        // A 4096-bit RSA key doesn't fit in one entry; split and joined it's
        // the same JSON.
        let credentials = StorageCredentials { access_key_id: "me".into(), secret_access_key: "k".repeat(3400), ..Default::default() };
        let json = serde_json::to_vec(&credentials).unwrap();
        assert!(json.len() > ENTRY_LIMIT);
        let joined: Vec<u8> = json.chunks(ENTRY_LIMIT).flatten().copied().collect();
        assert_eq!(joined, json);
    }
}
