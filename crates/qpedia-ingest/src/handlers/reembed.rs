//! Reembed job handler: clear Weaviate for a tenant and rebuild from the
//! git wiki repo. Git is the source of truth; Weaviate is a derived index.
//!
//! Use this when:
//!   - Weaviate data is lost or corrupted
//!   - The embedding model is changed (QPEDIA_EMBED_MODEL)
//!   - Weaviate schema is reset
//!   - Search results look wrong and a full rebuild is needed
//!
//! The job is idempotent: running it twice produces the same result.

use crate::runner::IngestContext;
use anyhow::{anyhow, Result};
use chrono::Utc;
use qpedia_core::tenant::Tenant;
use qpedia_store::weaviate::WikiPageRecord;
use tracing::{info, warn};

const MAX_EMBED_CHARS: usize = 8000;
/// Embed pages in batches to avoid sending thousands of texts at once.
const BATCH_SIZE: usize = 32;

pub async fn run(ctx: &IngestContext, tenant: &Tenant) -> Result<()> {
    let (Some(embedder), Some(weaviate)) = (ctx.embedder.as_ref(), ctx.weaviate.as_ref()) else {
        return Err(anyhow!("reembed requires both an embedder and Weaviate to be configured"));
    };

    let wiki = ctx.wiki_store.get(tenant).await?;
    let all_pages = wiki.list_pages("").await?;

    info!(
        tenant = %tenant,
        pages = all_pages.len(),
        "reembed: starting full rebuild from git"
    );

    // Step 1: clear all existing Weaviate objects for this tenant.
    let deleted = weaviate.delete_tenant_pages(tenant).await?;
    info!(tenant = %tenant, deleted, "reembed: cleared Weaviate");

    // Step 2: read, embed, and upsert every page from the git repo in batches.
    let now = Utc::now().to_rfc3339();
    let mut total_embedded = 0usize;
    let mut total_skipped = 0usize;

    for chunk in all_pages.chunks(BATCH_SIZE) {
        let mut records: Vec<(String, WikiPageRecord)> = Vec::new();
        let mut embed_inputs: Vec<String> = Vec::new();

        for path in chunk {
            let Some(content) = wiki.read_page(path).await? else {
                total_skipped += 1;
                continue;
            };
            let fm = parse_frontmatter(&content);
            let title = fm.title.clone().unwrap_or_else(|| path.clone());
            let body = strip_frontmatter(&content);
            let embed_text: String = format!("{title}\n\n{body}")
                .chars()
                .take(MAX_EMBED_CHARS)
                .collect();

            let record = WikiPageRecord {
                page_id: fm.id.unwrap_or_else(|| path.clone()),
                tenant: tenant.clone(),
                path: path.clone(),
                kind: fm.kind.unwrap_or_default(),
                title,
                content,
                tags: fm.tags,
                source_ids: fm.source_ids,
                updated_at: now.clone(),
            };
            records.push((path.clone(), record));
            embed_inputs.push(embed_text);
        }

        if records.is_empty() {
            continue;
        }

        let inputs_ref: Vec<&str> = embed_inputs.iter().map(|s| s.as_str()).collect();
        let vectors = embedder
            .embed(&inputs_ref)
            .await
            .map_err(|e| anyhow!("embed batch: {e}"))?;

        if vectors.len() != records.len() {
            return Err(anyhow!(
                "embedder returned {} vectors for {} inputs",
                vectors.len(),
                records.len()
            ));
        }

        for ((path, record), vector) in records.into_iter().zip(vectors) {
            if let Err(e) = weaviate.upsert_page(&record, vector).await {
                warn!(path = %path, error = %e, "reembed: upsert failed, skipping page");
                total_skipped += 1;
            } else {
                total_embedded += 1;
            }
        }
    }

    info!(
        tenant = %tenant,
        total_embedded,
        total_skipped,
        "reembed: complete"
    );

    let _ = ctx.db.audit(
        "qpedia-bot",
        "wiki.reembedded",
        Some(tenant.as_str()),
        Some(&serde_json::json!({
            "tenant": tenant.as_str(),
            "embedded": total_embedded,
            "skipped": total_skipped,
            "cleared": deleted,
        })),
    ).await;

    Ok(())
}

// ---------- frontmatter parsing (same as embed.rs, kept local) ----------

#[derive(Debug, Default)]
struct Frontmatter {
    id: Option<String>,
    title: Option<String>,
    kind: Option<String>,
    tags: Vec<String>,
    source_ids: Vec<String>,
}

fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start();
    let Some(after) = trimmed.strip_prefix("---") else { return content };
    match after.find("\n---") {
        Some(end) => after[end + "\n---".len()..].trim_start(),
        None => content,
    }
}

fn parse_frontmatter(content: &str) -> Frontmatter {
    let mut r = Frontmatter::default();
    let trimmed = content.trim_start();
    let Some(after) = trimmed.strip_prefix("---") else { return r };
    let Some(end) = after.find("\n---") else { return r };
    let fm = &after[..end];
    for line in fm.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("id:") {
            r.id = Some(unquote(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("title:") {
            r.title = Some(unquote(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("kind:") {
            r.kind = Some(unquote(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("tags:") {
            r.tags = parse_inline_list(rest.trim());
        } else if let Some(rest) = line.strip_prefix("source_ids:") {
            r.source_ids = parse_inline_list(rest.trim());
        }
    }
    r
}

fn unquote(s: &str) -> String {
    s.trim()
        .trim_start_matches('"').trim_end_matches('"')
        .trim_start_matches('\'').trim_end_matches('\'')
        .to_string()
}

fn parse_inline_list(s: &str) -> Vec<String> {
    let s = s.trim();
    if !s.starts_with('[') { return Vec::new(); }
    let s = s.trim_start_matches('[').trim_end_matches(']');
    s.split(',')
        .map(|x| unquote(x.trim()))
        .filter(|x| !x.is_empty())
        .collect()
}
