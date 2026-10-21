//! Secret-provider abstraction.
//!
//! The Manager keeps long-lived and recoverable secrets (GitHub App private
//! key, OAuth client secret, webhook secret, Coder operator token, and
//! installation-token references) behind a [`SecretProvider`] trait rather than
//! in plaintext database columns or application code. The concrete backend is a
//! deployment decision (D2 in compatibility-report.md); the manager provides the
//! trait and an isolated in-memory test implementation, and the selected
//! production adapter is supplied later via `OPENFLOWS_SECRET_PROVIDER`.
//!
//! Secrets are referenced by an opaque name. The trait is async so adapters
//! backed by network key-management services can be used directly.

use async_trait::async_trait;

/// A reference to a stored secret. Adapters persist the secret value under this
/// name; callers store only the reference in their own records.
pub type SecretRef = String;

#[cfg(test)]
mod reference_tests {
    use super::*;

    #[tokio::test]
    async fn filesystem_retry_reference_stays_absolute() {
        let dir = tempfile::tempdir().unwrap();
        let provider = FilesystemSecretProvider::new(dir.path()).unwrap();
        let name = "workspace-runtime-test";
        let original = provider.put(name, b"first").await.unwrap();
        assert!(matches!(
            provider.put(name, b"second").await,
            Err(SecretError::AlreadyExists(_))
        ));
        provider.replace(name, b"second").await.unwrap();
        let recovered = provider.canonical_reference(name).unwrap();
        assert_eq!(recovered, original);
        assert!(std::path::Path::new(&recovered).is_absolute());
        assert_eq!(provider.canonical_reference(&recovered).unwrap(), recovered);
        assert_eq!(provider.get(&recovered).await.unwrap(), b"second");
        assert!(provider.canonical_reference("../outside").is_err());
    }
}

/// The interface for storing and retrieving secrets.
#[async_trait]
pub trait SecretProvider: Send + Sync {
    /// Resolve a logical name to the provider's persistent reference without
    /// reading or modifying the secret value.
    fn canonical_reference(&self, reference: &str) -> Result<SecretRef, SecretError> {
        Ok(reference.to_owned())
    }

    /// Store `value` under `name`, returning a stable reference the caller can
    /// persist. If the name already exists, implementations should reject or
    /// require an explicit rotation rather than silently overwrite.
    async fn put(&self, name: &str, value: &[u8]) -> Result<SecretRef, SecretError>;

    /// Replace an existing secret while preserving its stable reference. This
    /// is used for mounted per-workspace bundles: rotating credentials must be
    /// visible to the running workload without rebuilding it or changing the
    /// provisioner parameter.
    async fn replace(&self, ref_name: &str, value: &[u8]) -> Result<(), SecretError>;

    /// Retrieve the secret for `ref_name`.
    async fn get(&self, ref_name: &str) -> Result<Vec<u8>, SecretError>;

    /// Delete a secret. Used during cleanup; not all backends support it.
    async fn delete(&self, ref_name: &str) -> Result<(), SecretError>;
}

/// Errors surfaced by a [`SecretProvider`].
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("secret {0} not found")]
    NotFound(String),
    #[error("secret already exists: {0}")]
    AlreadyExists(String),
    #[error("secret provider unavailable: {0}")]
    Unavailable(String),
    #[error("secret provider rejected operation: {0}")]
    Rejected(String),
}

/// The adapter name for the isolated in-memory provider used in tests and local
/// development. A production deployment selects a different adapter.
pub const IN_MEMORY_PROVIDER: &str = "in-memory";
pub const FILESYSTEM_PROVIDER: &str = "filesystem";

/// An isolated in-memory [`SecretProvider`] for tests and local development.
///
/// This is intentionally not a production backend: it stores secrets in process
/// memory, so it is only suitable for tests and ephemeral local runs. It
/// enforces the same "put does not silently overwrite" contract as production
/// adapters.
#[derive(Debug, Default)]
pub struct InMemorySecretProvider {
    secrets: std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>,
}

impl InMemorySecretProvider {
    pub fn new() -> Self {
        InMemorySecretProvider::default()
    }
}

#[async_trait]
impl SecretProvider for InMemorySecretProvider {
    async fn put(&self, name: &str, value: &[u8]) -> Result<SecretRef, SecretError> {
        let mut map = self.secrets.lock().unwrap();
        if map.contains_key(name) {
            return Err(SecretError::AlreadyExists(name.to_string()));
        }
        map.insert(name.to_string(), value.to_vec());
        Ok(name.to_string())
    }

    async fn get(&self, ref_name: &str) -> Result<Vec<u8>, SecretError> {
        let map = self.secrets.lock().unwrap();
        map.get(ref_name)
            .cloned()
            .ok_or_else(|| SecretError::NotFound(ref_name.to_string()))
    }

    async fn replace(&self, ref_name: &str, value: &[u8]) -> Result<(), SecretError> {
        let mut map = self.secrets.lock().unwrap();
        let secret = map
            .get_mut(ref_name)
            .ok_or_else(|| SecretError::NotFound(ref_name.to_string()))?;
        use zeroize::Zeroize;
        secret.zeroize();
        *secret = value.to_vec();
        Ok(())
    }

    async fn delete(&self, ref_name: &str) -> Result<(), SecretError> {
        let mut map = self.secrets.lock().unwrap();
        map.remove(ref_name)
            .map(|_| ())
            .ok_or_else(|| SecretError::NotFound(ref_name.to_string()))
    }
}

/// Durable local/Docker secret backend. Each secret is a mode-0600 file under
/// one operator-selected directory; the returned reference is directly
/// mountable by the Docker provisioner. Kubernetes deployments should provide
/// their own Secret-backed adapter rather than sharing this host directory.
#[derive(Debug)]
pub struct FilesystemSecretProvider {
    root: std::path::PathBuf,
}

impl FilesystemSecretProvider {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Result<Self, SecretError> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| {
            SecretError::Unavailable(format!("cannot create secret directory: {e}"))
        })?;
        let root = root.canonicalize().map_err(|e| {
            SecretError::Unavailable(format!("cannot resolve secret directory: {e}"))
        })?;
        Ok(FilesystemSecretProvider { root })
    }

    fn path(&self, reference: &str) -> Result<std::path::PathBuf, SecretError> {
        let candidate = std::path::Path::new(reference);
        let path = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            if reference.is_empty()
                || reference.contains('/')
                || reference.contains('\\')
                || reference == "."
                || reference == ".."
            {
                return Err(SecretError::Rejected("invalid secret reference".into()));
            }
            self.root.join(reference)
        };
        if !path.starts_with(&self.root) {
            return Err(SecretError::Rejected(
                "secret reference escapes root".into(),
            ));
        }
        Ok(path)
    }
}

#[async_trait]
impl SecretProvider for FilesystemSecretProvider {
    fn canonical_reference(&self, reference: &str) -> Result<SecretRef, SecretError> {
        Ok(self.path(reference)?.to_string_lossy().into_owned())
    }

    async fn put(&self, name: &str, value: &[u8]) -> Result<SecretRef, SecretError> {
        use std::io::Write;
        let path = self.path(name)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => SecretError::AlreadyExists(name.into()),
            _ => SecretError::Unavailable(format!("cannot create secret: {e}")),
        })?;
        file.write_all(value)
            .and_then(|_| file.sync_all())
            .map_err(|e| SecretError::Unavailable(format!("cannot write secret: {e}")))?;
        Ok(path.to_string_lossy().into_owned())
    }

    async fn get(&self, ref_name: &str) -> Result<Vec<u8>, SecretError> {
        let path = self.path(ref_name)?;
        std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SecretError::NotFound(ref_name.into()),
            _ => SecretError::Unavailable(format!("cannot read secret: {e}")),
        })
    }

    async fn replace(&self, ref_name: &str, value: &[u8]) -> Result<(), SecretError> {
        use std::io::Write;
        let path = self.path(ref_name)?;
        // Preserve the inode so an existing Docker bind mount observes the
        // new bytes. The directory itself is not mounted into the workload.
        let mut options = std::fs::OpenOptions::new();
        options.write(true).truncate(true);
        let mut file = options.open(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SecretError::NotFound(ref_name.into()),
            _ => SecretError::Unavailable(format!("cannot replace secret: {e}")),
        })?;
        file.write_all(value)
            .and_then(|_| file.sync_all())
            .map_err(|e| SecretError::Unavailable(format!("cannot replace secret: {e}")))
    }

    async fn delete(&self, ref_name: &str) -> Result<(), SecretError> {
        let path = self.path(ref_name)?;
        std::fs::remove_file(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => SecretError::NotFound(ref_name.into()),
            _ => SecretError::Unavailable(format!("cannot delete secret: {e}")),
        })
    }
}
