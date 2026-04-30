//! Weaviate client. See DESIGN.md §2.4.
//!
//! Talks to Weaviate's REST + GraphQL endpoints. We use Weaviate as a
//! combined vector + object store: the wiki page content lives here too,
//! not just the vector. Cross-references between pages will be modeled
//! later via Weaviate's native ref properties.

use crate::wikirepo::SearchHit;
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{info, warn};
use uuid::Uuid;

const CLASS_WIKI_PAGE: &str = "WikiPage";

/// Stable namespace for deriving Weaviate object UUIDs from page paths.
/// Reusing the standard URL namespace UUID — paths-as-keys are URL-like —
/// gives us deterministic, reproducible IDs without minting our own.
const NAMESPACE: Uuid = Uuid::NAMESPACE_URL;

#[derive(Clone)]
pub struct WeaviateStore {
    client: reqwest::Client,
    base_url: String,
}

impl WeaviateStore {
    pub fn new(base_url: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    /// Ping the readiness endpoint. Cheap; safe to call on startup.
    pub async fn ready(&self) -> Result<()> {
        let url = format!("{}/v1/.well-known/ready", self.base_url);
        let resp = self.client.get(&url).send().await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("weaviate not ready: {}", resp.status()));
        }
        Ok(())
    }

    /// Create the WikiPage class if absent. No-op when the class exists.
    pub async fn ensure_schema(&self) -> Result<()> {
        let url = format!("{}/v1/schema/{CLASS_WIKI_PAGE}", self.base_url);
        let exists = self.client.get(&url).send().await?.status().is_success();
        if exists {
            return Ok(());
        }
        let body = serde_json::json!({
            "class": CLASS_WIKI_PAGE,
            "description": "LLM-authored wiki page (Karpathy LLMWiki layer).",
            "vectorizer": "none",
            "vectorIndexType": "hnsw",
            "properties": [
                {"name": "page_id",    "dataType": ["text"]},
                {"name": "path",       "dataType": ["text"]},
                {"name": "kind",       "dataType": ["text"]},
                {"name": "title",      "dataType": ["text"]},
                {"name": "content",    "dataType": ["text"]},
                {"name": "tags",       "dataType": ["text[]"]},
                {"name": "source_ids", "dataType": ["text[]"]},
                {"name": "updated_at", "dataType": ["date"]}
            ]
        });
        let url = format!("{}/v1/schema", self.base_url);
        let resp = self.client.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("create schema failed ({status}): {text}"));
        }
        info!(class = CLASS_WIKI_PAGE, "weaviate schema created");
        Ok(())
    }

    /// Insert or replace a page. Uses a deterministic UUID derived from
    /// the page path so reingests target the same object.
    pub async fn upsert_page(&self, page: &WikiPageRecord, vector: Vec<f32>) -> Result<()> {
        let id = page_uuid(&page.path);
        // Weaviate uses PUT for replace-by-id; falls back to POST when missing.
        let put_url = format!("{}/v1/objects/{CLASS_WIKI_PAGE}/{id}", self.base_url);
        let body = serde_json::json!({
            "class": CLASS_WIKI_PAGE,
            "id": id.to_string(),
            "properties": {
                "page_id": page.page_id,
                "path": page.path,
                "kind": page.kind,
                "title": page.title,
                "content": page.content,
                "tags": page.tags,
                "source_ids": page.source_ids,
                "updated_at": page.updated_at,
            },
            "vector": vector,
        });

        let resp = self.client.put(&put_url).json(&body).send().await?;
        if resp.status().is_success() {
            return Ok(());
        }
        // 404 → object doesn't exist, fall back to POST.
        if resp.status().as_u16() == 404 {
            let post_url = format!("{}/v1/objects", self.base_url);
            let resp = self.client.post(&post_url).json(&body).send().await?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(anyhow!("weaviate POST object ({status}): {text}"));
            }
            return Ok(());
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(anyhow!("weaviate PUT object ({status}): {text}"))
    }

    pub async fn delete_page(&self, path: &str) -> Result<()> {
        let id = page_uuid(path);
        let url = format!("{}/v1/objects/{CLASS_WIKI_PAGE}/{id}", self.base_url);
        let resp = self.client.delete(&url).send().await?;
        if resp.status().is_success() || resp.status().as_u16() == 404 {
            return Ok(());
        }
        Err(anyhow!("weaviate DELETE ({}): {}", resp.status(), resp.text().await.unwrap_or_default()))
    }

    /// Hybrid (BM25 + vector) search. `alpha` is vector weight (0..=1);
    /// 0.7 by default matches DESIGN.md §2.4.
    pub async fn hybrid_search(
        &self,
        query: &str,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let alpha = 0.7;
        // Inline the vector since GraphQL variables for arrays can be tricky
        // across server versions; this stays explicit and version-stable.
        let vec_str = vector
            .iter()
            .map(|f| format!("{f:.6}"))
            .collect::<Vec<_>>()
            .join(",");
        let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
        let gql = format!(
            r#"{{
              Get {{
                {class}(
                  hybrid: {{ query: "{q}", vector: [{vec}], alpha: {a} }}
                  limit: {limit}
                ) {{
                  path
                  title
                  content
                  _additional {{ score }}
                }}
              }}
            }}"#,
            class = CLASS_WIKI_PAGE,
            q = escaped,
            vec = vec_str,
            a = alpha,
            limit = limit,
        );
        let url = format!("{}/v1/graphql", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({"query": gql}))
            .send()
            .await?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(anyhow!("weaviate graphql ({status}): {text}"));
        }
        let parsed: GqlResponse = serde_json::from_str(&text)
            .map_err(|e| anyhow!("decode gql: {e}\nbody: {text}"))?;
        if let Some(errs) = parsed.errors {
            if !errs.is_empty() {
                warn!(?errs, "weaviate graphql returned errors");
            }
        }
        let hits = parsed
            .data
            .and_then(|d| d.get.into_iter().next())
            .map(|(_, items)| items)
            .unwrap_or_default()
            .into_iter()
            .map(|h| SearchHit {
                path: h.path.unwrap_or_default(),
                title: h.title.unwrap_or_default(),
                snippet: snippet_from(&h.content.unwrap_or_default(), query),
            })
            .collect();
        Ok(hits)
    }
}

fn page_uuid(path: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE, path.as_bytes())
}

fn snippet_from(content: &str, query: &str) -> String {
    let q = query.trim().to_lowercase();
    let lower = content.to_lowercase();
    let pos = lower.find(&q).unwrap_or(0);
    let start = pos.saturating_sub(80);
    let end = (pos + q.len() + 80).min(content.len());
    let mut s = content[start..end].replace('\n', " ");
    s = s.chars().take(200).collect();
    s
}

#[derive(Debug, Clone, Serialize)]
pub struct WikiPageRecord {
    pub page_id: String,
    pub path: String,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source_ids: Vec<String>,
    pub updated_at: String, // RFC 3339
}

// ---------- GraphQL response shapes ----------

#[derive(Debug, Deserialize)]
struct GqlResponse {
    data: Option<GqlData>,
    #[serde(default)]
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct GqlData {
    #[serde(rename = "Get")]
    get: std::collections::BTreeMap<String, Vec<GqlHit>>,
}

#[derive(Debug, Deserialize)]
struct GqlHit {
    path: Option<String>,
    title: Option<String>,
    content: Option<String>,
}

/// Build a WeaviateStore from env. Returns None when no URL is configured
/// or the server isn't ready yet (degrades gracefully — agent search falls
/// back to filesystem grep).
pub async fn weaviate_from_env() -> Option<WeaviateStore> {
    let url = std::env::var("QPEDIA_WEAVIATE_URL").ok()?;
    if url.trim().is_empty() {
        return None;
    }
    let store = WeaviateStore::new(url);
    match store.ready().await {
        Ok(()) => match store.ensure_schema().await {
            Ok(()) => {
                info!("weaviate connected and schema ready");
                Some(store)
            }
            Err(e) => {
                warn!(error = %e, "weaviate schema bootstrap failed — disabling");
                None
            }
        },
        Err(e) => {
            warn!(error = %e, "weaviate not ready — disabling (start it via docker compose)");
            None
        }
    }
}
