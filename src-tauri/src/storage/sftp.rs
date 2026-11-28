//! SFTP destinations (docs/storage-backends.md in the Mac repo, "SFTP
//! (phase 3)"): any SSH server with the SFTP subsystem (OpenSSH, a Hetzner
//! Storage Box, Synology, a VPS), with a web server serving the same
//! folder for links.
//!
//! The SSH and SFTP work is `aktar_sftp_core` (crates/sftp-core, shared
//! with the Mac app): paths, keys, host keys, the connection pool and the
//! operations. This is the desktop's side of it: destinations and
//! credentials in, `StorageError`s (in the app's words) and bucket
//! listings out.
//!
//! The server's host key is trusted on first use, like `ssh`: a
//! destination without one takes the first key it sees and keeps it
//! (`set_host_key_listener`); one with a different key is refused before
//! anything is sent, with `StorageError::HostKeyChanged`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};

use aktar_sftp_core as core;
use core::{Client, ClientConfig, TestOutcome};

use super::{probe, BucketListing, BucketObject, ConnectionResult, Progress, StorageError, UploadResult};
use crate::credentials::StorageCredentials;
use crate::destinations::DestinationConfig;
use crate::output::resolve_public_url;
use crate::t;

pub use core::{fingerprint_of, host_key_text, is_private_key, parse_host_key, DEFAULT_PORT};

/// A recursive listing's page ends once it has this many files.
const RECURSIVE_PAGE_SIZE: usize = 1000;

// MARK: - Host keys

type HostKeyListener = Box<dyn Fn(&str, &str, &str) + Send + Sync>;

static HOST_KEY_LISTENER: OnceLock<HostKeyListener> = OnceLock::new();

/// Called with a destination's ID, its endpoint and the host key (OpenSSH
/// format) its first connection saw, so it's kept with the destination
/// while that still points at the same server (set once, at startup).
pub fn set_host_key_listener(listener: impl Fn(&str, &str, &str) + Send + Sync + 'static) {
    let _ = HOST_KEY_LISTENER.set(Box::new(listener));
}

// MARK: - Errors

/// The core's error in the app's words.
fn storage_error(error: core::Error) -> StorageError {
    use core::Error as E;
    match error {
        E::InvalidEndpoint => StorageError::Unknown(t!("Enter the server as a host name, such as files.example.com or files.example.com:2222.")),
        E::FolderHasParent => StorageError::Unknown(t!("The folder can’t contain “..”.")),
        E::InvalidName(key) => StorageError::Unknown(t!("This name can’t be used on an SFTP server: {0}", key)),
        E::AuthFailed => StorageError::InvalidSshLogin,
        E::KeyPassphrase => StorageError::KeyPassphrase,
        E::UnsupportedKey(name) => StorageError::UnsupportedKey(name),
        E::UnreadableKey => StorageError::UnreadableKey,
        E::UnreadableHostKey => StorageError::HostKeyChanged("?".into()),
        E::HostKeyChanged { seen, .. } => StorageError::HostKeyChanged(seen.as_deref().and_then(fingerprint_of).unwrap_or_default()),
        E::PermissionDenied => StorageError::Refused,
        E::FolderNotFound(path) => StorageError::RemoteFolderNotFound(path),
        E::FileNotFound => StorageError::Unknown(t!("The file isn’t on the server.")),
        E::FileInTheWay(folder) => StorageError::Unknown(t!("A file is in the way of the folder {0}.", folder)),
        E::Refused { host } => StorageError::Unreachable(t!("{0} refused the connection. Check the server and port.", host)),
        E::Timeout { host } => StorageError::Unreachable(t!("{0} didn’t answer in time.", host)),
        E::HostNotFound { host } => StorageError::Unreachable(t!("{0} couldn’t be found. Check the server name.", host)),
        E::NoCommonAlgorithm { host, detail } => StorageError::Unknown(t!("Aktar and {0} have no way of connecting in common ({1}).", host, detail)),
        E::NoSftp => StorageError::Unknown(t!("This server doesn’t offer SFTP.")),
        E::NoServerKey => StorageError::Connection(t!("The server didn’t show its key.")),
        E::Dropped(message) => StorageError::Connection(message),
        E::ServerTimeout => StorageError::Connection(t!("The server didn’t answer in time.")),
        E::OutOfSpace => StorageError::OutOfSpace,
        E::ServerAnswered(message) => StorageError::Unknown(t!("The server answered: {0}", message)),
        E::TooLarge => StorageError::Unknown("The object is too large.".into()),
        E::AlreadyExists => StorageError::AlreadyExists,
        E::Cancelled => StorageError::Unknown("Cancelled.".into()),
        E::Local(message) | E::Other(message) => StorageError::Unknown(message),
    }
}

/// During an upload, a connection failure is an interrupted upload.
fn as_upload_error(error: StorageError) -> StorageError {
    match error {
        StorageError::Connection(message) => StorageError::Network(message),
        other => other,
    }
}

/// Counts an upload's bytes into the app's `Progress`.
struct ProgressSink(Arc<Progress>);

impl core::Progress for ProgressSink {
    fn start(&self, total: u64) {
        self.0.start(total, 0);
    }

    fn advance(&self, bytes: u64) {
        self.0.sent.fetch_add(bytes, Ordering::Relaxed);
        self.0.last_activity.store(crate::util::now_millis(), Ordering::Relaxed);
    }
}

// MARK: - Provider

pub struct SftpProvider {
    config: DestinationConfig,
    client: Client,
}

impl SftpProvider {
    /// Refuses a server or folder that can't be right before anything is sent.
    pub fn new(config: DestinationConfig, credentials: StorageCredentials) -> Result<Self, StorageError> {
        let client = Client::new(ClientConfig {
            endpoint: config.endpoint.clone(),
            folder: config.bucket.clone(),
            user: credentials.access_key_id,
            secret: credentials.secret_access_key,
            key_passphrase: credentials.key_passphrase,
            host_key: config.ssh_host_key.clone(),
            trust_id: config.id.clone(),
        })
        .map_err(storage_error)?;
        let id = config.id.clone();
        let endpoint = config.endpoint.trim().to_string();
        let client = client.with_host_key_listener(move |key| {
            if !id.is_empty() {
                if let Some(listener) = HOST_KEY_LISTENER.get() {
                    listener(&id, &endpoint, key);
                }
            }
        });
        Ok(Self { config, client })
    }

    pub fn config(&self) -> &DestinationConfig {
        &self.config
    }

    /// Connects afresh (shows the key), signs in, looks at the folder,
    /// then the same test upload, public link check and delete as for S3.
    /// A server whose key isn't the stored one is reported before signing
    /// in: `previous_host_key_fingerprint` is set and nothing else is tried.
    pub async fn test_connection(&self) -> Result<ConnectionResult, StorageError> {
        match self.client.test_connection().await.map_err(storage_error)? {
            TestOutcome::HostKeyChanged { stored, seen } => Ok(ConnectionResult {
                bucket_reachable: false,
                writable: false,
                public_link: None,
                host_key_fingerprint: seen.as_deref().and_then(fingerprint_of),
                host_key: seen,
                previous_host_key_fingerprint: Some(stored.as_deref().and_then(fingerprint_of).unwrap_or_else(|| "?".to_string())),
            }),
            TestOutcome::Connected(session) => {
                let mut public_link = None;
                if session.writable && !self.config.public_base_url.trim().is_empty() {
                    public_link = Some(probe(&resolve_public_url(&self.config.public_base_url, &session.test_key)).await);
                }
                let result = ConnectionResult {
                    bucket_reachable: true,
                    writable: session.writable,
                    public_link,
                    host_key: Some(session.host_key.clone()),
                    host_key_fingerprint: Some(session.fingerprint.clone()),
                    ..Default::default()
                };
                session.finish().await;
                Ok(result)
            }
        }
    }

    /// Streams the file to its part file, counting the bytes sent into
    /// `progress`, then renames it over the key, after making the folders
    /// it goes in. With `only_if_new`, a file already at the key is left
    /// alone (callers only ask where the destination can, which SFTP
    /// doesn't claim).
    pub async fn upload(
        &self,
        path: &Path,
        object_key: &str,
        _content_type: &str,
        progress: Arc<Progress>,
        only_if_new: bool,
    ) -> Result<UploadResult, StorageError> {
        let sink: Arc<dyn core::Progress> = Arc::new(ProgressSink(progress.clone()));
        let byte_size = self.client.upload(path, object_key, Some(sink), only_if_new, None).await.map_err(|error| as_upload_error(storage_error(error)))?;
        progress.sent.store(byte_size, Ordering::Relaxed);
        Ok(UploadResult {
            object_key: object_key.to_string(),
            public_url: resolve_public_url(&self.config.public_base_url, object_key),
            byte_size: byte_size as i64,
        })
    }

    /// A small file from memory (thumbnails, share pages).
    pub async fn put_bytes(&self, object_key: &str, data: Vec<u8>, _content_type: &str) -> Result<(), StorageError> {
        self.client.put_bytes(object_key, &data).await.map_err(storage_error)
    }

    pub async fn put_page(&self, object_key: &str, html: String) -> Result<(), StorageError> {
        self.put_bytes(object_key, html.into_bytes(), crate::share_page::CONTENT_TYPE).await
    }

    /// A small file and when it was last written, or None when it's not
    /// there. Anything over `max_bytes` is refused.
    pub async fn get_small(&self, key: &str, max_bytes: u64) -> Result<Option<(Vec<u8>, Option<i64>)>, StorageError> {
        self.client.get_small(key, max_bytes).await.map_err(storage_error)
    }

    /// Downloads the file at `key` to `destination`, stopping past
    /// `max_bytes`. False when there's no such file.
    pub async fn download(&self, key: &str, destination: &Path, max_bytes: u64) -> Result<bool, StorageError> {
        self.client.download(key, destination, max_bytes, None).await.map_err(storage_error)
    }

    /// `remove`; no such file counts as done. A folder (a key ending in
    /// "/") goes only when it's empty, as for WebDAV.
    pub async fn delete(&self, object_key: &str) -> Result<(), StorageError> {
        self.client.delete(object_key).await.map_err(storage_error)
    }

    /// One by one: SFTP has no batch delete.
    pub async fn delete_many(&self, keys: &[String]) -> Result<(), StorageError> {
        for key in keys {
            self.delete(key).await?;
        }
        Ok(())
    }

    pub async fn object_exists(&self, key: &str) -> Result<bool, StorageError> {
        Ok(self.object_info(key).await?.is_some())
    }

    /// `stat`: size and mtime, or None when there's no file (a folder
    /// isn't one).
    pub async fn object_info(&self, key: &str) -> Result<Option<BucketObject>, StorageError> {
        let info = self.client.info(key).await.map_err(storage_error)?;
        Ok(info.map(|info| BucketObject { key: key.to_string(), size: info.size, last_modified: info.last_modified }))
    }

    /// The folders and files directly in the folder, in one page; `.`,
    /// `..` and part files are left out. A folder that isn't there is
    /// empty, as on S3; only the base folder missing is an error.
    pub async fn list(&self, prefix: &str, _continuation_token: Option<String>) -> Result<BucketListing, StorageError> {
        let prefix = core::folder_prefix(prefix);
        let entries = match self.client.list(&prefix).await.map_err(storage_error) {
            Ok(entries) => entries,
            Err(StorageError::RemoteFolderNotFound(_)) if !prefix.is_empty() => Vec::new(),
            Err(error) => return Err(error),
        };
        let mut folders = Vec::new();
        let mut objects = Vec::new();
        for entry in entries {
            if entry.is_folder {
                folders.push(format!("{prefix}{}/", entry.name));
            } else {
                objects.push(BucketObject { key: format!("{prefix}{}", entry.name), size: entry.size, last_modified: entry.last_modified });
            }
        }
        folders.sort();
        objects.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(BucketListing { prefix, folders, objects, next_continuation_token: None })
    }

    /// Every file under `prefix`, however deep: `list` on each folder in
    /// turn, about a thousand files a page. The continuation token is the
    /// folders still to list.
    pub async fn list_recursively(&self, prefix: &str, continuation_token: Option<String>) -> Result<BucketListing, StorageError> {
        let prefix = core::folder_prefix(prefix);
        let mut pending: VecDeque<String> = match continuation_token {
            Some(token) => serde_json::from_str::<Vec<String>>(&token)
                .map_err(|_| StorageError::Unknown("The listing can't be continued.".into()))?
                .into_iter()
                .filter(|folder| folder.starts_with(&prefix))
                .collect(),
            None => VecDeque::from([prefix.clone()]),
        };
        let mut folders = Vec::new();
        let mut objects = Vec::new();
        while let Some(folder) = pending.pop_front() {
            let level = self.list(&folder, None).await?;
            objects.extend(level.objects);
            for child in level.folders {
                pending.push_back(child.clone());
                folders.push(child);
            }
            if objects.len() >= RECURSIVE_PAGE_SIZE {
                break;
            }
        }
        let next_continuation_token = (!pending.is_empty()).then(|| serde_json::to_string(&pending).unwrap_or_default());
        Ok(BucketListing { prefix, folders, objects, next_continuation_token })
    }

    /// Read and written again through the app (SFTP has no server-side
    /// copy), with the same part-file rule.
    pub async fn copy(&self, source_key: &str, destination_key: &str) -> Result<(), StorageError> {
        self.client.copy(source_key, destination_key).await.map_err(storage_error)
    }

    /// A move on the server (`rename`, replacing what's at the target),
    /// after making the target's folders.
    pub async fn rename(&self, source_key: &str, destination_key: &str) -> Result<(), StorageError> {
        self.client.rename(source_key, destination_key).await.map_err(storage_error)
    }

    /// `mkdir` of the folder and each one it's in.
    pub async fn create_folder(&self, prefix: &str) -> Result<(), StorageError> {
        self.client.create_folder(prefix).await.map_err(storage_error)
    }
}

#[cfg(test)]
mod tests;
