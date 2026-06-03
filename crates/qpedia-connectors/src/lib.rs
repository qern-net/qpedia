//! External document connectors. Each connector knows how to enumerate
//! and download docs from one source-of-truth system (Confluence Cloud,
//! Google Drive, SharePoint, ...). The Sync job runner asks a connector
//! for "what changed since the last cursor" and feeds each doc through
//! the normal /sources upload path so it lands in the right tenant and
//! gets the full ingest treatment.
//!
//! See DESIGN.md §16. v1 ships the trait and a working Confluence Cloud
//! impl; GDrive / SharePoint are documented stubs.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod confluence;
pub mod gdrive;
pub mod oauth;

pub use confluence::ConfluenceConnector;
pub use gdrive::GoogleDriveConnector;

/// Persistent connector configuration, mirrored from the `connectors`
/// SQLite table. `config_json` is connector-specific (see each impl).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorConfig {
    pub id: String,
    pub tenant: String,
    pub kind: String,                   // "confluence" | "gdrive" | "sharepoint"
    pub name: String,
    pub config_json: serde_json::Value,
    pub cursor: Option<String>,
    pub enabled: bool,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteDoc {
    pub remote_id: String,
    pub name: String,
    pub mime: String,
    pub modified_at: Option<DateTime<Utc>>,
    pub size_bytes: Option<u64>,
}

pub struct Downloaded {
    pub bytes: bytes::Bytes,
    pub mime: String,
    pub filename: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListChanged {
    pub docs: Vec<RemoteDoc>,
    pub new_cursor: Option<String>,
}

#[async_trait]
pub trait Connector: Send + Sync {
    fn kind(&self) -> &'static str;
    /// Docs that are new or modified since `cursor`. `cursor` is opaque
    /// per connector; pass None for an initial enumeration.
    async fn list_changed(&self, cursor: Option<&str>) -> Result<ListChanged>;
    /// Download the body of a doc.
    async fn download(&self, doc: &RemoteDoc) -> Result<Downloaded>;
}

/// Factory: build a concrete Connector from a stored `ConnectorConfig`.
/// Unknown kinds return an error so adding a new connector is a single
/// match arm.
pub fn build(config: &ConnectorConfig) -> Result<Box<dyn Connector>> {
    match config.kind.as_str() {
        "confluence" => Ok(Box::new(ConfluenceConnector::from_config(&config.config_json)?)),
        "gdrive" => Ok(Box::new(GoogleDriveConnector::from_config(&config.config_json)?)),
        // SharePoint/OneDrive (deltaLink on driveItems) + Slack live in
        // qpedia-pvt; GitHub joins this crate next. All four share the
        // OAuth refresh-token credential shape in `oauth.rs`.
        other => Err(anyhow::anyhow!("unknown connector kind: {other}")),
    }
}
