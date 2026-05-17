use crate::{acl::Acl, tenant::Tenant, SourceId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Raw document uploaded by a user. Immutable once ingested.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: SourceId,
    #[serde(default)]
    pub tenant: Tenant,
    pub folder_path: String,
    pub filename: String,
    pub mime: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub acl: Acl,
    pub status: SourceStatus,
    pub language: Option<String>,
    pub created_at: DateTime<Utc>,
    pub ingested_at: Option<DateTime<Utc>>,
    /// Classifier output: `{doc_type, language, sensitivity, hints, ...}`.
    /// Populated by the Classify phase. See DESIGN.md §5.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceStatus {
    Pending,
    Extracting,
    Extracted,
    Classifying,
    Classified,
    AgentDistilling,
    AgentDistilled,
    Validating,
    Validated,
    Committing,
    Committed,
    Embedding,
    Done,
    /// Stopped mid-pipeline due to a missing dependency (e.g. no LLM configured).
    /// The source can be resumed by re-enqueueing an Ingest job once the
    /// dependency is available.
    Tainted,
    Failed,
    Dead,
}
