//! Durable OAuth credential storage with cross-process refresh serialization.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};

use dar_extension_sdk::BridgeSecrets;

use fs2::FileExt;
use rmcp::transport::{AuthError, CredentialRefreshGuard, CredentialStore, StoredCredentials};

#[derive(Clone, Debug)]
pub struct FileCredentialStore {
    file: PathBuf,
    lock: PathBuf,
    secrets: Option<Arc<BridgeSecrets>>,
}

impl FileCredentialStore {
    pub fn new(root: &Path, server: &str) -> Result<Self, AuthError> {
        let root = root.canonicalize().map_err(store_error)?;
        let data = root.join("data");
        fs::create_dir_all(&data).map_err(store_error)?;
        reject_symlink(&data)?;
        let dir = data.join("mcp-auth");
        // Tolerates concurrent first-boot creation by sibling bridge processes.
        fs::create_dir_all(&dir).map_err(store_error)?;
        reject_symlink(&dir)?;
        let canonical_dir = dir.canonicalize().map_err(store_error)?;
        if !canonical_dir.starts_with(&root) {
            return Err(store_error("credential directory escapes agent root"));
        }
        set_mode(&canonical_dir, 0o700)?;
        let file = canonical_dir.join(format!("{server}.json"));
        let lock = canonical_dir.join(format!("{server}.lock"));
        if file.exists() {
            reject_symlink(&file)?;
        }
        if lock.exists() {
            reject_symlink(&lock)?;
        }
        Ok(Self {
            file,
            lock,
            secrets: None,
        })
    }

    pub fn with_secrets(mut self, secrets: Arc<BridgeSecrets>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    pub fn secret_values(&self) -> Result<Vec<String>, AuthError> {
        let Some(value) = self.read_value()? else {
            return Ok(Vec::new());
        };
        Ok(token_values(&value))
    }

    fn read_value(&self) -> Result<Option<serde_json::Value>, AuthError> {
        if self.file.exists() {
            reject_symlink(&self.file)?;
        }
        match fs::read(&self.file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(store_error),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(store_error(error)),
        }
    }
}

struct FileGuard(File);
impl Drop for FileGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[async_trait::async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let value = self.read_value()?;
        // Another bridge process may have rotated tokens; mask them here too.
        if let (Some(secrets), Some(value)) = (&self.secrets, &value) {
            secrets.extend(token_values(value));
        }
        value
            .map(serde_json::from_value)
            .transpose()
            .map_err(store_error)
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        if self.file.exists() {
            reject_symlink(&self.file)?;
        }
        let value = serde_json::to_value(&credentials).map_err(store_error)?;
        if let Some(secrets) = &self.secrets {
            secrets.extend(token_values(&value));
        }
        let dir = self
            .file
            .parent()
            .ok_or_else(|| store_error("credential file has no parent"))?;
        let mut temp = tempfile::Builder::new()
            .prefix(".credentials-")
            .tempfile_in(dir)
            .map_err(store_error)?;
        set_mode(temp.path(), 0o600)?;
        use std::io::Write as _;
        temp.write_all(&serde_json::to_vec_pretty(&value).map_err(store_error)?)
            .map_err(store_error)?;
        temp.as_file().sync_all().map_err(store_error)?;
        temp.persist(&self.file).map_err(store_error)?;
        set_mode(&self.file, 0o600)
    }

    async fn clear(&self) -> Result<(), AuthError> {
        match fs::remove_file(&self.file) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(store_error(e)),
        }
    }

    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        if self.lock.exists() {
            reject_symlink(&self.lock)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(&self.lock).map_err(store_error)?;
        set_mode(&self.lock, 0o600)?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Ok(Some(CredentialRefreshGuard::new(FileGuard(file)))),
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    return Err(store_error(
                        "timed out acquiring OAuth credential refresh lock",
                    ))
                }
                Err(error) => return Err(store_error(error)),
            }
        }
    }
}

fn reject_symlink(path: &Path) -> Result<(), AuthError> {
    if fs::symlink_metadata(path)
        .map_err(store_error)?
        .file_type()
        .is_symlink()
    {
        return Err(store_error(format!(
            "refusing symlinked credential path {}",
            path.display()
        )));
    }
    Ok(())
}

/// Secret material in a stored credential: tokens and any client secret. Scopes,
/// issuer, and token type stay visible (masking them would corrupt tool names).
fn token_values(value: &serde_json::Value) -> Vec<String> {
    [
        "/token_response/access_token",
        "/token_response/refresh_token",
        "/token_response/id_token",
        "/client_secret",
    ]
    .iter()
    .filter_map(|pointer| value.pointer(pointer).and_then(serde_json::Value::as_str))
    .filter(|token| !token.is_empty())
    .map(str::to_owned)
    .collect()
}
fn store_error(error: impl std::fmt::Display) -> AuthError {
    AuthError::CredentialStoreError(error.to_string())
}
#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), AuthError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(store_error)
}
#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), AuthError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::CredentialStore;
    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_auth_directory() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("data")).unwrap();
        symlink(outside.path(), root.path().join("data/mcp-auth")).unwrap();
        assert!(FileCredentialStore::new(root.path(), "linear").is_err());
    }

    #[tokio::test]
    async fn only_token_values_become_secrets_including_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let writer = FileCredentialStore::new(dir.path(), "linear").unwrap();
        let credentials: StoredCredentials = serde_json::from_value(serde_json::json!({
            "client_id": "id",
            "token_response": { "access_token": "at-1", "token_type": "bearer", "refresh_token": "rt-1" },
            "granted_scopes": ["read"],
            "token_received_at": null,
            "issuer": null
        }))
        .unwrap();
        writer.save(credentials).await.unwrap();

        // A sibling bridge process loading rotated tokens registers them too.
        let secrets = Arc::new(BridgeSecrets::default());
        let reader = FileCredentialStore::new(dir.path(), "linear")
            .unwrap()
            .with_secrets(secrets.clone());
        reader.load().await.unwrap().unwrap();
        let mut values = secrets.values();
        values.sort();
        assert_eq!(values, vec!["at-1", "rt-1"]);
    }

    #[test]
    fn concurrent_first_creation_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let root = dir.path().to_path_buf();
                std::thread::spawn(move || FileCredentialStore::new(&root, "linear").map(|_| ()))
            })
            .collect();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn permissions_roundtrip_clear_and_lock() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileCredentialStore::new(dir.path(), "linear").unwrap();
        let credentials: StoredCredentials = serde_json::from_value(serde_json::json!({"client_id":"id","token_response":null,"granted_scopes":[],"token_received_at":null,"issuer":null})).unwrap();
        store.save(credentials.clone()).await.unwrap();
        assert!(store.load().await.unwrap().is_some());
        let _guard = store.acquire_refresh_guard().await.unwrap().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(dir.path().join("data/mcp-auth"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&store.file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
    }
}
