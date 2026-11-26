//! WebDAV destinations (docs/storage-backends.md in the Mac repo, "WebDAV
//! (phase 2)"): Nextcloud, ownCloud, Synology, a Hetzner Storage Box,
//! Apache `mod_dav`, nginx's DAV module, `rclone serve webdav`.
//!
//! The endpoint is the folder uploads go into, with its path, and a key is
//! a path under it, each "/"-separated segment percent-encoded. Every
//! request carries Basic authentication. Requests that write (`PUT`,
//! `MKCOL`, `COPY`, `DELETE`) never follow a redirect, and reads (`GET`,
//! `HEAD`, `PROPFIND`) only one to the same origin, so the password never
//! goes to another address: any other 3xx is an error naming where the
//! server pointed. Files of any size go up in one streamed `PUT`. A 5xx
//! (but 507) or 429, or a dropped connection, is tried again as the S3
//! SDK does: three tries in all.

use std::collections::{HashSet, VecDeque};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aws_sdk_s3::primitives::ByteStream;
use aws_smithy_types::body::SdkBody;
use percent_encoding::percent_decode_str;
use reqwest::header::{HeaderMap, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE, LAST_MODIFIED, LOCATION};
use reqwest::{Method, StatusCode};

use super::{innermost, probe, BucketListing, BucketObject, ConnectionResult, CountingBody, Progress, StorageError, UploadResult};
use crate::credentials::StorageCredentials;
use crate::destinations::DestinationConfig;
use crate::output::{encode_copy_source, resolve_public_url};
use crate::t;

/// What `list` asks each entry for.
const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;
/// A recursive listing's page ends once it has this many files.
const RECURSIVE_PAGE_SIZE: usize = 1000;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Any request other than sending a file, reading the answer included.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The waits before the second and third try, as the S3 SDK's standard
/// retries (three tries, backing off from a second).
const RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(2)];
/// How many same-origin redirects a read follows.
const MAX_REDIRECTS: usize = 5;

/// One shared client, so connections are pooled. It never follows a
/// redirect by itself: `WebdavProvider::send` decides which to follow.
fn client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default()
        })
        .clone()
}

pub struct WebdavProvider {
    config: DestinationConfig,
    /// The endpoint, ending in "/", without a query or user info.
    base: url::Url,
    /// The endpoint's path, decoded and ending in "/", which `PROPFIND`
    /// hrefs are matched against.
    base_path: String,
    user: String,
    password: String,
    /// Folders made (or found there) with `MKCOL` by this backend, so each
    /// is only asked for once.
    made_folders: Mutex<HashSet<String>>,
}

impl WebdavProvider {
    /// Refuses a server URL that isn't https (except on this computer),
    /// before anything is sent.
    pub fn new(config: DestinationConfig, credentials: StorageCredentials) -> Result<Self, StorageError> {
        let base = base_url(&config.endpoint)?;
        let base_path = decoded(base.path());
        Ok(Self {
            config,
            base,
            base_path,
            user: credentials.access_key_id,
            password: credentials.secret_access_key,
            made_folders: Mutex::new(HashSet::new()),
        })
    }

    pub fn config(&self) -> &DestinationConfig {
        &self.config
    }

    /// The URL of `key`: a folder's ends in "/" like its key.
    fn url(&self, key: &str) -> String {
        format!("{}{}", self.base.as_str(), encode_copy_source(key.trim_start_matches('/')))
    }

    fn method(name: &str) -> Method {
        Method::from_bytes(name.as_bytes()).unwrap_or(Method::GET)
    }

    /// Sends `method` to `url` with Basic authentication and what `build`
    /// adds (made again for each try), within `REQUEST_TIMEOUT` when
    /// `timed`. Reads follow redirects to the same origin; a server error,
    /// 429 or a dropped connection is tried again (`RETRY_DELAYS`).
    async fn send(
        &self,
        method: Method,
        url: &str,
        timed: bool,
        build: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, StorageError> {
        let reads = method == Method::GET || method == Method::HEAD || method.as_str() == "PROPFIND";
        let mut url = url.to_string();
        let mut redirects = 0;
        let mut attempt = 0;
        loop {
            let mut request = build(client().request(method.clone(), &url).basic_auth(&self.user, Some(&self.password)));
            if timed {
                request = request.timeout(REQUEST_TIMEOUT);
            }
            let retry_delay = RETRY_DELAYS.get(attempt).copied();
            match request.send().await {
                Err(error) => match retry_delay {
                    Some(delay) if is_retryable(&error) => {
                        attempt += 1;
                        tokio::time::sleep(delay).await;
                    }
                    _ => return Err(connection_error(error)),
                },
                Ok(response) => {
                    let status = response.status();
                    if reads && status.is_redirection() && redirects < MAX_REDIRECTS {
                        if let Some(next) = same_origin_location(&url, &response) {
                            redirects += 1;
                            url = next;
                            continue;
                        }
                    }
                    let retryable = (status.is_server_error() && status != StatusCode::INSUFFICIENT_STORAGE) || status == StatusCode::TOO_MANY_REQUESTS;
                    match retry_delay {
                        Some(delay) if retryable => {
                            attempt += 1;
                            tokio::time::sleep(delay).await;
                        }
                        _ => return Ok(response),
                    }
                }
            }
        }
    }

    /// `PROPFIND Depth: 0` on the endpoint (401: wrong user name or
    /// password, 404: no such folder), then the same test upload, public
    /// link check and delete as for S3.
    pub async fn test_connection(&self) -> Result<ConnectionResult, StorageError> {
        let response = self.propfind(self.base.as_str(), "0").await?;
        if !response.status().is_success() {
            return Err(self.status_error(&response, true));
        }

        let test_key = format!("_aktar_test_{}.txt", &uuid::Uuid::new_v4().to_string().to_uppercase()[..8]);
        let writable = self.put(&test_key, b"aktar connection test".to_vec(), "text/plain", None).await.is_ok();
        let mut public_link = None;
        if writable && !self.config.public_base_url.trim().is_empty() {
            public_link = Some(probe(&resolve_public_url(&self.config.public_base_url, &test_key)).await);
        }
        if writable {
            let _ = self.delete(&test_key).await;
        }
        Ok(ConnectionResult { bucket_reachable: true, writable, public_link })
    }

    /// Streams the file in one `PUT`, counting the bytes sent into
    /// `progress`, after creating the folders it goes in. With
    /// `only_if_new`, the server is asked not to write over a file at the
    /// key (If-None-Match: *); callers only ask where the destination can
    /// (`Capabilities::conditional_writes`), which WebDAV doesn't claim.
    pub async fn upload(
        &self,
        path: &Path,
        object_key: &str,
        content_type: &str,
        progress: Arc<Progress>,
        only_if_new: bool,
    ) -> Result<UploadResult, StorageError> {
        let byte_size = tokio::fs::metadata(path).await.map_err(|error| StorageError::Unknown(error.to_string()))?.len();
        progress.start(byte_size, 0);
        self.create_parents(object_key).await.map_err(as_upload_error)?;
        let file = ByteStream::from_path(path).await.map_err(|error| StorageError::Unknown(error.to_string()))?.into_inner();
        let cache_control = self.cache_control();
        // The body is made again from the file for every try, with the
        // count starting over, as for S3. No overall timeout: a big file
        // takes as long as it takes, and the upload's stall watchdog stops
        // one that stopped moving.
        let response = self
            .send(Method::PUT, &self.url(object_key), false, |request| {
                progress.sent.store(0, Ordering::Relaxed);
                let inner = file.try_clone().unwrap_or_else(SdkBody::taken);
                let mut request = request
                    .header(CONTENT_TYPE, content_type)
                    .header(CONTENT_LENGTH, byte_size)
                    .body(reqwest::Body::wrap(CountingBody { inner, progress: progress.clone(), attempt: None }));
                if let Some(cache_control) = cache_control {
                    request = request.header(CACHE_CONTROL, cache_control);
                }
                if only_if_new {
                    request = request.header("If-None-Match", "*");
                }
                request
            })
            .await
            .map_err(as_upload_error)?;
        if !response.status().is_success() {
            return Err(self.status_error(&response, false));
        }
        progress.sent.store(byte_size, Ordering::Relaxed);
        Ok(UploadResult {
            object_key: object_key.to_string(),
            public_url: resolve_public_url(&self.config.public_base_url, object_key),
            byte_size: byte_size as i64,
        })
    }

    /// What "Short cache" sends, as for S3.
    fn cache_control(&self) -> Option<&'static str> {
        self.config.short_cache().then_some(super::SHORT_CACHE_CONTROL)
    }

    /// A small file from memory (thumbnails, share pages), after creating
    /// the folders it goes in.
    async fn put(&self, key: &str, data: Vec<u8>, content_type: &str, cache_control: Option<&str>) -> Result<(), StorageError> {
        self.create_parents(key).await?;
        let response = self
            .send(Method::PUT, &self.url(key), true, |request| {
                let request = request.header(CONTENT_TYPE, content_type).header(CONTENT_LENGTH, data.len()).body(data.clone());
                match cache_control {
                    Some(cache_control) => request.header(CACHE_CONTROL, cache_control),
                    None => request,
                }
            })
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(self.status_error(&response, false))
        }
    }

    pub async fn put_bytes(&self, object_key: &str, data: Vec<u8>, content_type: &str) -> Result<(), StorageError> {
        self.put(object_key, data, content_type, None).await
    }

    /// A share page: opens inline, with a short cache time since Replace
    /// File writes it again.
    pub async fn put_page(&self, object_key: &str, html: String) -> Result<(), StorageError> {
        self.put(object_key, html.into_bytes(), crate::share_page::CONTENT_TYPE, Some(crate::share_page::CACHE_CONTROL)).await
    }

    /// `MKCOL` for each folder `key` is in, from the top: 201 made it, 405
    /// says it's there already. The endpoint itself has to exist.
    async fn create_parents(&self, key: &str) -> Result<(), StorageError> {
        for folder in parent_folders(key) {
            self.make_folder(&folder).await?;
        }
        Ok(())
    }

    /// One `MKCOL`, unless this backend made the folder already.
    async fn make_folder(&self, folder: &str) -> Result<(), StorageError> {
        if self.made_folders.lock().unwrap().contains(folder) {
            return Ok(());
        }
        let response = self.send(Self::method("MKCOL"), &self.url(folder), true, |request| request).await?;
        match response.status().as_u16() {
            200..=299 | 405 => {
                self.made_folders.lock().unwrap().insert(folder.to_string());
                Ok(())
            }
            _ => Err(self.status_error(&response, false)),
        }
    }

    /// A small file and when it was last written, or None when it's not
    /// there. Anything over `max_bytes` is refused.
    pub async fn get_small(&self, key: &str, max_bytes: u64) -> Result<Option<(Vec<u8>, Option<i64>)>, StorageError> {
        let mut response = self.send(Method::GET, &self.url(key), true, |request| request).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(self.status_error(&response, false));
        }
        let too_large = || StorageError::Unknown("The object is too large.".into());
        if response.content_length().is_some_and(|length| length > max_bytes) {
            return Err(too_large());
        }
        let last_modified = last_modified(response.headers());
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(connection_error)? {
            data.extend_from_slice(&chunk);
            if data.len() as u64 > max_bytes {
                return Err(too_large());
            }
        }
        Ok(Some((data, last_modified)))
    }

    /// Downloads the file at `key` to `destination`, streamed, stopping
    /// past `max_bytes`. False when there's no such file.
    pub async fn download(&self, key: &str, destination: &Path, max_bytes: u64) -> Result<bool, StorageError> {
        use tokio::io::AsyncWriteExt;
        // Only connecting and the headers are timed: the body takes as long
        // as the file needs.
        let mut response = self.send(Method::GET, &self.url(key), false, |request| request).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !response.status().is_success() {
            return Err(self.status_error(&response, false));
        }
        let too_large = || StorageError::Unknown("The object is too large.".into());
        if response.content_length().is_some_and(|length| length > max_bytes) {
            return Err(too_large());
        }
        let mut file = tokio::fs::File::create(destination).await.map_err(|error| StorageError::Unknown(error.to_string()))?;
        let mut written = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(connection_error)? {
            written += chunk.len() as u64;
            if written > max_bytes {
                drop(file);
                let _ = tokio::fs::remove_file(destination).await;
                return Err(too_large());
            }
            file.write_all(&chunk).await.map_err(|error| StorageError::Unknown(error.to_string()))?;
        }
        file.flush().await.map_err(|error| StorageError::Unknown(error.to_string()))?;
        Ok(true)
    }

    /// `DELETE`; 404 counts as done. A folder (a key ending in "/") goes
    /// only when it's empty: on S3 deleting "name/" takes only the empty
    /// placeholder, never the files under it, and here it would take them all.
    pub async fn delete(&self, object_key: &str) -> Result<(), StorageError> {
        if object_key.ends_with('/') {
            let listing = self.list(object_key, None).await?;
            if !listing.objects.is_empty() || !listing.folders.is_empty() {
                return Ok(());
            }
        }
        let response = self.send(Method::DELETE, &self.url(object_key), true, |request| request).await?;
        if object_key.ends_with('/') {
            self.made_folders.lock().unwrap().retain(|folder| !folder.starts_with(object_key));
        }
        match response.status().as_u16() {
            200..=299 | 404 => Ok(()),
            _ => Err(self.status_error(&response, false)),
        }
    }

    /// One by one: WebDAV has no batch delete.
    pub async fn delete_many(&self, keys: &[String]) -> Result<(), StorageError> {
        for key in keys {
            self.delete(key).await?;
        }
        Ok(())
    }

    pub async fn object_exists(&self, key: &str) -> Result<bool, StorageError> {
        Ok(self.object_info(key).await?.is_some())
    }

    /// `HEAD`: the size from Content-Length and the date from
    /// Last-Modified, or None for 404. Asked for without compression: a
    /// server that would gzip the file (rclone) reports the compressed size
    /// otherwise.
    pub async fn object_info(&self, key: &str) -> Result<Option<BucketObject>, StorageError> {
        let response = self.send(Method::HEAD, &self.url(key), true, |request| request.header(ACCEPT_ENCODING, "identity")).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(self.status_error(&response, false));
        }
        let size = response
            .headers()
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or_default();
        Ok(Some(BucketObject { key: key.to_string(), size, last_modified: last_modified(response.headers()) }))
    }

    /// `PROPFIND Depth: 1` on the folder: its folders and the files
    /// directly in it, in one page. A folder that isn't there is empty, as
    /// on S3; only the endpoint itself missing is an error.
    pub async fn list(&self, prefix: &str, _continuation_token: Option<String>) -> Result<BucketListing, StorageError> {
        let prefix = folder_prefix(prefix);
        let response = self.propfind(&self.url(&prefix), "1").await?;
        let status = response.status();
        if status == StatusCode::NOT_FOUND && !prefix.is_empty() {
            return Ok(BucketListing { prefix, folders: Vec::new(), objects: Vec::new(), next_continuation_token: None });
        }
        if !status.is_success() {
            return Err(self.status_error(&response, prefix.is_empty()));
        }
        let xml = response.text().await.map_err(connection_error)?;
        let entries = parse_multistatus(&xml).map_err(|error| {
            log::warn!("Unreadable PROPFIND answer: {error}");
            StorageError::Unknown(t!("The server’s list of files couldn’t be read."))
        })?;
        // Hrefs under the endpoint's path, or, from a server behind a
        // reverse proxy that rewrites the path, under the folder's own
        // entry (the first one: the folder itself).
        let own_href = entries.first().map(|entry| entry.href.clone()).unwrap_or_default();
        let under_endpoint = key_for_href(&own_href, &self.base_path).is_some();
        let folder_path = if under_endpoint { String::new() } else { format!("{}/", href_path(&own_href).trim_end_matches('/')) };
        let mut folders = Vec::new();
        let mut objects = Vec::new();
        for entry in entries {
            let key = if under_endpoint {
                key_for_href(&entry.href, &self.base_path)
            } else {
                key_for_href(&entry.href, &folder_path).map(|rest| format!("{prefix}{rest}"))
            };
            let Some(key) = key else { continue };
            let is_folder = entry.is_collection || key.ends_with('/');
            let key = if is_folder { folder_prefix(&key) } else { key };
            // The folder's own entry, and anything not directly in it.
            if key.is_empty() || key == prefix || !is_direct_child(&key, &prefix) {
                continue;
            }
            if is_folder {
                folders.push(key);
            } else {
                objects.push(BucketObject { key, size: entry.size.unwrap_or_default(), last_modified: entry.last_modified });
            }
        }
        folders.sort();
        folders.dedup();
        objects.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(BucketListing { prefix, folders, objects, next_continuation_token: None })
    }

    /// Every file under `prefix`, however deep: `list` on each folder in
    /// turn (servers often refuse `Depth: infinity`), about a thousand files
    /// a page. The continuation token is the folders still to list.
    /// `folders` holds every folder found, as S3's holds the placeholders.
    pub async fn list_recursively(&self, prefix: &str, continuation_token: Option<String>) -> Result<BucketListing, StorageError> {
        let prefix = folder_prefix(prefix);
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

    /// `COPY` to the absolute URL of `destination_key`, replacing what's
    /// there, after creating its folders.
    pub async fn copy(&self, source_key: &str, destination_key: &str) -> Result<(), StorageError> {
        self.create_parents(destination_key).await?;
        let destination = self.url(destination_key);
        let response = self
            .send(Self::method("COPY"), &self.url(source_key), true, |request| {
                request.header("Destination", destination.as_str()).header("Overwrite", "T")
            })
            .await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(self.status_error(&response, false))
        }
    }

    /// `MKCOL` of the folder and each one it's in.
    pub async fn create_folder(&self, prefix: &str) -> Result<(), StorageError> {
        let prefix = folder_prefix(prefix);
        if prefix.is_empty() {
            return Ok(());
        }
        self.create_parents(&prefix).await?;
        self.make_folder(&prefix).await
    }

    async fn propfind(&self, url: &str, depth: &str) -> Result<reqwest::Response, StorageError> {
        self.send(Self::method("PROPFIND"), url, true, |request| {
            request.header("Depth", depth).header(CONTENT_TYPE, "application/xml; charset=utf-8").body(PROPFIND_BODY)
        })
        .await
    }

    /// What a failed answer means. `at_endpoint`: the request was for the
    /// endpoint itself, so 404 is the folder not being there.
    fn status_error(&self, response: &reqwest::Response, at_endpoint: bool) -> StorageError {
        let location = response.headers().get(LOCATION).and_then(|value| value.to_str().ok());
        status_error(response.status(), location, at_endpoint)
    }
}

/// The endpoint as the base every key's URL starts with: https (or plain
/// http on this computer only), ending in "/". One with user info, a query
/// or a fragment is refused.
pub fn base_url(endpoint: &str) -> Result<url::Url, StorageError> {
    let invalid = || StorageError::Unknown(t!("Enter the folder’s full address, such as https://cloud.example.com/remote.php/dav/files/me/aktar."));
    let trimmed = endpoint.trim();
    let raw = if trimmed.contains("://") { trimmed.to_string() } else { format!("https://{trimmed}") };
    let mut url = url::Url::parse(&raw).map_err(|_| invalid())?;
    if url.host_str().is_none_or(str::is_empty) {
        return Err(invalid());
    }
    match url.scheme() {
        "https" => {}
        "http" if is_local(&url) => {}
        "http" => return Err(StorageError::InsecureServer),
        _ => return Err(invalid()),
    }
    // The password goes in the Authorization header only, and a key is
    // always added to the path.
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
        return Err(invalid());
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

/// `localhost`, `127.0.0.1` or `::1`: a server on this computer, where
/// plain http doesn't cross the network.
fn is_local(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address == std::net::Ipv4Addr::LOCALHOST,
        Some(url::Host::Ipv6(address)) => address == std::net::Ipv6Addr::LOCALHOST,
        None => false,
    }
}

/// A status outside 2xx as a `StorageError`: 401 a wrong user name or
/// password, 403 refused, 404 on the endpoint as the folder not being
/// there, 507 as a full server, a redirect naming where it points, 5xx and
/// 429 as the server's trouble.
fn status_error(status: StatusCode, location: Option<&str>, at_endpoint: bool) -> StorageError {
    match status.as_u16() {
        401 => StorageError::InvalidLogin,
        403 => StorageError::Refused,
        404 if at_endpoint => StorageError::FolderNotFound,
        412 => StorageError::AlreadyExists,
        507 => StorageError::OutOfSpace,
        300..=399 => StorageError::Redirected(location.unwrap_or_default().to_string()),
        code if code >= 500 || code == 429 => StorageError::Server(status_text(status)),
        _ => StorageError::Unknown(status_text(status)),
    }
}

/// "The server answered with HTTP 409."
fn status_text(status: StatusCode) -> String {
    t!("The server answered with HTTP {0}.", status.as_u16())
}

/// Whether a failed request may work on another try: the connection
/// couldn't be made or dropped, or it timed out.
fn is_retryable(error: &reqwest::Error) -> bool {
    error.is_connect() || error.is_timeout() || error.is_request() || error.is_body()
}

/// Where a redirect points when that's the same origin (scheme, host and
/// port) as `url`, so the password may go there too.
fn same_origin_location(url: &str, response: &reqwest::Response) -> Option<String> {
    let from = url::Url::parse(url).ok()?;
    let location = response.headers().get(LOCATION)?.to_str().ok()?;
    let to = from.join(location).ok()?;
    (to.origin() == from.origin()).then(|| to.to_string())
}

fn connection_error(error: reqwest::Error) -> StorageError {
    StorageError::Connection(innermost(&error))
}

/// During an upload, a connection failure is an interrupted upload.
fn as_upload_error(error: StorageError) -> StorageError {
    match error {
        StorageError::Connection(message) => StorageError::Network(message),
        other => other,
    }
}

fn last_modified(headers: &HeaderMap) -> Option<i64> {
    headers.get(LAST_MODIFIED).and_then(|value| value.to_str().ok()).and_then(http_date_millis)
}

/// An HTTP date ("Tue, 15 Nov 1994 08:12:31 GMT") as Unix milliseconds.
fn http_date_millis(date: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc2822(date.trim()).ok().map(|date| date.timestamp_millis())
}

/// "" or "a/b/": a folder key with exactly one trailing "/".
fn folder_prefix(raw: &str) -> String {
    let trimmed = raw.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// The folders a key is in, from the top: "a/b/c.txt" is in "a/" and
/// "a/b/"; "a/b/" itself is in "a/".
fn parent_folders(key: &str) -> Vec<String> {
    let key = key.trim_start_matches('/');
    let name_start = key.trim_end_matches('/').rfind('/').map_or(0, |index| index + 1);
    let mut folders = Vec::new();
    for (index, character) in key[..name_start].char_indices() {
        if character == '/' && index > 0 {
            folders.push(key[..=index].to_string());
        }
    }
    folders
}

/// Whether `key` is directly in the folder `prefix` ("" for the endpoint).
fn is_direct_child(key: &str, prefix: &str) -> bool {
    let Some(rest) = key.strip_prefix(prefix) else { return false };
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    !rest.is_empty() && !rest.contains('/')
}

fn decoded(path: &str) -> String {
    percent_decode_str(path).decode_utf8_lossy().into_owned()
}

/// The key a `PROPFIND` href stands for: absolute URL or absolute path,
/// percent-encoded or not, with the endpoint's path taken off. A folder's
/// key keeps its trailing "/". None for an href outside the endpoint.
fn key_for_href(href: &str, base_path: &str) -> Option<String> {
    let path = href_path(href);
    let base = base_path.trim_end_matches('/');
    if path == base || path == format!("{base}/") {
        return Some(String::new());
    }
    path.strip_prefix(&format!("{base}/")).map(str::to_string)
}

/// An href's path, decoded, starting with "/".
fn href_path(href: &str) -> String {
    let href = href.trim();
    let path = match url::Url::parse(href) {
        Ok(url) if url.has_host() => url.path().to_string(),
        _ => href.split(['?', '#']).next().unwrap_or_default().to_string(),
    };
    let path = decoded(&path);
    if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    }
}

/// One `<response>` of a `PROPFIND` answer.
#[derive(Debug, Default, Clone, PartialEq)]
struct Entry {
    href: String,
    is_collection: bool,
    size: Option<i64>,
    last_modified: Option<i64>,
}

/// The `<response>` entries of a 207 Multi-Status answer, matched by
/// namespace ("DAV:") and local name, so any prefix works: `d:`, `D:`, a
/// default namespace. Only properties in a 2xx `<propstat>` count.
fn parse_multistatus(xml: &str) -> Result<Vec<Entry>, String> {
    use quick_xml::events::Event;
    use quick_xml::name::{Namespace, ResolveResult};

    #[derive(Clone, Copy, PartialEq)]
    enum Field {
        None,
        Href,
        Length,
        Modified,
        Status,
    }

    let mut reader = quick_xml::NsReader::from_str(xml);
    let mut entries = Vec::new();
    let mut entry: Option<Entry> = None;
    // Properties of the `<propstat>` being read, kept once its status is 2xx.
    let mut propstat: Option<(Entry, bool)> = None;
    let mut field = Field::None;
    let mut text = String::new();
    loop {
        let (namespace, event) = reader.read_resolved_event().map_err(|error| error.to_string())?;
        let is_dav = matches!(namespace, ResolveResult::Bound(Namespace("DAV:")));
        match event {
            Event::Start(start) if is_dav => match start.local_name().as_ref() {
                "response" => entry = Some(Entry::default()),
                "propstat" => propstat = Some((Entry::default(), true)),
                "href" if propstat.is_none() => field = Field::Href,
                "getcontentlength" => field = Field::Length,
                "getlastmodified" => field = Field::Modified,
                "status" => field = Field::Status,
                "collection" => {
                    if let Some((props, _)) = &mut propstat {
                        props.is_collection = true;
                    }
                }
                _ => {}
            },
            Event::Empty(empty) if is_dav && empty.local_name().as_ref() == "collection" => {
                if let Some((props, _)) = &mut propstat {
                    props.is_collection = true;
                }
            }
            Event::Text(content) if field != Field::None => text.push_str(&content.xml10_content()),
            Event::CData(content) if field != Field::None => text.push_str(&content.xml10_content()),
            Event::GeneralRef(reference) if field != Field::None => {
                let name = reference.xml10_content();
                if let Ok(Some(character)) = reference.resolve_char_ref() {
                    text.push(character);
                } else if let Some(resolved) = quick_xml::escape::resolve_predefined_entity(&name) {
                    text.push_str(resolved);
                }
            }
            Event::End(end) if is_dav => {
                let name = end.local_name();
                let value = std::mem::take(&mut text);
                let value = value.trim();
                match (name.as_ref(), field) {
                    ("href", Field::Href) => {
                        if let Some(entry) = &mut entry {
                            entry.href = value.to_string();
                        }
                    }
                    ("getcontentlength", Field::Length) => {
                        if let Some((props, _)) = &mut propstat {
                            props.size = value.parse().ok();
                        }
                    }
                    ("getlastmodified", Field::Modified) => {
                        if let Some((props, _)) = &mut propstat {
                            props.last_modified = http_date_millis(value);
                        }
                    }
                    ("status", Field::Status) => {
                        // "HTTP/1.1 200 OK"
                        if let Some((_, ok)) = &mut propstat {
                            *ok = value.split_whitespace().nth(1).is_some_and(|code| code.starts_with('2'));
                        }
                    }
                    ("propstat", _) => {
                        if let (Some(entry), Some((props, true))) = (&mut entry, propstat.take()) {
                            entry.is_collection |= props.is_collection;
                            entry.size = entry.size.or(props.size);
                            entry.last_modified = entry.last_modified.or(props.last_modified);
                        }
                    }
                    ("response", _) => {
                        if let Some(entry) = entry.take().filter(|entry| !entry.href.is_empty()) {
                            entries.push(entry);
                        }
                    }
                    _ => {}
                }
                field = Field::None;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(endpoint: &str) -> WebdavProvider {
        let config: DestinationConfig = serde_json::from_value(serde_json::json!({
            "id": "W", "name": "Cloud", "preset": "webdav", "endpoint": endpoint, "region": "", "bucket": "",
            "publicBaseURL": "https://files.example.com", "objectPathTemplate": "{uuid}.{ext}", "forcePathStyle": false,
        }))
        .unwrap();
        let credentials = StorageCredentials {
            access_key_id: "me".into(),
            secret_access_key: "secret".into(),
            session_token: None,
            cloudflare_token: None,
            short_link_token: None,
        };
        WebdavProvider::new(config, credentials).unwrap()
    }

    #[test]
    fn keys_become_urls_segment_by_segment() {
        let storage = provider("https://cloud.example.com/remote.php/dav/files/me/aktar");
        assert_eq!(storage.url("2026/11/a b+c (1).png"), "https://cloud.example.com/remote.php/dav/files/me/aktar/2026/11/a%20b%2Bc%20%281%29.png");
        assert_eq!(storage.url("ü/é~x_y-z.txt"), "https://cloud.example.com/remote.php/dav/files/me/aktar/%C3%BC/%C3%A9~x_y-z.txt");
        assert_eq!(storage.url("a/#?%.txt"), "https://cloud.example.com/remote.php/dav/files/me/aktar/a/%23%3F%25.txt");
        assert_eq!(storage.url("folder/"), "https://cloud.example.com/remote.php/dav/files/me/aktar/folder/");
        assert_eq!(storage.url(""), "https://cloud.example.com/remote.php/dav/files/me/aktar/");
        // The endpoint's own path may need encoding too.
        let spaced = provider("https://nas.example.com/home/My Files/");
        assert_eq!(spaced.url("x.png"), "https://nas.example.com/home/My%20Files/x.png");
        assert_eq!(spaced.base_path, "/home/My Files/");
    }

    #[test]
    fn server_urls_need_https_except_here() {
        assert_eq!(base_url("cloud.example.com/dav").unwrap().as_str(), "https://cloud.example.com/dav/");
        assert_eq!(base_url(" https://cloud.example.com/dav/ ").unwrap().as_str(), "https://cloud.example.com/dav/");
        for unusable in ["https://u:p@cloud.example.com/dav", "https://u@cloud.example.com/dav", "https://cloud.example.com/dav?x=1", "https://cloud.example.com/dav#y"] {
            assert!(matches!(base_url(unusable), Err(StorageError::Unknown(_))), "{unusable}");
        }
        assert!(matches!(base_url("http://cloud.example.com/dav"), Err(StorageError::InsecureServer)));
        assert!(matches!(base_url("http://192.168.1.10:8080/"), Err(StorageError::InsecureServer)));
        for local in ["http://localhost:8080/dav", "http://LOCALHOST/", "http://127.0.0.1:9000", "http://[::1]:9000/x"] {
            assert!(base_url(local).is_ok(), "{local}");
        }
        assert!(matches!(base_url("ftp://example.com"), Err(StorageError::Unknown(_))));
        assert!(matches!(base_url("https://"), Err(StorageError::Unknown(_))));
        assert!(matches!(base_url(""), Err(StorageError::Unknown(_))));
    }

    #[test]
    fn finds_the_folders_a_key_is_in() {
        assert_eq!(parent_folders("a/b/c.txt"), vec!["a/", "a/b/"]);
        assert_eq!(parent_folders("c.txt"), Vec::<String>::new());
        assert_eq!(parent_folders("a/b/"), vec!["a/"]);
        assert_eq!(parent_folders("a/"), Vec::<String>::new());
        assert_eq!(parent_folders("ü/ş/x"), vec!["ü/", "ü/ş/"]);
        assert_eq!(folder_prefix("/a/b"), "a/b/");
        assert_eq!(folder_prefix(""), "");
        assert!(is_direct_child("a/b.txt", "a/"));
        assert!(is_direct_child("a/c/", "a/"));
        assert!(!is_direct_child("a/c/d.txt", "a/"));
        assert!(is_direct_child("x.txt", ""));
    }

    #[test]
    fn hrefs_become_keys() {
        let base = "/remote.php/dav/files/me/aktar/";
        // Absolute paths, encoded or not.
        assert_eq!(key_for_href("/remote.php/dav/files/me/aktar/2026/a%20b.png", base).as_deref(), Some("2026/a b.png"));
        assert_eq!(key_for_href("/remote.php/dav/files/me/aktar/2026/a b.png", base).as_deref(), Some("2026/a b.png"));
        assert_eq!(key_for_href("/remote.php/dav/files/me/aktar/%C3%BC/", base).as_deref(), Some("ü/"));
        // Absolute URLs.
        assert_eq!(key_for_href("https://cloud.example.com/remote.php/dav/files/me/aktar/x%2By.txt", base).as_deref(), Some("x+y.txt"));
        // The folder itself, with or without its slash.
        assert_eq!(key_for_href("/remote.php/dav/files/me/aktar/", base).as_deref(), Some(""));
        assert_eq!(key_for_href("/remote.php/dav/files/me/aktar", base).as_deref(), Some(""));
        // An endpoint path written encoded by the server but not by the user.
        assert_eq!(key_for_href("/home/My%20Files/x.png", "/home/My Files/").as_deref(), Some("x.png"));
        // Outside the endpoint, or merely sharing its start.
        assert_eq!(key_for_href("/remote.php/dav/files/me/other/x", base), None);
        assert_eq!(key_for_href("/remote.php/dav/files/me/aktarx/y", base), None);
        // An endpoint at the server's root.
        assert_eq!(key_for_href("/a/b.txt", "/").as_deref(), Some("a/b.txt"));
        assert_eq!(key_for_href("/", "/").as_deref(), Some(""));
    }

    /// Nextcloud's answer: `d:` prefixed, absolute paths, the props it
    /// doesn't have for a folder in a 404 `propstat`.
    const NEXTCLOUD: &str = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns" xmlns:oc="http://owncloud.org/ns" xmlns:nc="http://nextcloud.org/ns">
 <d:response>
  <d:href>/remote.php/dav/files/me/aktar/2026/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype><d:getlastmodified>Wed, 25 Nov 2026 10:00:00 GMT</d:getlastmodified></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
  <d:propstat><d:prop><d:getcontentlength/></d:prop><d:status>HTTP/1.1 404 Not Found</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/me/aktar/2026/a%20b%20%C3%BC.png</d:href>
  <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>1234</d:getcontentlength><d:getlastmodified>Thu, 26 Nov 2026 08:12:31 GMT</d:getlastmodified></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/me/aktar/2026/sub/</d:href>
  <d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
 <d:response>
  <d:href>/remote.php/dav/files/me/aktar/2026/R&amp;D&#x20;notes.txt</d:href>
  <d:propstat><d:prop><d:resourcetype/><d:getcontentlength>7</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>
 </d:response>
</d:multistatus>"#;

    #[test]
    fn parses_nextcloud_style_answers() {
        let entries = parse_multistatus(NEXTCLOUD).unwrap();
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0], Entry { href: "/remote.php/dav/files/me/aktar/2026/".into(), is_collection: true, size: None, last_modified: Some(1_795_600_800_000) });
        assert_eq!(entries[1].href, "/remote.php/dav/files/me/aktar/2026/a%20b%20%C3%BC.png");
        assert!(!entries[1].is_collection);
        assert_eq!(entries[1].size, Some(1234));
        assert_eq!(entries[1].last_modified, Some(1_795_680_751_000));
        assert!(entries[2].is_collection);
        // Entities and character references in an href.
        assert_eq!(entries[3].href, "/remote.php/dav/files/me/aktar/2026/R&D notes.txt");

        let base = "/remote.php/dav/files/me/aktar/";
        let keys: Vec<Option<String>> = entries.iter().map(|entry| key_for_href(&entry.href, base)).collect();
        assert_eq!(keys, vec![Some("2026/".into()), Some("2026/a b ü.png".into()), Some("2026/sub/".into()), Some("2026/R&D notes.txt".into())]);
    }

    /// Apache and rclone: `D:` or a default namespace, absolute URLs, a
    /// folder told only by its trailing slash, and an unknown namespace's
    /// href that isn't the entry's.
    #[test]
    fn parses_any_prefix_and_absolute_urls() {
        let default_namespace = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:" xmlns:x="urn:other">
<response><href>https://dav.example.com/files/a.txt</href><propstat><prop><getcontentlength>5</getcontentlength><x:href>/nope</x:href></prop><status>HTTP/1.1 200 OK</status></propstat></response>
<response><href>https://dav.example.com/files/folder/</href><propstat><prop><resourcetype></resourcetype></prop><status>HTTP/1.1 200 OK</status></propstat></response>
</multistatus>"#;
        let entries = parse_multistatus(default_namespace).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].href, "https://dav.example.com/files/a.txt");
        assert_eq!(entries[0].size, Some(5));
        assert_eq!(key_for_href(&entries[1].href, "/files/").as_deref(), Some("folder/"));

        let upper = r#"<D:multistatus xmlns:D="DAV:"><D:response><D:href>/files/b.bin</D:href><D:propstat><D:prop><D:getcontentlength>
          42 </D:getcontentlength><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response></D:multistatus>"#;
        let entries = parse_multistatus(upper).unwrap();
        assert_eq!((entries[0].href.as_str(), entries[0].size, entries[0].is_collection), ("/files/b.bin", Some(42), false));

        // A `collection` element outside DAV: doesn't make a folder.
        let foreign = r#"<a:multistatus xmlns:a="DAV:" xmlns:b="urn:b"><a:response><a:href>/f/x</a:href><a:propstat><a:prop><a:resourcetype><b:collection/></a:resourcetype></a:prop><a:status>HTTP/1.1 200 OK</a:status></a:propstat></a:response></a:multistatus>"#;
        assert!(!parse_multistatus(foreign).unwrap()[0].is_collection);
        assert!(parse_multistatus("<d:multistatus xmlns:d=\"DAV:\"><d:response>").is_ok_and(|entries| entries.is_empty()));
        assert!(parse_multistatus("<a><b></a>").is_err());
    }

    #[test]
    fn maps_statuses_to_errors() {
        let error = |code: u16, at_endpoint: bool| status_error(StatusCode::from_u16(code).unwrap(), Some("https://other.example.com/x"), at_endpoint);
        assert!(matches!(error(401, false), StorageError::InvalidLogin));
        assert!(matches!(error(403, false), StorageError::Refused));
        assert!(matches!(error(404, true), StorageError::FolderNotFound));
        assert!(matches!(error(404, false), StorageError::Unknown(message) if message == "The server answered with HTTP 404."));
        assert!(matches!(error(507, false), StorageError::OutOfSpace));
        assert!(matches!(error(301, false), StorageError::Redirected(location) if location == "https://other.example.com/x"));
        assert!(matches!(error(503, false), StorageError::Server(_)));
        assert!(matches!(error(429, false), StorageError::Server(_)));
        assert!(matches!(error(409, false), StorageError::Unknown(message) if message == "The server answered with HTTP 409."));
        assert!(error(503, false).is_transient() && !error(401, false).is_transient());
    }

    /// A server on 127.0.0.1 that answers every request with `answer` and
    /// hands back the requests' heads (request line and headers).
    async fn fake_server(answer: &'static str) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let sender = sender.clone();
                tokio::spawn(async move {
                    let mut data = Vec::new();
                    let mut buffer = [0u8; 65536];
                    while let Ok(read) = socket.read(&mut buffer).await {
                        if read == 0 {
                            break;
                        }
                        data.extend_from_slice(&buffer[..read]);
                        let text = String::from_utf8_lossy(&data);
                        if let Some(end) = text.find("\r\n\r\n") {
                            let head = text[..end].to_string();
                            let length: usize = head
                                .lines()
                                .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|value| value.trim().parse().unwrap_or(0)))
                                .unwrap_or(0);
                            if data.len() >= end + 4 + length {
                                let _ = sender.send(head);
                                let _ = socket.write_all(answer.as_bytes()).await;
                                break;
                            }
                        }
                    }
                });
            }
        });
        (format!("http://{address}/dav/"), receiver)
    }

    #[tokio::test]
    async fn sends_files_with_their_length_and_basic_auth() {
        let (endpoint, mut requests) = fake_server("HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n").await;
        let storage = provider(&endpoint);
        let file = std::env::temp_dir().join(format!("aktar-webdav-put-{}.txt", std::process::id()));
        std::fs::write(&file, b"twelve bytes").unwrap();
        let progress = Arc::new(Progress::default());
        storage.upload(&file, "a/b c.txt", "text/plain", progress.clone(), false).await.unwrap();
        let _ = std::fs::remove_file(&file);
        assert_eq!(progress.fraction(), Some(1.0));
        let mkcol = requests.recv().await.unwrap();
        let put = requests.recv().await.unwrap();
        assert!(mkcol.starts_with("MKCOL /dav/a/ HTTP/1.1"), "{mkcol}");
        assert!(put.starts_with("PUT /dav/a/b%20c.txt HTTP/1.1"), "{put}");
        let lower = put.to_ascii_lowercase();
        // A length, never chunked: nginx's DAV module refuses chunked PUTs.
        assert!(lower.contains("content-length: 12"), "{put}");
        assert!(!lower.contains("transfer-encoding"), "{put}");
        assert!(lower.contains("content-type: text/plain"), "{put}");
        // "me:secret"
        assert!(put.contains("authorization: Basic bWU6c2VjcmV0") || put.contains("Authorization: Basic bWU6c2VjcmV0"), "{put}");
    }

    #[tokio::test]
    async fn never_follows_a_redirect_with_the_password() {
        let (endpoint, mut requests) =
            fake_server("HTTP/1.1 307 Temporary Redirect\r\nLocation: https://elsewhere.example.com/dav/x\r\nContent-Length: 0\r\n\r\n").await;
        let storage = provider(&endpoint);
        let error = storage.put_bytes("x", b"data".to_vec(), "text/plain").await.unwrap_err();
        assert!(matches!(&error, StorageError::Redirected(location) if location == "https://elsewhere.example.com/dav/x"), "{error:?}");
        assert!(requests.recv().await.unwrap().starts_with("PUT /dav/x "));
        assert!(matches!(storage.delete("x").await, Err(StorageError::Redirected(_))));
        assert!(matches!(storage.copy("x", "y").await, Err(StorageError::Redirected(_))));
        assert!(matches!(storage.list("", None).await, Err(StorageError::Redirected(_))));
    }

    #[tokio::test]
    async fn reads_follow_same_origin_redirects_only() {
        // Always pointing at another path here: followed five times, then
        // reported.
        let (endpoint, mut requests) = fake_server("HTTP/1.1 302 Found\r\nLocation: /dav/elsewhere\r\nContent-Length: 0\r\n\r\n").await;
        let storage = provider(&endpoint);
        assert!(matches!(storage.get_small("x", 10).await, Err(StorageError::Redirected(location)) if location == "/dav/elsewhere"));
        let mut paths = Vec::new();
        while let Ok(head) = requests.try_recv() {
            paths.push(head.lines().next().unwrap_or_default().to_string());
        }
        assert_eq!(paths.len(), 1 + MAX_REDIRECTS, "{paths:?}");
        assert!(paths[0].starts_with("GET /dav/x ") && paths[1].starts_with("GET /dav/elsewhere "), "{paths:?}");
        // Writes never follow, not even to the same origin.
        assert!(matches!(storage.put_bytes("x", vec![1], "text/plain").await, Err(StorageError::Redirected(_))));
    }

    #[tokio::test]
    async fn lists_behind_a_proxy_that_rewrites_the_path() {
        let body = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">
<d:response><d:href>/internal/files/sub/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/internal/files/sub/a%20b.txt</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>3</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
<d:response><d:href>/internal/files/sub/f/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>
</d:multistatus>"#;
        let answer: &'static str = Box::leak(format!("HTTP/1.1 207 Multi-Status\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_boxed_str());
        let (endpoint, mut requests) = fake_server(answer).await;
        let listing = provider(&endpoint).list("sub", None).await.unwrap();
        assert_eq!(listing.prefix, "sub/");
        assert_eq!(listing.folders, vec!["sub/f/"]);
        assert_eq!(listing.objects.iter().map(|object| (object.key.as_str(), object.size)).collect::<Vec<_>>(), vec![("sub/a b.txt", 3)]);
        let head = requests.recv().await.unwrap();
        // A folder's URL ends in "/", and only its children are asked for.
        assert!(head.starts_with("PROPFIND /dav/sub/ HTTP/1.1"), "{head}");
        assert!(head.to_ascii_lowercase().contains("depth: 1"), "{head}");
    }

    #[tokio::test]
    async fn makes_each_folder_once() {
        let (endpoint, mut requests) = fake_server("HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n").await;
        let storage = provider(&endpoint);
        storage.put_bytes("a/b/1.txt", vec![1], "text/plain").await.unwrap();
        storage.put_bytes("a/b/2.txt", vec![2], "text/plain").await.unwrap();
        let mut lines = Vec::new();
        while let Ok(head) = requests.try_recv() {
            lines.push(head.lines().next().unwrap_or_default().split(' ').take(2).collect::<Vec<_>>().join(" "));
        }
        assert_eq!(lines, vec!["MKCOL /dav/a/", "MKCOL /dav/a/b/", "PUT /dav/a/b/1.txt", "PUT /dav/a/b/2.txt"]);
    }

    #[tokio::test]
    async fn tries_a_server_error_again() {
        let (endpoint, mut requests) = fake_server("HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n").await;
        let storage = provider(&endpoint);
        assert!(matches!(storage.object_info("x").await, Err(StorageError::Server(_))));
        let mut count = 0;
        while let Ok(head) = requests.try_recv() {
            assert!(head.to_ascii_lowercase().contains("accept-encoding: identity"), "{head}");
            count += 1;
        }
        assert_eq!(count, 1 + RETRY_DELAYS.len());
    }

    #[test]
    fn reads_http_dates() {
        assert_eq!(http_date_millis("Thu, 26 Nov 2026 08:12:31 GMT"), Some(1_795_680_751_000));
        assert_eq!(http_date_millis("yesterday"), None);
    }
}

/// Against a real WebDAV server, ignored by default. `rclone serve webdav
/// <dir> --addr 127.0.0.1:8090 --user u --pass p`, then
/// `AKTAR_TEST_WEBDAV_URL=http://127.0.0.1:8090/aktar cargo test webdav -- --ignored`
/// (the folder in the URL is created first). `AKTAR_TEST_WEBDAV_USER` and
/// `AKTAR_TEST_WEBDAV_PASSWORD` default to u and p.
#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::storage::Backend;

    fn storage(password: Option<&str>) -> Option<Backend> {
        let endpoint = std::env::var("AKTAR_TEST_WEBDAV_URL").ok()?;
        let config: DestinationConfig = serde_json::from_value(serde_json::json!({
            "id": "WEBDAV-TEST", "name": "Test", "preset": "webdav", "endpoint": endpoint, "region": "", "bucket": "",
            "publicBaseURL": endpoint, "objectPathTemplate": "{filename}.{ext}", "forcePathStyle": false,
        }))
        .unwrap();
        let credentials = StorageCredentials {
            access_key_id: std::env::var("AKTAR_TEST_WEBDAV_USER").unwrap_or_else(|_| "u".into()),
            secret_access_key: password.map(str::to_string).unwrap_or_else(|| std::env::var("AKTAR_TEST_WEBDAV_PASSWORD").unwrap_or_else(|_| "p".into())),
            session_token: None,
            cloudflare_token: None,
            short_link_token: None,
        };
        Some(crate::storage::backend(&config, &credentials).unwrap())
    }

    /// The folder the URL names, made (with its parents) from the server's root.
    async fn prepare() -> Option<Backend> {
        let endpoint = std::env::var("AKTAR_TEST_WEBDAV_URL").ok()?;
        let url = url::Url::parse(&endpoint).unwrap();
        let root = format!("{}://{}/", url.scheme(), url.host_str().unwrap().to_string() + &url.port().map(|port| format!(":{port}")).unwrap_or_default());
        let config: DestinationConfig = serde_json::from_value(serde_json::json!({
            "id": "ROOT", "name": "Root", "preset": "webdav", "endpoint": root, "region": "", "bucket": "",
            "publicBaseURL": root, "objectPathTemplate": "{uuid}.{ext}", "forcePathStyle": false,
        }))
        .unwrap();
        let credentials = StorageCredentials {
            access_key_id: std::env::var("AKTAR_TEST_WEBDAV_USER").unwrap_or_else(|_| "u".into()),
            secret_access_key: std::env::var("AKTAR_TEST_WEBDAV_PASSWORD").unwrap_or_else(|_| "p".into()),
            session_token: None,
            cloudflare_token: None,
            short_link_token: None,
        };
        let root = crate::storage::backend(&config, &credentials).unwrap();
        root.create_folder(&decoded(url.path())).await.unwrap();
        storage(None)
    }

    #[tokio::test]
    #[ignore]
    async fn webdav_round_trip() {
        let Some(storage) = prepare().await else { return };
        assert!(storage.config().kind() == crate::destinations::StorageKind::Webdav);

        let result = storage.test_connection().await.unwrap();
        assert!(result.bucket_reachable && result.writable);
        // rclone wants the password for reading too, so the public link
        // is probed and answers 401.
        assert_eq!(result.public_link, Some(crate::storage::PublicLinkCheck::Status { code: 401 }));

        let directory = std::env::temp_dir().join(format!("aktar-webdav-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("small ü.txt");
        std::fs::write(&file, b"hello from aktar").unwrap();
        let progress = Arc::new(Progress::default());
        let key = "live/deep/er/small ü+(1).txt";
        let uploaded = storage.upload(&file, key, "text/plain", progress.clone(), false).await.unwrap();
        assert_eq!(uploaded.byte_size, 16);
        assert_eq!(progress.fraction(), Some(1.0));
        assert!(uploaded.public_url.ends_with("/live/deep/er/small%20%C3%BC%2B(1).txt"), "{}", uploaded.public_url);
        let fetched = reqwest::Client::new().get(&uploaded.public_url).basic_auth("u", Some("p")).send().await.unwrap();
        assert_eq!(fetched.text().await.unwrap(), "hello from aktar");

        assert!(storage.object_exists(key).await.unwrap());
        assert!(!storage.object_exists("live/deep/er/small").await.unwrap());
        let info = storage.object_info(key).await.unwrap().unwrap();
        assert_eq!(info.size, 16);
        assert!(info.last_modified.is_some_and(|date| (date - crate::util::now_millis()).abs() < 600_000));
        assert_eq!(storage.get_small(key, 100).await.unwrap().map(|(data, _)| data), Some(b"hello from aktar".to_vec()));
        assert!(storage.get_small("live/missing.txt", 100).await.unwrap().is_none());
        assert!(storage.get_small(key, 4).await.is_err());
        let downloaded = directory.join("downloaded.txt");
        assert!(storage.download(key, &downloaded, 100).await.unwrap());
        assert_eq!(std::fs::read(&downloaded).unwrap(), b"hello from aktar");
        assert!(!storage.download("live/missing.txt", &downloaded, 100).await.unwrap());

        storage.put_bytes("live/thumbs/a.webp", vec![1, 2, 3], "image/webp").await.unwrap();
        storage.put_page("live/page.html", "<p>hi</p>".into()).await.unwrap();
        storage.create_folder("live/empty/nested/").await.unwrap();

        let level = storage.list("live/", None).await.unwrap();
        assert_eq!(level.prefix, "live/");
        assert_eq!(level.folders, vec!["live/deep/", "live/empty/", "live/thumbs/"]);
        assert_eq!(level.objects.iter().map(|object| object.key.as_str()).collect::<Vec<_>>(), vec!["live/page.html"]);
        assert!(level.next_continuation_token.is_none());
        assert!(storage.list("live/nothing-here/", None).await.unwrap().objects.is_empty());

        let deep = storage.list_recursively("live/", None).await.unwrap();
        assert!(deep.folders.contains(&"live/empty/nested/".to_string()));
        let mut keys: Vec<&str> = deep.objects.iter().map(|object| object.key.as_str()).collect();
        keys.sort();
        assert_eq!(keys, vec![key, "live/page.html", "live/thumbs/a.webp"]);
        assert_eq!(deep.objects.iter().find(|object| object.key == key).unwrap().size, 16);

        // Numbered names, moves and renames, as the bucket browser does them.
        let numbered = crate::bucket::available_key(&storage, "small ü+(1).txt", "live/deep/er/", &[]).await.unwrap();
        assert_eq!(numbered, "live/deep/er/small ü+(1) 2.txt");
        assert!(crate::bucket::move_object(&storage, key, "moved/into/new folder/renamed.txt", &[]).await.is_ok());
        assert!(!storage.object_exists(key).await.unwrap());
        assert_eq!(storage.get_small("moved/into/new folder/renamed.txt", 100).await.unwrap().unwrap().0, b"hello from aktar");
        storage.copy("moved/into/new folder/renamed.txt", "live/copy.txt").await.unwrap();
        assert!(storage.object_exists("live/copy.txt").await.unwrap());

        // A folder with files in it isn't deleted as a whole; an empty one is.
        storage.delete("live/").await.unwrap();
        assert!(storage.object_exists("live/copy.txt").await.unwrap());
        storage.delete("live/empty/nested/").await.unwrap();
        assert!(!storage.list("live/empty/", None).await.unwrap().folders.contains(&"live/empty/nested/".to_string()));
        storage
            .delete_many(&["live/copy.txt".into(), "live/page.html".into(), "live/thumbs/a.webp".into(), "moved/into/new folder/renamed.txt".into()])
            .await
            .unwrap();
        // Deleting what isn't there is done.
        storage.delete("live/copy.txt").await.unwrap();
        assert!(!storage.object_exists("live/copy.txt").await.unwrap());
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Bigger than `SINGLE_UPLOAD_LIMIT`: still one streamed PUT, with
    /// progress all the way.
    #[tokio::test]
    #[ignore]
    async fn webdav_big_upload() {
        let Some(storage) = prepare().await else { return };
        let directory = std::env::temp_dir().join(format!("aktar-webdav-big-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("big.bin");
        let size = crate::storage::SINGLE_UPLOAD_LIMIT + 3 * 1024 * 1024 + 17;
        {
            use std::io::Write;
            let mut writer = std::io::BufWriter::new(std::fs::File::create(&file).unwrap());
            let block: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
            let mut written = 0;
            while written < size {
                let take = (size - written).min(block.len() as u64) as usize;
                writer.write_all(&block[..take]).unwrap();
                written += take as u64;
            }
        }
        let progress = Arc::new(Progress::default());
        let uploaded = storage.upload(&file, "big/one put.bin", "application/octet-stream", progress.clone(), false).await.unwrap();
        assert_eq!(uploaded.byte_size as u64, size);
        assert_eq!(progress.fraction(), Some(1.0));
        assert_eq!(storage.object_info("big/one put.bin").await.unwrap().unwrap().size as u64, size);
        let back = directory.join("back.bin");
        assert!(storage.download("big/one put.bin", &back, size).await.unwrap());
        assert_eq!(std::fs::metadata(&back).unwrap().len(), size);
        assert_eq!(std::fs::read(&back).unwrap(), std::fs::read(&file).unwrap());
        storage.delete("big/one put.bin").await.unwrap();
        storage.delete("big/").await.unwrap();
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    #[ignore]
    async fn webdav_refuses_what_it_should() {
        let Some(_) = prepare().await else { return };
        let wrong = storage(Some("wrong password")).unwrap();
        assert!(matches!(wrong.test_connection().await, Err(StorageError::InvalidLogin)));

        let endpoint = std::env::var("AKTAR_TEST_WEBDAV_URL").unwrap();
        let config: DestinationConfig = serde_json::from_value(serde_json::json!({
            "id": "MISSING", "name": "Missing", "preset": "webdav", "endpoint": format!("{}/no/such/folder", endpoint.trim_end_matches('/')),
            "region": "", "bucket": "", "publicBaseURL": endpoint, "objectPathTemplate": "{uuid}.{ext}", "forcePathStyle": false,
        }))
        .unwrap();
        let credentials = StorageCredentials {
            access_key_id: std::env::var("AKTAR_TEST_WEBDAV_USER").unwrap_or_else(|_| "u".into()),
            secret_access_key: std::env::var("AKTAR_TEST_WEBDAV_PASSWORD").unwrap_or_else(|_| "p".into()),
            session_token: None,
            cloudflare_token: None,
            short_link_token: None,
        };
        let missing = crate::storage::backend(&config, &credentials).unwrap();
        assert!(matches!(missing.test_connection().await, Err(StorageError::FolderNotFound)));
        assert!(matches!(missing.list("", None).await, Err(StorageError::FolderNotFound)));
    }
}
