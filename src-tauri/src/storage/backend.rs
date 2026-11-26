//! The storage a destination's files go to, whatever kind it is
//! (docs/storage-backends.md in the Mac repo). Callers build one with
//! `backend` instead of naming a provider, so another kind only adds a
//! variant here and its own module. S3 is the only one so far.
//!
//! An enum rather than a trait object: the methods are async, and a trait
//! with async methods can't be made into `dyn` without boxing every future.

use std::path::Path;
use std::sync::Arc;

use super::{BucketListing, BucketObject, ConnectionResult, Progress, S3Provider, StorageError, UploadResult};
use crate::credentials::StorageCredentials;
use crate::destinations::{DestinationConfig, StorageKind};
use crate::output::resolve_public_url;

pub enum Backend {
    S3(S3Provider),
}

/// The backend for `config`'s kind of storage. It can't fail for S3 (the
/// SDK checks the endpoint only when a request goes out), but a kind added
/// later may refuse settings it can't use before anything is sent.
pub fn backend(config: &DestinationConfig, credentials: &StorageCredentials) -> Result<Backend, StorageError> {
    match config.kind() {
        StorageKind::S3 => Ok(Backend::S3(S3Provider::new(config.clone(), credentials.clone()))),
        StorageKind::Webdav | StorageKind::Sftp => Err(StorageError::Unsupported),
    }
}

impl Backend {
    pub fn config(&self) -> &DestinationConfig {
        match self {
            Backend::S3(provider) => provider.config(),
        }
    }

    /// The S3 provider behind this backend, for what only S3 can do
    /// (presigned URLs, lifecycle rules, CORS, multipart sessions). Callers
    /// check the destination's capability for that first; this is None for
    /// any other kind either way.
    pub fn as_s3(&self) -> Option<&S3Provider> {
        match self {
            Backend::S3(provider) => Some(provider),
        }
    }

    /// `as_s3` for an operation the destination's capability allows, as in
    /// `backend.s3_if(destination.capabilities().temporary_links)`: None
    /// when it doesn't.
    pub fn s3_if(&self, capability: bool) -> Option<&S3Provider> {
        if capability {
            self.as_s3()
        } else {
            None
        }
    }

    pub async fn test_connection(&self) -> Result<ConnectionResult, StorageError> {
        match self {
            Backend::S3(provider) => provider.test_connection().await,
        }
    }

    /// Uploads a file in one request. With `only_if_new`, where the
    /// storage can (`Capabilities::conditional_writes`), nothing at the key
    /// is written over and `StorageError::AlreadyExists` says so.
    pub async fn upload(
        &self,
        path: &Path,
        object_key: &str,
        content_type: &str,
        progress: Arc<Progress>,
        only_if_new: bool,
    ) -> Result<UploadResult, StorageError> {
        match self {
            Backend::S3(provider) => provider.upload(path, object_key, content_type, progress, only_if_new).await,
        }
    }

    /// One small write from memory, such as a thumbnail.
    pub async fn put_bytes(&self, object_key: &str, data: Vec<u8>, content_type: &str) -> Result<(), StorageError> {
        match self {
            Backend::S3(provider) => provider.put_bytes(object_key, data, content_type).await,
        }
    }

    /// A share page, which opens inline and is written again on Replace File.
    pub async fn put_page(&self, object_key: &str, html: String) -> Result<(), StorageError> {
        match self {
            Backend::S3(provider) => provider.put_page(object_key, html).await,
        }
    }

    /// A small file and when it was last written, or None when it's not there.
    pub async fn get_small(&self, key: &str, max_bytes: u64) -> Result<Option<(Vec<u8>, Option<i64>)>, StorageError> {
        match self {
            Backend::S3(provider) => provider.get_small(key, max_bytes).await,
        }
    }

    /// Downloads a file to `destination`; false when it's not there.
    pub async fn download(&self, key: &str, destination: &Path, max_bytes: u64) -> Result<bool, StorageError> {
        match self {
            Backend::S3(provider) => provider.download(key, destination, max_bytes).await,
        }
    }

    pub async fn delete(&self, object_key: &str) -> Result<(), StorageError> {
        match self {
            Backend::S3(provider) => provider.delete(object_key).await,
        }
    }

    pub async fn delete_many(&self, keys: &[String]) -> Result<(), StorageError> {
        match self {
            Backend::S3(provider) => provider.delete_many(keys).await,
        }
    }

    pub async fn object_exists(&self, key: &str) -> Result<bool, StorageError> {
        match self {
            Backend::S3(provider) => provider.object_exists(key).await,
        }
    }

    /// The file at exactly `key` (size, last written), or None.
    pub async fn object_info(&self, key: &str) -> Result<Option<BucketObject>, StorageError> {
        match self {
            Backend::S3(provider) => provider.object_info(key).await,
        }
    }

    /// The size of the file at exactly `key`, or None when there's none.
    /// Watched folders check it before moving an uploaded original away.
    pub async fn object_size(&self, key: &str) -> Result<Option<i64>, StorageError> {
        Ok(self.object_info(key).await?.map(|object| object.size))
    }

    /// When the file at exactly `key` was last written (Unix milliseconds),
    /// or None when it's not there.
    pub async fn last_modified(&self, key: &str) -> Result<Option<i64>, StorageError> {
        Ok(self.object_info(key).await?.map(|object| object.last_modified.unwrap_or_default()))
    }

    /// One level under `prefix`: its folders and the files directly in it.
    pub async fn list(&self, prefix: &str, continuation_token: Option<String>) -> Result<BucketListing, StorageError> {
        match self {
            Backend::S3(provider) => provider.list(prefix, continuation_token).await,
        }
    }

    /// Every key under `prefix`, however deep, a page at a time.
    pub async fn list_recursively(&self, prefix: &str, continuation_token: Option<String>) -> Result<BucketListing, StorageError> {
        match self {
            Backend::S3(provider) => provider.list_recursively(prefix, continuation_token).await,
        }
    }

    pub async fn copy(&self, source_key: &str, destination_key: &str) -> Result<(), StorageError> {
        match self {
            Backend::S3(provider) => provider.copy(source_key, destination_key).await,
        }
    }

    pub async fn create_folder(&self, prefix: &str) -> Result<(), StorageError> {
        match self {
            Backend::S3(provider) => provider.create_folder(prefix).await,
        }
    }

    /// The link a file at `key` is reached by: the public base URL with
    /// the key appended, the same for every kind.
    pub fn public_url(&self, key: &str) -> String {
        resolve_public_url(&self.config().public_base_url, key)
    }
}
