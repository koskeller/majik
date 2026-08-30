//! Blob storage abstraction. The library keeps its own content (generated media, thumbnails, input
//! assets, trash) behind [`BlobStore`], so the local filesystem is only one backend and an
//! S3-compatible one can be added later without touching the library, thumbnailing, or UI code.
//!
//! Blobs are addressed by a **relative key** (a forward-slash path such as `"a1b2.png"` or
//! `".majik/thumbs/deadbeef.jpg"`). For the local backend a key maps directly to a file under the
//! library root, so [`BlobStore::local_path`] returns the real file. For a remote backend the same
//! call materializes the object into a local cache first, because the UI (GPUI `img`, the video and
//! audio players) can only read local paths.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

/// A content store addressed by relative keys. All methods are synchronous; a remote implementation
/// may block on its own runtime internally.
pub trait BlobStore: Send + Sync {
    /// Human-readable name of the backend (for logs / settings).
    fn kind(&self) -> &'static str;

    /// Write bytes under `key`, overwriting any existing blob.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;

    /// Read the bytes for `key`.
    fn read(&self, key: &str) -> Result<Vec<u8>>;

    /// Remove `key`. Missing keys are not an error.
    fn delete(&self, key: &str) -> Result<()>;

    fn exists(&self, key: &str) -> bool;

    /// Size in bytes, if known without downloading.
    fn len(&self, key: &str) -> Option<u64>;

    /// A local filesystem path for `key`, materializing the blob into a cache if the backend is
    /// remote. The path is valid for reading until the cache is evicted.
    fn local_path(&self, key: &str) -> Result<PathBuf>;

    /// Keys of the blobs directly under the directory `prefix` (no recursion), in no particular
    /// order. A prefix that holds nothing yields an empty list.
    fn list(&self, prefix: &str) -> Result<Vec<String>>;

    /// Move an existing local file into the store under `key` (used to trash/adopt a file without a
    /// full read+write round-trip when the backend is local). Default: read + put + remove.
    fn adopt(&self, key: &str, from: &Path) -> Result<()> {
        let bytes = std::fs::read(from)?;
        self.put(key, &bytes)?;
        let _ = std::fs::remove_file(from);
        Ok(())
    }
}

/// Rejects keys that could escape the store root (absolute paths, `..`, drive letters).
fn safe_key(key: &str) -> Result<&str> {
    if key.is_empty() || key.starts_with('/') || key.split('/').any(|c| c == ".." || c == ".") || key.contains('\\') || key.contains(':') {
        return Err(anyhow!("unsafe blob key: {key:?}"));
    }
    Ok(key)
}

/// Filesystem-backed store: `key` is a path relative to `root`.
pub struct LocalBlobStore {
    root: PathBuf,
}

impl LocalBlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &str) -> Result<PathBuf> {
        Ok(self.root.join(safe_key(key)?))
    }
}

impl BlobStore for LocalBlobStore {
    fn kind(&self) -> &'static str {
        "local"
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path_for(key)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Atomic-ish: write to a temp sibling then rename.
        let tmp = path.with_extension("tmp-write");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    fn read(&self, key: &str) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.path_for(key)?)?)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let path = self.path_for(key)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn exists(&self, key: &str) -> bool {
        self.path_for(key).map(|p| p.exists()).unwrap_or(false)
    }

    fn len(&self, key: &str) -> Option<u64> {
        self.path_for(key).ok().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len())
    }

    fn local_path(&self, key: &str) -> Result<PathBuf> {
        self.path_for(key)
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let dir = self.path_for(prefix)?;
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut keys = Vec::new();
        for entry in entries {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                keys.push(format!("{}/{}", prefix.trim_end_matches('/'), entry.file_name().to_string_lossy()));
            }
        }
        Ok(keys)
    }

    fn adopt(&self, key: &str, from: &Path) -> Result<()> {
        let dest = self.path_for(key)?;
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Prefer a rename; fall back to copy+remove across filesystems.
        std::fs::rename(from, &dest).or_else(|_| std::fs::copy(from, &dest).and_then(|_| std::fs::remove_file(from)).map(|_| ()))?;
        Ok(())
    }
}

// A future `S3BlobStore` implements the same trait: `put`/`read`/`delete` map to S3 object
// operations (via `aws-sdk-s3` or `opendal` for S3-compatible endpoints), and `local_path`
// downloads the object into a local cache directory (keyed by the blob key) on first access, so the
// UI keeps reading real files. Only the `Library`'s store construction changes; nothing else does.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_round_trip_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path());
        store.put("a.png", b"hello").unwrap();
        store.put(".majik/thumbs/x.jpg", b"thumb").unwrap();
        assert!(store.exists("a.png"));
        assert_eq!(store.read("a.png").unwrap(), b"hello");
        assert_eq!(store.len("a.png"), Some(5));
        assert_eq!(store.local_path("a.png").unwrap(), dir.path().join("a.png"));
        assert_eq!(store.local_path(".majik/thumbs/x.jpg").unwrap(), dir.path().join(".majik/thumbs/x.jpg"));
        store.delete("a.png").unwrap();
        assert!(!store.exists("a.png"));
        store.delete("a.png").unwrap(); // idempotent
    }

    #[test]
    fn list_returns_direct_children_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path());
        assert!(store.list(".majik/thumbs").unwrap().is_empty(), "absent prefix is empty, not an error");
        store.put(".majik/thumbs/a.jpg", b"1").unwrap();
        store.put(".majik/thumbs/b.png", b"2").unwrap();
        store.put(".majik/thumbs/nested/c.png", b"3").unwrap();
        let mut keys = store.list(".majik/thumbs/").unwrap();
        keys.sort();
        assert_eq!(keys, vec![".majik/thumbs/a.jpg", ".majik/thumbs/b.png"]);
    }

    #[test]
    fn rejects_unsafe_keys() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path());
        for bad in ["/etc/passwd", "../escape", "a/../b", "", "c:\\x", "a\\b"] {
            assert!(store.put(bad, b"x").is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn adopt_moves_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalBlobStore::new(dir.path());
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"data").unwrap();
        store.adopt(".majik/trash/src.bin", &src).unwrap();
        assert!(!src.exists());
        assert_eq!(store.read(".majik/trash/src.bin").unwrap(), b"data");
    }
}
