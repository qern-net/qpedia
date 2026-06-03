//! Filesystem-only storage primitives. SQL is in `qpedia-pg-store`.
//!
//! This crate owns:
//!   - `WikiRepo` / `WikiRepoStore`: per-tenant git repos on disk.
//!   - `BlobStore`: raw + extracted blobs under `/data/raw/<id>/`.

pub mod blob;
pub mod wikirepo;

pub use blob::{BlobKind, BlobStorage, BlobStore};
pub use wikirepo::{SearchHit, WikiRepo, WikiRepoStore};
