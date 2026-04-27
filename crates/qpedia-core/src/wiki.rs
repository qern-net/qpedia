use crate::{acl::Acl, PageId, SourceId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// LLM-authored wiki page. Persisted as markdown in the git repo
/// and mirrored into Weaviate for search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub id: PageId,
    pub path: String,                       // e.g. "concepts/revenue_model.md"
    pub kind: PageKind,
    pub title: String,
    pub content: String,                    // full markdown body
    pub frontmatter: Frontmatter,
    pub acl: Acl,                           // derived: union of source ACLs
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageKind {
    Concept,
    Entity,
    Comparison,
    Summary,
    Meta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frontmatter {
    pub id: PageId,
    pub title: String,
    pub kind: PageKind,
    #[serde(default)]
    pub source_ids: Vec<SourceId>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub links_out: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub embedding_hash: Option<String>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
}

fn default_confidence() -> f32 { 1.0 }

/// A proposed change to the wiki, staged before commit. See DESIGN.md §6.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffBundle {
    pub ingest_id: String,
    pub operations: Vec<DiffOp>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DiffOp {
    Create { path: String, content: String, rationale: String },
    Patch  { path: String, new_content: String, rationale: String },
    Delete { path: String, rationale: String },
    Link   { from: String, to: String, kind: String },
}
