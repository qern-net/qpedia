//! Storage layer: SQLite (jobs/audit/sources), Weaviate (vectors + wiki objects),
//! git (wiki markdown), and filesystem (raw docs).
//!
//! Each submodule exposes a trait so tests can swap in-memory impls.

pub mod sqlite;
pub mod weaviate;
pub mod wikirepo;
pub mod blob;

pub use sqlite::{SqliteStore, SourceStore, JobQueue};
pub use weaviate::WeaviateStore;
pub use wikirepo::{WikiRepo, WikiRepoStore, SearchHit};
pub use blob::{BlobStore, BlobStorage, BlobKind};
