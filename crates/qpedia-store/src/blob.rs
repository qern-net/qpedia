//! Raw document storage on the /data/raw volume. See DESIGN.md §2.1.
//!
//! Layout:
//!   <root>/<source_id>/original.<ext>
//!   <root>/<source_id>/extracted.txt
//!   <root>/<source_id>/manifest.json

use async_trait::async_trait;
use bytes::Bytes;
use qpedia_core::{Error, Result, SourceId};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

#[async_trait]
pub trait BlobStorage: Send + Sync {
    async fn put(&self, id: &SourceId, kind: BlobKind, ext: &str, bytes: Bytes) -> Result<PathBuf>;
    async fn get(&self, id: &SourceId, kind: BlobKind, ext: &str) -> Result<Bytes>;
    async fn put_text(&self, id: &SourceId, kind: BlobKind, text: &str) -> Result<PathBuf>;
    fn path_for(&self, id: &SourceId, kind: BlobKind, ext: &str) -> PathBuf;
}

#[derive(Debug, Clone, Copy)]
pub enum BlobKind {
    Original,
    Extracted,
    Manifest,
}

impl BlobKind {
    fn stem(self) -> &'static str {
        match self {
            BlobKind::Original  => "original",
            BlobKind::Extracted => "extracted",
            BlobKind::Manifest  => "manifest",
        }
    }
}

#[derive(Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    fn dir(&self, id: &SourceId) -> PathBuf {
        self.root.join(id.as_str())
    }
}

#[async_trait]
impl BlobStorage for BlobStore {
    fn path_for(&self, id: &SourceId, kind: BlobKind, ext: &str) -> PathBuf {
        let ext = if ext.is_empty() { String::new() } else { format!(".{}", ext.trim_start_matches('.')) };
        self.dir(id).join(format!("{}{ext}", kind.stem()))
    }

    async fn put(&self, id: &SourceId, kind: BlobKind, ext: &str, bytes: Bytes) -> Result<PathBuf> {
        let dir = self.dir(id);
        tokio::fs::create_dir_all(&dir).await?;
        let path = self.path_for(id, kind, ext);
        tokio::fs::write(&path, &bytes).await?;
        Ok(path)
    }

    async fn put_text(&self, id: &SourceId, kind: BlobKind, text: &str) -> Result<PathBuf> {
        let ext = match kind {
            BlobKind::Manifest => "json",
            _ => "txt",
        };
        self.put(id, kind, ext, Bytes::copy_from_slice(text.as_bytes())).await
    }

    async fn get(&self, id: &SourceId, kind: BlobKind, ext: &str) -> Result<Bytes> {
        let path = self.path_for(id, kind, ext);
        let mut f = tokio::fs::File::open(&path).await
            .map_err(|_| Error::NotFound(path.display().to_string()))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).await?;
        Ok(Bytes::from(buf))
    }
}
