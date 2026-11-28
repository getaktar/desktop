use super::*;
use aktar_sftp_core::keys;
use aktar_sftp_core::PrivateKey;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// An ed25519 key from a fixed seed, in OpenSSH format, for tests that
/// don't need `ssh-keygen`. Never a real key.
fn seeded_key(seed: u8) -> PrivateKey {
    let pair = keys::ssh_key::private::Ed25519Keypair::from_seed(&[seed; 32]);
    PrivateKey::from(pair)
}

fn openssh(key: &PrivateKey) -> String {
    key.to_openssh(keys::ssh_key::LineEnding::LF).unwrap().to_string()
}

/// A key made by `ssh-keygen` (`-t` type, `-N` passphrase), or None when
/// it isn't installed.
fn keygen(kind: &str, bits: Option<&str>, passphrase: &str) -> Option<(String, String)> {
    let directory = std::env::temp_dir().join(format!("aktar-sftp-key-{}-{}", std::process::id(), crate::util::new_id()));
    std::fs::create_dir_all(&directory).ok()?;
    let path = directory.join("key");
    let mut command = std::process::Command::new("ssh-keygen");
    command.args(["-q", "-t", kind, "-N", passphrase, "-C", "aktar-test", "-f"]).arg(&path);
    if let Some(bits) = bits {
        command.args(["-b", bits]);
    }
    let status = command.stdin(std::process::Stdio::null()).output().ok()?.status;
    if !status.success() {
        return None;
    }
    let private = std::fs::read_to_string(&path).ok()?;
    let public = std::fs::read_to_string(path.with_extension("pub")).ok()?;
    let _ = std::fs::remove_dir_all(&directory);
    Some((private, public))
}

#[test]
fn words_errors() {
    use aktar_sftp_core::Error as E;
    assert!(matches!(storage_error(E::PermissionDenied), StorageError::Refused));
    assert!(matches!(storage_error(E::FolderNotFound("/srv".into())), StorageError::RemoteFolderNotFound(folder) if folder == "/srv"));
    assert!(matches!(storage_error(E::OutOfSpace), StorageError::OutOfSpace));
    assert!(matches!(storage_error(E::Dropped("lost".into())), StorageError::Connection(_)));
    assert!(matches!(storage_error(E::ServerTimeout), StorageError::Connection(_)));
    assert!(matches!(storage_error(E::AuthFailed), StorageError::InvalidSshLogin));
    assert!(matches!(storage_error(E::KeyPassphrase), StorageError::KeyPassphrase));
    assert!(matches!(storage_error(E::UnreadableKey), StorageError::UnreadableKey));
    assert!(matches!(storage_error(E::UnsupportedKey("ssh-dss".into())), StorageError::UnsupportedKey(name) if name == "ssh-dss"));
    assert!(matches!(storage_error(E::UnreadableHostKey), StorageError::HostKeyChanged(fingerprint) if fingerprint == "?"));
    let seen = host_key_text(seeded_key(5).public_key());
    assert!(matches!(storage_error(E::HostKeyChanged { stored: None, seen: Some(seen.clone()) }), StorageError::HostKeyChanged(fingerprint) if Some(fingerprint.clone()) == fingerprint_of(&seen)));
    let refused = storage_error(E::Refused { host: "files.example.com:2222".into() });
    assert!(matches!(&refused, StorageError::Unreachable(message) if message.contains("files.example.com:2222")));
    // Network trouble is transient; signing in and permissions aren't.
    assert!(refused.is_transient());
    assert!(matches!(as_upload_error(storage_error(E::Dropped("reset".into()))), StorageError::Network(_)));
    assert!(StorageError::HostKeyChanged("SHA256:abc".into()).to_string().contains("SHA256:abc"));
}

#[test]
fn backends_refuse_what_cant_be_right() {
    let config = |endpoint: &str, folder: &str| -> DestinationConfig {
        serde_json::from_value(serde_json::json!({
            "id": "S", "name": "Server", "preset": "sftp", "endpoint": endpoint, "region": "", "bucket": folder,
            "publicBaseURL": "https://files.example.com", "objectPathTemplate": "{uuid}.{ext}", "forcePathStyle": false,
        }))
        .unwrap()
    };
    let credentials = StorageCredentials { access_key_id: "me".into(), secret_access_key: "pw".into(), ..Default::default() };
    assert!(SftpProvider::new(config("files.example.com", "/srv"), credentials.clone()).is_ok());
    assert!(SftpProvider::new(config("", "/srv"), credentials.clone()).is_err());
    assert!(SftpProvider::new(config("host", "/srv/../etc"), credentials.clone()).is_err());
    let provider = SftpProvider::new(config("host:2200", "/srv"), credentials.clone()).unwrap();
    assert!(!provider.client.uses_key());
    assert_eq!(provider.client.display_host(), "host:2200");
    let with_key = StorageCredentials { secret_access_key: openssh(&seeded_key(3)), key_passphrase: Some(String::new()), ..credentials.clone() };
    let provider = SftpProvider::new(config("host", ""), with_key).unwrap();
    assert!(provider.client.uses_key() && !provider.client.has_passphrase());
    // Another key, password or host key is another connection.
    let a = SftpProvider::new(config("host", "/a"), credentials.clone()).unwrap();
    let b = SftpProvider::new(config("host", "/b"), credentials.clone()).unwrap();
    assert_eq!(a.client.pool_key(), b.client.pool_key());
    let other = SftpProvider::new(config("host", "/a"), StorageCredentials { secret_access_key: "other".into(), ..credentials.clone() }).unwrap();
    assert_ne!(a.client.pool_key(), other.client.pool_key());
    let mut pinned = config("host", "/a");
    pinned.ssh_host_key = Some(host_key_text(seeded_key(4).public_key()));
    let pinned = SftpProvider::new(pinned, credentials.clone()).unwrap();
    assert_ne!(a.client.pool_key(), pinned.client.pool_key());
    // A saved key that can't be read is never "any key".
    let mut unreadable = config("host", "/a");
    unreadable.ssh_host_key = Some("ssh-ed25519 garbage".into());
    let unreadable = SftpProvider::new(unreadable, credentials).unwrap();
    assert!(unreadable.client.has_unreadable_host_key());
}

#[test]
fn credentials_keep_the_passphrase_only_when_set() {
    let plain = StorageCredentials { access_key_id: "me".into(), secret_access_key: "pw".into(), ..Default::default() };
    let json = serde_json::to_value(&plain).unwrap();
    assert!(json.get("keyPassphrase").is_none());
    let with = StorageCredentials { key_passphrase: Some("pass".into()), ..plain };
    let json = serde_json::to_value(&with).unwrap();
    assert_eq!(json["keyPassphrase"], "pass");
    let back: StorageCredentials = serde_json::from_value(json).unwrap();
    assert_eq!(back.key_passphrase.as_deref(), Some("pass"));
}

/// Against `rclone serve sftp`, ignored by default: `cargo test sftp --
/// --ignored` starts its own servers on free ports (rclone and ssh-keygen
/// have to be installed; the tests are skipped without them).
mod live {
    use super::*;
    use crate::storage::Backend;

    struct Server {
        child: std::process::Child,
        port: u16,
        directory: std::path::PathBuf,
        /// The server's host key, as stored.
        host_key: String,
        /// A client key it accepts.
        client_key: String,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn installed(program: &str) -> bool {
        std::process::Command::new(program).arg("--version").output().is_ok() || std::process::Command::new("which").arg(program).output().is_ok_and(|output| output.status.success())
    }

    /// `rclone serve sftp` on a folder of its own, with user u, password p
    /// and one authorized key (the client key is encrypted with
    /// `client_passphrase` when it's not empty).
    async fn start(client_passphrase: &str) -> Option<Server> {
        if !installed("rclone") || keygen("ed25519", None, "").is_none() {
            eprintln!("rclone or ssh-keygen isn't installed; skipped");
            return None;
        }
        let directory = std::env::temp_dir().join(format!("aktar-sftp-live-{}", crate::util::new_id()));
        std::fs::create_dir_all(directory.join("served/base")).unwrap();
        let (host_private, host_public) = keygen("ed25519", None, "").unwrap();
        std::fs::write(directory.join("host_key"), host_private).unwrap();
        let (client_private, client_public) = keygen("ed25519", None, client_passphrase).unwrap();
        std::fs::write(directory.join("authorized_keys"), client_public).unwrap();
        let port = free_port();
        let child = std::process::Command::new("rclone")
            .args(["serve", "sftp"])
            .arg(directory.join("served"))
            .args(["--addr", &format!("127.0.0.1:{port}"), "--user", "u", "--pass", "p", "--key"])
            .arg(directory.join("host_key"))
            .arg("--authorized-keys")
            .arg(directory.join("authorized_keys"))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let host_key = host_key_text(&parse_host_key(&host_public).unwrap());
        Some(Server { child, port, directory, host_key, client_key: client_private })
    }

    fn config(port: u16, folder: &str, host_key: Option<&str>) -> DestinationConfig {
        let mut config: DestinationConfig = serde_json::from_value(serde_json::json!({
            "id": format!("SFTP-TEST-{}", crate::util::new_id()), "name": "Test", "preset": "sftp", "endpoint": format!("127.0.0.1:{port}"),
            "region": "", "bucket": folder, "publicBaseURL": "http://127.0.0.1:9/files", "objectPathTemplate": "{filename}.{ext}", "forcePathStyle": false,
        }))
        .unwrap();
        config.ssh_host_key = host_key.map(str::to_string);
        config.sanitize();
        config
    }

    fn password(password: &str) -> StorageCredentials {
        StorageCredentials { access_key_id: "u".into(), secret_access_key: password.into(), ..Default::default() }
    }

    fn backend(config: &DestinationConfig, credentials: &StorageCredentials) -> Backend {
        crate::storage::backend(config, credentials).unwrap()
    }

    #[tokio::test]
    #[ignore]
    async fn sftp_round_trip() {
        let Some(server) = start("").await else { return };
        let config = config(server.port, "/base", None);
        let storage = backend(&config, &password("p"));
        assert_eq!(storage.config().kind(), crate::destinations::StorageKind::Sftp);

        let result = storage.test_connection().await.unwrap();
        assert!(result.bucket_reachable && result.writable, "{result:?}");
        assert_eq!(result.host_key.as_deref(), Some(server.host_key.as_str()));
        assert_eq!(result.host_key_fingerprint, fingerprint_of(&server.host_key));
        assert!(result.previous_host_key_fingerprint.is_none());
        assert_eq!(result.public_link, Some(crate::storage::PublicLinkCheck::NoResponse));

        let directory = server.directory.join("local");
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join("small ü.txt");
        std::fs::write(&file, b"hello from aktar").unwrap();
        let progress = Arc::new(Progress::default());
        let key = "live/deep/er/small ü+(1).txt";
        let uploaded = storage.upload(&file, key, "text/plain", progress.clone(), false).await.unwrap();
        assert_eq!(uploaded.byte_size, 16);
        assert_eq!(progress.fraction(), Some(1.0));
        assert!(uploaded.public_url.ends_with("/live/deep/er/small%20%C3%BC%2B(1).txt"), "{}", uploaded.public_url);
        let on_disk = server.directory.join("served/base").join(key);
        assert_eq!(std::fs::read(&on_disk).unwrap(), b"hello from aktar");
        // No part file left next to it.
        let names: Vec<String> = std::fs::read_dir(on_disk.parent().unwrap()).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["small ü+(1).txt"]);

        assert!(storage.object_exists(key).await.unwrap());
        assert!(!storage.object_exists("live/deep/er/small").await.unwrap());
        assert!(!storage.object_exists("live/deep/").await.unwrap(), "a folder isn't a file");
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

        // Replacing a file goes over the old one.
        storage.put_bytes(key, b"replaced".to_vec(), "text/plain").await.unwrap();
        assert_eq!(std::fs::read(&on_disk).unwrap(), b"replaced");

        storage.put_bytes("live/thumbs/a.webp", vec![1, 2, 3], "image/webp").await.unwrap();
        storage.put_page("live/page.html", "<p>hi</p>".into()).await.unwrap();
        storage.create_folder("live/empty/nested/").await.unwrap();
        // A part file left by a crash isn't listed.
        std::fs::write(server.directory.join("served/base/live/.stale.png.aktar-part"), b"x").unwrap();

        let level = storage.list("live/", None).await.unwrap();
        assert_eq!(level.prefix, "live/");
        assert_eq!(level.folders, vec!["live/deep/", "live/empty/", "live/thumbs/"]);
        assert_eq!(level.objects.iter().map(|object| object.key.as_str()).collect::<Vec<_>>(), vec!["live/page.html"]);
        assert!(storage.list("live/nothing-here/", None).await.unwrap().objects.is_empty());

        let deep = storage.list_recursively("live/", None).await.unwrap();
        assert!(deep.folders.contains(&"live/empty/nested/".to_string()));
        let mut keys: Vec<&str> = deep.objects.iter().map(|object| object.key.as_str()).collect();
        keys.sort();
        assert_eq!(keys, vec![key, "live/page.html", "live/thumbs/a.webp"]);

        // Numbered names, moves (a rename on the server) and copies.
        let numbered = crate::bucket::available_key(&storage, "small ü+(1).txt", "live/deep/er/", &[]).await.unwrap();
        assert_eq!(numbered, "live/deep/er/small ü+(1) 2.txt");
        assert!(crate::bucket::move_object(&storage, key, "moved/into/new folder/renamed.txt", &[]).await.is_ok());
        assert!(!storage.object_exists(key).await.unwrap());
        assert_eq!(storage.get_small("moved/into/new folder/renamed.txt", 100).await.unwrap().unwrap().0, b"replaced");
        storage.copy("moved/into/new folder/renamed.txt", "live/copy.txt").await.unwrap();
        assert_eq!(storage.get_small("live/copy.txt", 100).await.unwrap().unwrap().0, b"replaced");

        // A folder with files in it isn't deleted as a whole; an empty one is.
        storage.delete("live/").await.unwrap();
        assert!(storage.object_exists("live/copy.txt").await.unwrap());
        storage.delete("live/empty/nested/").await.unwrap();
        assert!(!storage.list("live/empty/", None).await.unwrap().folders.contains(&"live/empty/nested/".to_string()));
        storage
            .delete_many(&["live/copy.txt".into(), "live/page.html".into(), "live/thumbs/a.webp".into(), "moved/into/new folder/renamed.txt".into()])
            .await
            .unwrap();
        storage.delete("live/copy.txt").await.unwrap();
        assert!(!storage.object_exists("live/copy.txt").await.unwrap());
        // Nothing outside the folder.
        assert!(storage.get_small("../outside.txt", 100).await.is_err());
    }

    #[tokio::test]
    #[ignore]
    async fn sftp_big_upload_and_reuse() {
        let Some(server) = start("").await else { return };
        let config = config(server.port, "base", None);
        let storage = backend(&config, &password("p"));
        let file = server.directory.join("big.bin");
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
        let uploaded = storage.upload(&file, "big/one stream.bin", "application/octet-stream", progress.clone(), false).await.unwrap();
        assert_eq!(uploaded.byte_size as u64, size);
        assert_eq!(progress.fraction(), Some(1.0));
        let back = server.directory.join("back.bin");
        assert!(storage.download("big/one stream.bin", &back, size).await.unwrap());
        assert_eq!(std::fs::read(&back).unwrap(), std::fs::read(&file).unwrap());
        // The same connection served all of that, and stays for the next one.
        assert!(aktar_sftp_core::pooled_connections() >= 1);
        storage.delete("big/one stream.bin").await.unwrap();
        storage.delete("big/").await.unwrap();
        // A dropped connection is replaced on the next operation.
        aktar_sftp_core::close_pooled_connections().await;
        assert!(!storage.object_exists("big/one stream.bin").await.unwrap());
    }

    #[tokio::test]
    #[ignore]
    async fn sftp_signs_in_with_keys() {
        let Some(server) = start("key words").await else { return };
        let config = config(server.port, "/base", Some(&server.host_key));
        let key = |passphrase: Option<&str>| StorageCredentials {
            access_key_id: "u".into(),
            secret_access_key: server.client_key.clone(),
            key_passphrase: passphrase.map(str::to_string),
            ..Default::default()
        };
        let storage = backend(&config, &key(Some("key words")));
        let result = storage.test_connection().await.unwrap();
        assert!(result.writable, "{result:?}");
        storage.put_bytes("by-key.txt", b"key".to_vec(), "text/plain").await.unwrap();
        assert!(storage.object_exists("by-key.txt").await.unwrap());
        storage.delete("by-key.txt").await.unwrap();

        assert!(matches!(backend(&config, &key(None)).test_connection().await, Err(StorageError::KeyPassphrase)));
        assert!(matches!(backend(&config, &key(Some("wrong"))).test_connection().await, Err(StorageError::KeyPassphrase)));
        // Another key the server doesn't know.
        let stranger = StorageCredentials { secret_access_key: openssh(&seeded_key(9)), key_passphrase: None, ..key(None) };
        assert!(matches!(backend(&config, &stranger).test_connection().await, Err(StorageError::InvalidSshLogin)));
        assert!(matches!(backend(&config, &password("wrong")).test_connection().await, Err(StorageError::InvalidSshLogin)));
        let missing = self::config(server.port, "/no/such/folder", Some(&server.host_key));
        assert!(matches!(backend(&missing, &password("p")).test_connection().await, Err(StorageError::RemoteFolderNotFound(_))));
        assert!(matches!(backend(&missing, &password("p")).put_bytes("a/b.txt", vec![1], "text/plain").await, Err(StorageError::RemoteFolderNotFound(_))));
        let closed = self::config(free_port(), "/base", None);
        assert!(matches!(backend(&closed, &password("p")).test_connection().await, Err(StorageError::Unreachable(_))));
    }

    #[tokio::test]
    #[ignore]
    async fn sftp_refuses_a_changed_host_key() {
        let Some(server) = start("").await else { return };
        let Some(other) = start("").await else { return };
        // The first connection takes the key it sees and keeps it.
        let learned: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
        let sink = learned.clone();
        set_host_key_listener(move |id, _endpoint, key| sink.lock().unwrap().push((id.to_string(), key.to_string())));
        let fresh = config(server.port, "/base", None);
        let storage = backend(&fresh, &password("p"));
        storage.put_bytes("tofu.txt", b"x".to_vec(), "text/plain").await.unwrap();
        // The listener is set once per test binary; when another test set
        // it first, the learned key is still remembered here.
        if let Some((id, key)) = learned.lock().unwrap().first() {
            assert_eq!(id, &fresh.id);
            assert_eq!(key, &server.host_key);
        }
        assert_eq!(aktar_sftp_core::learned_host_key(&fresh.id, "127.0.0.1", server.port), Some(server.host_key.clone()));

        // The same destination pointed at a server with another key.
        let moved = config(other.port, "/base", Some(&server.host_key));
        let storage = backend(&moved, &password("p"));
        let refused = storage.put_bytes("x.txt", b"x".to_vec(), "text/plain").await;
        match refused {
            Err(StorageError::HostKeyChanged(fingerprint)) => assert_eq!(Some(fingerprint), fingerprint_of(&other.host_key)),
            other => panic!("{other:?}"),
        }
        assert!(!other.directory.join("served/base/x.txt").exists());
        // Test Connection shows both, signs in to neither, and saving the
        // new key makes it work.
        let result = storage.test_connection().await.unwrap();
        assert!(!result.writable && !result.bucket_reachable);
        assert_eq!(result.previous_host_key_fingerprint, fingerprint_of(&server.host_key));
        assert_eq!(result.host_key_fingerprint, fingerprint_of(&other.host_key));
        assert_eq!(result.host_key.as_deref(), Some(other.host_key.as_str()));
        let saved = config(other.port, "/base", result.host_key.as_deref());
        backend(&saved, &password("p")).put_bytes("x.txt", b"x".to_vec(), "text/plain").await.unwrap();
        assert!(other.directory.join("served/base/x.txt").exists());
    }
}
