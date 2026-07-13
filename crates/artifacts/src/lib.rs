//! Immutable, verified artifact storage.
//!
//! Callers stage only regular files below an [`ExportRoot`]. The store copies
//! bytes into its private vault and later verifies size and SHA-256 before
//! exposing a reader. Public values never reveal filesystem paths.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const DATA_FILE: &str = "data";
const METADATA_FILE: &str = "metadata.json";
const DELIVERIES_FILE: &str = "deliveries.json";
const DELIVERY_LOCK_FILE: &str = "delivery.lock";
const DELIVERY_CLAIM_LEASE_MS: u64 = 5 * 60 * 1000;

/// Opaque artifact identifier. It contains no filesystem location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ArtifactId(Uuid);

impl ArtifactId {
    /// Generate a new opaque identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ArtifactId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ArtifactId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(value)?))
    }
}

/// Immutable fields supplied when staging an artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactMetadataInput {
    pub filename: String,
    pub media_type: Option<String>,
    pub caption: Option<String>,
}

/// Immutable artifact identity and content metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub id: ArtifactId,
    pub filename: String,
    pub media_type: Option<String>,
    pub bytes: u64,
    sha256: [u8; 32],
    pub caption: Option<String>,
}

impl ArtifactMetadata {
    /// SHA-256 digest as lowercase hexadecimal.
    pub fn sha256_hex(&self) -> String {
        hex::encode(self.sha256)
    }

    /// SHA-256 digest bytes.
    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

/// Destination that originated a delivery (for example, a channel and thread).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDeliveryTarget {
    pub surface_id: String,
    pub origin_destination: String,
}

/// Completed delivery receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactDelivery {
    pub target: ArtifactDeliveryTarget,
    pub remote_id: String,
}

/// Exclusive permission to upload one artifact to one origin destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtifactDeliveryClaim {
    id: ArtifactId,
    target: ArtifactDeliveryTarget,
    token: Uuid,
}

/// Result of atomically claiming a delivery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryClaimResult {
    Claimed(ArtifactDeliveryClaim),
    Delivered(ArtifactDelivery),
    InProgress,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum DeliveryState {
    Claimed {
        target: ArtifactDeliveryTarget,
        token: Uuid,
        #[serde(default)]
        claimed_at_unix_ms: u64,
    },
    Delivered(ArtifactDelivery),
}

/// Opaque public failure category. Errors deliberately omit filesystem paths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactError {
    InvalidSource,
    InvalidArtifact,
    NotFound,
    TooLarge,
    VerificationFailed,
    Io,
}

impl std::fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidSource => "invalid artifact source",
            Self::InvalidArtifact => "invalid artifact",
            Self::NotFound => "artifact not found",
            Self::TooLarge => "artifact exceeds size limit",
            Self::VerificationFailed => "artifact verification failed",
            Self::Io => "artifact storage error",
        })
    }
}

impl std::error::Error for ArtifactError {}

type Result<T> = std::result::Result<T, ArtifactError>;

/// Canonical, caller-owned folder permitted as artifact source.
#[derive(Clone, Debug)]
pub struct ExportRoot(PathBuf);

impl ExportRoot {
    /// Open an existing export folder.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|_| ArtifactError::InvalidSource)?;
        if !root.is_dir() {
            return Err(ArtifactError::InvalidSource);
        }
        Ok(Self(root))
    }

    fn source(&self, source: &Path) -> Result<PathBuf> {
        if source.is_absolute()
            || source.components().any(|part| {
                matches!(
                    part,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(ArtifactError::InvalidSource);
        }
        let joined = self.0.join(source);
        let link_meta = fs::symlink_metadata(&joined).map_err(|_| ArtifactError::InvalidSource)?;
        if link_meta.file_type().is_symlink() || !link_meta.file_type().is_file() {
            return Err(ArtifactError::InvalidSource);
        }
        let canonical = joined
            .canonicalize()
            .map_err(|_| ArtifactError::InvalidSource)?;
        if !canonical.starts_with(&self.0) {
            return Err(ArtifactError::InvalidSource);
        }
        Ok(canonical)
    }
}

/// Private artifact vault. Its location is never returned from public methods.
#[derive(Clone, Debug)]
pub struct ArtifactVault(PathBuf);

impl ArtifactVault {
    /// Open or create a vault directory.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        match fs::symlink_metadata(root.as_ref()) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
                return Err(ArtifactError::Io)
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(root.as_ref()).map_err(|_| ArtifactError::Io)?;
            }
            Err(_) => return Err(ArtifactError::Io),
        }
        let root = root
            .as_ref()
            .canonicalize()
            .map_err(|_| ArtifactError::Io)?;
        Ok(Self(root))
    }

    fn dir(&self, id: ArtifactId) -> PathBuf {
        self.0.join(id.to_string())
    }

    fn artifact_dir(&self, id: ArtifactId) -> Result<PathBuf> {
        let dir = self.dir(id);
        let meta = fs::symlink_metadata(&dir).map_err(|_| ArtifactError::NotFound)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(ArtifactError::InvalidArtifact);
        }
        Ok(dir)
    }
}

/// Artifact store backed by a private vault.
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    vault: ArtifactVault,
    max_bytes: u64,
}

impl ArtifactStore {
    /// Open or create an artifact vault. `max_bytes` limits every staged file.
    pub fn open(root: impl AsRef<Path>, max_bytes: u64) -> Result<Self> {
        Ok(Self {
            vault: ArtifactVault::open(root)?,
            max_bytes,
        })
    }

    /// Copy a regular, non-symlink file from `export_root` into immutable vault storage.
    pub fn stage_from_export_root(
        &self,
        export_root: &ExportRoot,
        source: impl AsRef<Path>,
        input: ArtifactMetadataInput,
    ) -> Result<ArtifactMetadata> {
        let source = export_root.source(source.as_ref())?;
        let mut source_file = open_source(&source)?;
        let source_meta = source_file
            .metadata()
            .map_err(|_| ArtifactError::InvalidSource)?;
        if !source_meta.is_file() || source_meta.len() > self.max_bytes {
            return Err(if source_meta.is_file() {
                ArtifactError::TooLarge
            } else {
                ArtifactError::InvalidSource
            });
        }

        let id = ArtifactId::new();
        let dir = self.vault.dir(id);
        fs::create_dir(&dir).map_err(|_| ArtifactError::Io)?;
        let result = (|| {
            let data_path = dir.join(DATA_FILE);
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(data_path)
                .map_err(|_| ArtifactError::Io)?;
            let (bytes, sha256) = copy_hash_limited(&mut source_file, &mut out, self.max_bytes)?;
            out.sync_all().map_err(|_| ArtifactError::Io)?;
            let metadata = ArtifactMetadata {
                id,
                filename: input.filename,
                media_type: input.media_type,
                bytes,
                sha256,
                caption: input.caption,
            };
            write_json(&dir.join(METADATA_FILE), &metadata)?;
            write_json(&dir.join(DELIVERIES_FILE), &Vec::<DeliveryState>::new())?;
            Ok(metadata)
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&dir);
        }
        result
    }

    /// Read stored immutable metadata without exposing vault paths.
    pub fn metadata(&self, id: ArtifactId) -> Result<ArtifactMetadata> {
        read_json(&self.vault.artifact_dir(id)?.join(METADATA_FILE)).map_err(not_found)
    }

    /// Verify stored size and digest, then return a reader over artifact bytes.
    pub fn open_verified(&self, id: ArtifactId) -> Result<VerifiedArtifact> {
        let metadata = self.metadata(id)?;
        let path = self.vault.artifact_dir(id)?.join(DATA_FILE);
        let mut file = open_vault_file(&path)?;
        let file_meta = file
            .metadata()
            .map_err(|_| ArtifactError::InvalidArtifact)?;
        if !file_meta.is_file() || file_meta.len() != metadata.bytes {
            return Err(ArtifactError::VerificationFailed);
        }
        let (bytes, digest) = hash_reader(&mut file)?;
        if bytes != metadata.bytes || digest != *metadata.sha256() {
            return Err(ArtifactError::VerificationFailed);
        }
        file.seek(SeekFrom::Start(0))
            .map_err(|_| ArtifactError::InvalidArtifact)?;
        Ok(VerifiedArtifact { metadata, file })
    }

    /// Atomically claim an upload. Claims expire after five minutes so crashes
    /// recover; only a current claim token can record an accepted delivery.
    pub fn claim_delivery(
        &self,
        id: ArtifactId,
        target: ArtifactDeliveryTarget,
    ) -> Result<DeliveryClaimResult> {
        let dir = self.vault.artifact_dir(id)?;
        let _lock = DeliveryLock::acquire(&dir)?;
        let mut states = delivery_states(&dir)?;
        let now = unix_time_ms()?;
        if expire_claims(&mut states, now) {
            write_json(&dir.join(DELIVERIES_FILE), &states)?;
        }
        if let Some(state) = states.iter().find(|state| match state {
            DeliveryState::Claimed {
                target: existing, ..
            } => existing == &target,
            DeliveryState::Delivered(delivery) => delivery.target == target,
        }) {
            return Ok(match state {
                DeliveryState::Claimed { .. } => DeliveryClaimResult::InProgress,
                DeliveryState::Delivered(delivery) => {
                    DeliveryClaimResult::Delivered(delivery.clone())
                }
            });
        }
        let claim = ArtifactDeliveryClaim {
            id,
            target,
            token: Uuid::new_v4(),
        };
        states.push(DeliveryState::Claimed {
            target: claim.target.clone(),
            token: claim.token,
            claimed_at_unix_ms: now,
        });
        write_json(&dir.join(DELIVERIES_FILE), &states)?;
        Ok(DeliveryClaimResult::Claimed(claim))
    }

    /// Commit successful upload under its claim. A stale or foreign claim fails.
    pub fn complete_delivery(&self, claim: ArtifactDeliveryClaim, remote_id: String) -> Result<()> {
        let dir = self.vault.artifact_dir(claim.id)?;
        let _lock = DeliveryLock::acquire(&dir)?;
        let mut states = delivery_states(&dir)?;
        if expire_claims(&mut states, unix_time_ms()?) {
            write_json(&dir.join(DELIVERIES_FILE), &states)?;
        }
        let Some(index) = states.iter().position(|state| matches!(state, DeliveryState::Claimed { target, token, .. } if target == &claim.target && *token == claim.token)) else {
            return Err(ArtifactError::InvalidArtifact);
        };
        states[index] = DeliveryState::Delivered(ArtifactDelivery {
            target: claim.target,
            remote_id,
        });
        write_json(&dir.join(DELIVERIES_FILE), &states)
    }

    /// Release failed upload claim so a retry can claim it. A stale claim fails.
    pub fn release_delivery(&self, claim: ArtifactDeliveryClaim) -> Result<()> {
        let dir = self.vault.artifact_dir(claim.id)?;
        let _lock = DeliveryLock::acquire(&dir)?;
        let mut states = delivery_states(&dir)?;
        if expire_claims(&mut states, unix_time_ms()?) {
            write_json(&dir.join(DELIVERIES_FILE), &states)?;
        }
        let Some(index) = states.iter().position(|state| matches!(state, DeliveryState::Claimed { target, token, .. } if target == &claim.target && *token == claim.token)) else {
            return Err(ArtifactError::InvalidArtifact);
        };
        states.remove(index);
        write_json(&dir.join(DELIVERIES_FILE), &states)
    }

    /// Return completed delivery receipts without exposing vault paths.
    pub fn deliveries(&self, id: ArtifactId) -> Result<Vec<ArtifactDelivery>> {
        let dir = self.vault.artifact_dir(id)?;
        Ok(delivery_states(&dir)?
            .into_iter()
            .filter_map(|state| match state {
                DeliveryState::Delivered(delivery) => Some(delivery),
                DeliveryState::Claimed { .. } => None,
            })
            .collect())
    }

    /// Verify a resource link against canonical vault metadata before delivery.
    pub fn validate_resource_link(
        &self,
        id: ArtifactId,
        filename: &str,
        media_type: Option<&str>,
        bytes: u64,
        sha256: &str,
        caption: Option<&str>,
    ) -> Result<ArtifactMetadata> {
        let metadata = self.metadata(id)?;
        if metadata.filename != filename
            || metadata.media_type.as_deref() != media_type
            || metadata.bytes != bytes
            || metadata.sha256_hex() != sha256
            || metadata.caption.as_deref() != caption
        {
            return Err(ArtifactError::InvalidArtifact);
        }
        Ok(metadata)
    }
}

/// Reader yielded only after integrity verification. It exposes no storage path.
#[derive(Debug)]
pub struct VerifiedArtifact {
    metadata: ArtifactMetadata,
    file: File,
}

impl VerifiedArtifact {
    pub fn metadata(&self) -> &ArtifactMetadata {
        &self.metadata
    }
}

impl Read for VerifiedArtifact {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

struct DeliveryLock(File);

impl DeliveryLock {
    fn acquire(dir: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(DELIVERY_LOCK_FILE))
            .map_err(|_| ArtifactError::Io)?;
        file.lock_exclusive().map_err(|_| ArtifactError::Io)?;
        Ok(Self(file))
    }
}

impl Drop for DeliveryLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

fn delivery_states(dir: &Path) -> Result<Vec<DeliveryState>> {
    read_json(&dir.join(DELIVERIES_FILE)).map_err(not_found)
}

fn open_source(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path).map_err(|_| ArtifactError::InvalidSource)
}

fn open_vault_file(path: &Path) -> Result<File> {
    let meta = fs::symlink_metadata(path).map_err(|_| ArtifactError::NotFound)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(ArtifactError::InvalidArtifact);
    }
    OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| ArtifactError::InvalidArtifact)
}

fn copy_hash_limited(
    reader: &mut File,
    writer: &mut File,
    max_bytes: u64,
) -> Result<(u64, [u8; 32])> {
    let mut hash = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|_| ArtifactError::InvalidSource)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .ok_or(ArtifactError::TooLarge)?;
        if bytes > max_bytes {
            return Err(ArtifactError::TooLarge);
        }
        writer
            .write_all(&buffer[..count])
            .map_err(|_| ArtifactError::Io)?;
        hash.update(&buffer[..count]);
    }
    Ok((bytes, hash.finalize().into()))
}

fn hash_reader(reader: &mut File) -> Result<(u64, [u8; 32])> {
    let mut hash = Sha256::new();
    let bytes =
        io::copy(reader, &mut HashWriter(&mut hash)).map_err(|_| ArtifactError::InvalidArtifact)?;
    Ok((bytes, hash.finalize().into()))
}

struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let encoded = serde_json::to_vec(value).map_err(|_| ArtifactError::Io)?;
    let parent = path.parent().ok_or(ArtifactError::Io)?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|_| ArtifactError::Io)?;
        file.write_all(&encoded).map_err(|_| ArtifactError::Io)?;
        file.sync_all().map_err(|_| ArtifactError::Io)?;
        fs::rename(&temp, path).map_err(|_| ArtifactError::Io)?;
        File::open(parent)
            .and_then(|dir| dir.sync_all())
            .map_err(|_| ArtifactError::Io)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn unix_time_ms() -> Result<u64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .map_err(|_| ArtifactError::Io)
}

fn claim_expired(claimed_at_unix_ms: u64, now_unix_ms: u64) -> bool {
    now_unix_ms.saturating_sub(claimed_at_unix_ms) >= DELIVERY_CLAIM_LEASE_MS
}

fn expire_claims(states: &mut Vec<DeliveryState>, now_unix_ms: u64) -> bool {
    let original_len = states.len();
    states.retain(|state| {
        !matches!(state, DeliveryState::Claimed { claimed_at_unix_ms, .. } if claim_expired(*claimed_at_unix_ms, now_unix_ms))
    });
    states.len() != original_len
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T> {
    let meta = fs::symlink_metadata(path).map_err(|_| ArtifactError::NotFound)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(ArtifactError::InvalidArtifact);
    }
    let file = File::open(path).map_err(|_| ArtifactError::NotFound)?;
    serde_json::from_reader(file).map_err(|_| ArtifactError::InvalidArtifact)
}

fn not_found(error: ArtifactError) -> ArtifactError {
    match error {
        ArtifactError::NotFound => ArtifactError::NotFound,
        _ => ArtifactError::InvalidArtifact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ArtifactMetadataInput {
        ArtifactMetadataInput {
            filename: "report.txt".into(),
            media_type: Some("text/plain".into()),
            caption: Some("report".into()),
        }
    }

    fn roots() -> (tempfile::TempDir, ExportRoot, ArtifactStore) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("exports")).unwrap();
        let export = ExportRoot::open(dir.path().join("exports")).unwrap();
        let store = ArtifactStore::open(dir.path().join("vault"), 16).unwrap();
        (dir, export, store)
    }

    #[test]
    fn stages_snapshot_verifies_and_hides_paths() {
        let (dir, export, store) = roots();
        fs::write(dir.path().join("exports/report.txt"), b"hello").unwrap();
        let metadata = store
            .stage_from_export_root(&export, "report.txt", input())
            .unwrap();
        fs::write(dir.path().join("exports/report.txt"), b"changed").unwrap();
        assert_eq!(metadata.bytes, 5);
        assert_eq!(
            metadata.sha256_hex(),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        let mut opened = store.open_verified(metadata.id).unwrap();
        let mut body = String::new();
        opened.read_to_string(&mut body).unwrap();
        assert_eq!(body, "hello");
        assert_eq!(opened.metadata(), &metadata);
    }

    #[test]
    fn delivery_claim_is_atomic_and_retryable_per_origin() {
        let (dir, export, store) = roots();
        fs::write(dir.path().join("exports/report.txt"), b"hello").unwrap();
        let artifact = store
            .stage_from_export_root(&export, "report.txt", input())
            .unwrap();
        let target = ArtifactDeliveryTarget {
            surface_id: "slack".into(),
            origin_destination: "channel:one/thread:two".into(),
        };
        let claim = match store.claim_delivery(artifact.id, target.clone()).unwrap() {
            DeliveryClaimResult::Claimed(claim) => claim,
            other => panic!("unexpected claim state: {other:?}"),
        };
        assert_eq!(
            store.claim_delivery(artifact.id, target.clone()).unwrap(),
            DeliveryClaimResult::InProgress
        );
        store.release_delivery(claim).unwrap();
        let claim = match store.claim_delivery(artifact.id, target.clone()).unwrap() {
            DeliveryClaimResult::Claimed(claim) => claim,
            other => panic!("unexpected retry state: {other:?}"),
        };
        store.complete_delivery(claim, "opaque-42".into()).unwrap();
        assert_eq!(
            store.deliveries(artifact.id).unwrap(),
            vec![ArtifactDelivery {
                target: target.clone(),
                remote_id: "opaque-42".into()
            }]
        );
        assert!(matches!(
            store.claim_delivery(artifact.id, target).unwrap(),
            DeliveryClaimResult::Delivered(_)
        ));
        let other = ArtifactDeliveryTarget {
            surface_id: "slack".into(),
            origin_destination: "channel:other".into(),
        };
        assert!(matches!(
            store.claim_delivery(artifact.id, other).unwrap(),
            DeliveryClaimResult::Claimed(_)
        ));
    }

    #[test]
    fn expired_claim_recovers_but_stale_completion_is_rejected() {
        let (dir, export, store) = roots();
        fs::write(dir.path().join("exports/report.txt"), b"hello").unwrap();
        let artifact = store
            .stage_from_export_root(&export, "report.txt", input())
            .unwrap();
        let target = ArtifactDeliveryTarget {
            surface_id: "slack".into(),
            origin_destination: "channel:one".into(),
        };
        let stale = match store.claim_delivery(artifact.id, target.clone()).unwrap() {
            DeliveryClaimResult::Claimed(claim) => claim,
            other => panic!("unexpected claim state: {other:?}"),
        };
        write_json(
            &dir.path()
                .join("vault")
                .join(artifact.id.to_string())
                .join(DELIVERIES_FILE),
            &vec![DeliveryState::Claimed {
                target: target.clone(),
                token: stale.token,
                claimed_at_unix_ms: 0,
            }],
        )
        .unwrap();
        let current = match store.claim_delivery(artifact.id, target).unwrap() {
            DeliveryClaimResult::Claimed(claim) => claim,
            other => panic!("unexpected recovered claim state: {other:?}"),
        };
        assert_eq!(
            store.complete_delivery(stale, "old-upload".into()),
            Err(ArtifactError::InvalidArtifact)
        );
        store
            .complete_delivery(current, "current-upload".into())
            .unwrap();
    }

    #[test]
    fn concurrent_claims_allow_one_uploader() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let (dir, export, store) = roots();
        fs::write(dir.path().join("exports/report.txt"), b"hello").unwrap();
        let artifact = store
            .stage_from_export_root(&export, "report.txt", input())
            .unwrap();
        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(2));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            workers.push(thread::spawn(move || {
                barrier.wait();
                matches!(
                    store
                        .claim_delivery(
                            artifact.id,
                            ArtifactDeliveryTarget {
                                surface_id: "slack".into(),
                                origin_destination: "channel:one".into()
                            },
                        )
                        .unwrap(),
                    DeliveryClaimResult::Claimed(_)
                )
            }));
        }
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|claimed| *claimed)
                .count(),
            1
        );
    }

    #[test]
    fn rejects_resource_metadata_not_matching_vault() {
        let (dir, export, store) = roots();
        fs::write(dir.path().join("exports/report.txt"), b"hello").unwrap();
        let artifact = store
            .stage_from_export_root(&export, "report.txt", input())
            .unwrap();
        assert!(store
            .validate_resource_link(
                artifact.id,
                "forged.txt",
                Some("text/plain"),
                5,
                &artifact.sha256_hex(),
                Some("report")
            )
            .is_err());
        assert_eq!(
            store
                .validate_resource_link(
                    artifact.id,
                    "report.txt",
                    Some("text/plain"),
                    5,
                    &artifact.sha256_hex(),
                    Some("report")
                )
                .unwrap(),
            artifact
        );
    }

    #[test]
    fn rejects_traversal_nonregular_and_oversize() {
        let (dir, export, store) = roots();
        fs::write(dir.path().join("exports/large"), b"0123456789abcdefg").unwrap();
        assert_eq!(
            store
                .stage_from_export_root(&export, "../outside", input())
                .unwrap_err(),
            ArtifactError::InvalidSource
        );
        assert_eq!(
            store
                .stage_from_export_root(&export, "large", input())
                .unwrap_err(),
            ArtifactError::TooLarge
        );
        fs::create_dir(dir.path().join("exports/folder")).unwrap();
        assert_eq!(
            store
                .stage_from_export_root(&export, "folder", input())
                .unwrap_err(),
            ArtifactError::InvalidSource
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_source_and_tampered_vault_file() {
        use std::os::unix::fs::symlink;
        let (dir, export, store) = roots();
        fs::write(dir.path().join("outside"), b"secret").unwrap();
        symlink(dir.path().join("outside"), dir.path().join("exports/link")).unwrap();
        assert_eq!(
            store
                .stage_from_export_root(&export, "link", input())
                .unwrap_err(),
            ArtifactError::InvalidSource
        );
        fs::write(dir.path().join("exports/report.txt"), b"hello").unwrap();
        let artifact = store
            .stage_from_export_root(&export, "report.txt", input())
            .unwrap();
        fs::write(
            dir.path()
                .join("vault")
                .join(artifact.id.to_string())
                .join(DATA_FILE),
            b"other",
        )
        .unwrap();
        assert_eq!(
            store.open_verified(artifact.id).unwrap_err(),
            ArtifactError::VerificationFailed
        );
    }
}
