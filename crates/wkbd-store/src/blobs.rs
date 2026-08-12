//! Content-addressed storage for payloads too large to belong in a row.
//!
//! Whole-file diffs, terminal transcripts and raw model responses go here; the database
//! keeps a hash. Besides keeping the database small, content addressing means the common
//! case of an agent re-emitting the same file contents costs nothing.

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating blob directory {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn hash(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        hex::encode(h.finalize())
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        // Two-level fan-out: a flat directory with a hundred thousand entries makes
        // directory listing and some filesystems' lookup noticeably slow.
        self.root.join(&hash[0..2]).join(&hash[2..4]).join(hash)
    }

    /// Writes the blob if it is not already present and returns its hash.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let hash = Self::hash(bytes);
        let path = self.path_for(&hash);
        if path.exists() {
            return Ok(hash);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write to a temporary name and rename, so a crash mid-write cannot leave a
        // truncated file sitting under a hash that claims to describe its contents.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.path_for(hash);
        std::fs::read(&path).with_context(|| format!("reading blob {hash}"))
    }

    pub fn exists(&self, hash: &str) -> bool {
        self.path_for(hash).exists()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}
