//! Raw document storage on the /data/raw volume. See DESIGN.md §2.1.
//!
//! Layout:
//!   <root>/<tenant>/<source_id>/original.<ext>
//!   <root>/<tenant>/<source_id>/extracted.txt
//!   <root>/<tenant>/<source_id>/manifest.json

use async_trait::async_trait;
use bytes::Bytes;
use qpedia_core::{Error, Result, SourceId, Tenant};
use std::path::{Path, PathBuf};
use tokio::io::AsyncReadExt;

#[async_trait]
pub trait BlobStorage: Send + Sync {
    async fn put(&self, tenant: &Tenant, id: &SourceId, kind: BlobKind, ext: &str, bytes: Bytes) -> Result<PathBuf>;
    async fn get(&self, tenant: &Tenant, id: &SourceId, kind: BlobKind, ext: &str) -> Result<Bytes>;
    async fn put_text(&self, tenant: &Tenant, id: &SourceId, kind: BlobKind, text: &str) -> Result<PathBuf>;
    fn path_for(&self, tenant: &Tenant, id: &SourceId, kind: BlobKind, ext: &str) -> Result<PathBuf>;
    /// Recursively delete every blob for a source. Idempotent.
    async fn delete_all(&self, tenant: &Tenant, id: &SourceId) -> Result<()>;
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
        // An unattended upgrade must not create a new tenant tree beside legacy
        // originals and then appear to serve an empty store.
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            for item in std::fs::read_dir(entry.path())? {
                let item = item?;
                if item.file_type()?.is_dir() {
                    continue;
                }
                let filename = item.file_name();
                let filename = filename.to_string_lossy();
                if filename == "original" || filename.starts_with("original.")
                    || matches!(filename.as_ref(), "extracted.txt" | "manifest.json")
                {
                    return Err(Error::Invalid(
                        "legacy flat blob storage detected; migrate it to tenant directories before startup".into(),
                    ));
                }
            }
        }
        Ok(Self { root })
    }

    fn dir(&self, tenant: &Tenant, id: &SourceId) -> Result<PathBuf> {
        for segment in [tenant.as_str(), id.as_str()] {
            if segment.is_empty() || segment == "." || segment == ".." || segment.contains(['/', '\\', ':', '\0']) {
                return Err(Error::Invalid("unsafe blob storage segment".into()));
            }
        }
        let dir = self.root.join(tenant.as_str()).join(id.as_str());
        for path in [&self.root, &self.root.join(tenant.as_str()), &dir] {
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(Error::Forbidden("linked blob storage path".into()));
            }
        }
        Ok(dir)
    }
}

#[async_trait]
impl BlobStorage for BlobStore {
    fn path_for(&self, tenant: &Tenant, id: &SourceId, kind: BlobKind, ext: &str) -> Result<PathBuf> {
        if ext.chars().any(|c| c.is_control() || "/\\:<>\"|?*".contains(c)) || ext.ends_with(['.', ' ']) {
            return Err(Error::Invalid("unsafe blob extension".into()));
        }
        let ext = if ext.is_empty() { String::new() } else { format!(".{}", ext.trim_start_matches('.')) };
        let path = self.dir(tenant, id)?.join(format!("{}{ext}", kind.stem()));
        if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::Forbidden("linked blob file".into()));
        }
        Ok(path)
    }

    async fn put(&self, tenant: &Tenant, id: &SourceId, kind: BlobKind, ext: &str, bytes: Bytes) -> Result<PathBuf> {
        let dir = self.dir(tenant, id)?;
        tokio::fs::create_dir_all(&dir).await?;
        let path = self.path_for(tenant, id, kind, ext)?;
        tokio::fs::write(&path, &bytes).await?;
        Ok(path)
    }

    async fn put_text(&self, tenant: &Tenant, id: &SourceId, kind: BlobKind, text: &str) -> Result<PathBuf> {
        let ext = match kind {
            BlobKind::Manifest => "json",
            _ => "txt",
        };
        self.put(tenant, id, kind, ext, Bytes::copy_from_slice(text.as_bytes())).await
    }

    async fn get(&self, tenant: &Tenant, id: &SourceId, kind: BlobKind, ext: &str) -> Result<Bytes> {
        let path = self.path_for(tenant, id, kind, ext)?;
        let mut f = tokio::fs::File::open(&path).await
            .map_err(|_| Error::NotFound(path.display().to_string()))?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).await?;
        Ok(Bytes::from(buf))
    }

    async fn delete_all(&self, tenant: &Tenant, id: &SourceId) -> Result<()> {
        let dir = self.dir(tenant, id)?;
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn identical_slugs_never_share_originals_extraction_or_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let store = BlobStore::open(temp.path()).unwrap();
        let a = Tenant::new("counsel-a"); let b = Tenant::new("litigant-b");
        let id = SourceId::from("same-filename".to_string());
        store.put(&a, &id, BlobKind::Original, "pdf", Bytes::from_static(b"attorney-only")).await.unwrap();
        store.put(&b, &id, BlobKind::Original, "pdf", Bytes::from_static(b"disclosure-copy")).await.unwrap();
        store.put_text(&a, &id, BlobKind::Extracted, "private work product").await.unwrap();
        store.put_text(&b, &id, BlobKind::Extracted, "released facts").await.unwrap();
        assert_eq!(store.get(&a,&id,BlobKind::Original,"pdf").await.unwrap().as_ref(),b"attorney-only");
        assert_eq!(store.get(&b,&id,BlobKind::Extracted,"txt").await.unwrap().as_ref(),b"released facts");
        store.delete_all(&b,&id).await.unwrap();
        assert_eq!(store.get(&a,&id,BlobKind::Extracted,"txt").await.unwrap().as_ref(),b"private work product");
        assert!(store.get(&b,&id,BlobKind::Original,"pdf").await.is_err());
    }
    #[test]
    fn unsafe_paths_and_extensions_are_rejected() {
        let temp=tempfile::tempdir().unwrap(); let store=BlobStore::open(temp.path()).unwrap();
        let tenant=Tenant::new("safe"); let id=SourceId::from("safe-source".to_string());
        for value in ["../other","..","a/b","a\\b"] {
            assert!(store.path_for(&tenant,&SourceId::from(value.to_string()),BlobKind::Original,"pdf").is_err());
            assert!(store.path_for(&Tenant(value.into()),&id,BlobKind::Original,"pdf").is_err());
        }
        assert!(store.path_for(&tenant,&id,BlobKind::Original,"../../secret").is_err());
    }
    #[cfg(unix)]
    #[test]
    fn linked_audience_directory_is_rejected() {
        let temp=tempfile::tempdir().unwrap(); let outside=tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(),temp.path().join("counsel")).unwrap();
        let store=BlobStore::open(temp.path()).unwrap();
        assert!(store.path_for(&Tenant::new("counsel"),&SourceId::from("file".to_string()),BlobKind::Original,"pdf").is_err());
    }
}
