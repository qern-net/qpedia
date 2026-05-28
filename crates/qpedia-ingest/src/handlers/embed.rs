//! Embedding phase: walk the pages touched by the latest wiki commit,
//! embed each, and upsert into the `wiki_pages` table (pgvector + tsvector).
//! See SPEC-v2.md §5.1.
//!
//! Idempotent: `wiki_pages` is keyed on (tenant_id, path) so re-embedding
//! the same page replaces in place. Degrades cleanly when no embedder is
//! configured (transitions straight to Done so the queue doesn't stall).

use crate::runner::IngestContext;
use anyhow::{anyhow, Result};
use qpedia_core::{source::SourceStatus, tenant::Tenant, SourceId};
use qpedia_pg_store::WikiPageUpsert;
use tracing::{info, warn};

const MAX_EMBED_CHARS: usize = 8000;

/// Per-source ingest phase: update status, embed pages from HEAD, audit.
pub async fn run(ctx: &IngestContext, tenant: &Tenant, source_id: &SourceId) -> Result<()> {
    if ctx.embedder.is_none() {
        info!(id = %source_id, "no embedder — marking Done");
        ctx.db.update_status(tenant, source_id, SourceStatus::Done).await?;
        return Ok(());
    }

    ctx.db.update_status(tenant, source_id, SourceStatus::Embedding).await?;

    let touched = embed_changed_pages(ctx, tenant).await?;
    if touched.is_empty() {
        warn!(id = %source_id, "embed: no markdown pages touched in HEAD");
    }

    ctx.db.update_status(tenant, source_id, SourceStatus::Done).await?;
    ctx.db
        .write_audit(
            tenant,
            "qpedia-bot",
            "wiki.embedded",
            Some(source_id.as_str()),
            Some(&serde_json::json!({"pages": touched, "tenant": tenant.as_str()})),
        )
        .await?;

    info!(id = %source_id, pages = touched.len(), "embed phase complete");
    Ok(())
}

/// Re-embed exactly the pages touched by HEAD and upsert into pgvector.
/// Used by both the per-source embed phase and the remove pipeline (which
/// re-embeds patched pages after stripping a removed source from frontmatter).
///
/// Returns the list of paths embedded. No-ops cleanly when embedder is missing.
pub async fn embed_changed_pages(ctx: &IngestContext, tenant: &Tenant) -> Result<Vec<String>> {
    let Some(embedder) = ctx.embedder.as_ref() else {
        return Ok(Vec::new());
    };

    let wiki = ctx.wiki_store.get(tenant).await?;
    let touched = wiki.changed_in_head().await?;
    if touched.is_empty() {
        return Ok(touched);
    }

    let mut records: Vec<(String, WikiPageUpsert)> = Vec::new();
    let mut embed_inputs: Vec<String> = Vec::new();

    for path in &touched {
        let Some(content) = wiki.read_page(path).await? else { continue };
        let fm = parse_frontmatter(&content);
        let title = fm.title.clone().unwrap_or_else(|| path.clone());
        let body = strip_frontmatter(&content);
        let embed_text: String = format!("{title}\n\n{body}")
            .chars()
            .take(MAX_EMBED_CHARS)
            .collect();
        let upsert = WikiPageUpsert {
            page_id: fm.id.unwrap_or_else(|| path.clone()),
            path: path.clone(),
            kind: fm.kind.unwrap_or_default(),
            title,
            content,
            tags: fm.tags,
            source_ids: fm.source_ids,
        };
        records.push((path.clone(), upsert));
        embed_inputs.push(embed_text);
    }

    if records.is_empty() {
        return Ok(touched);
    }

    let inputs_ref: Vec<&str> = embed_inputs.iter().map(|s| s.as_str()).collect();
    let vectors = embedder.embed(&inputs_ref).await.map_err(|e| anyhow!("embed: {e}"))?;
    if vectors.len() != records.len() {
        return Err(anyhow!(
            "embedder returned {} vectors for {} inputs",
            vectors.len(),
            records.len()
        ));
    }

    for ((path, upsert), vector) in records.into_iter().zip(vectors) {
        ctx.db
            .upsert_wiki_page(tenant, &upsert, vector)
            .await
            .map_err(|e| anyhow!("wiki_pages upsert {}: {e}", path))?;
    }
    Ok(touched)
}

// ---------- frontmatter parsing (lenient) ----------

#[derive(Debug, Default, Clone)]
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
        .trim_start_matches('"')
        .trim_end_matches('"')
        .trim_start_matches('\'')
        .trim_end_matches('\'')
        .to_string()
}

fn parse_inline_list(s: &str) -> Vec<String> {
    let s = s.trim();
    if !s.starts_with('[') {
        return Vec::new();
    }
    let s = s.trim_start_matches('[').trim_end_matches(']');
    s.split(',')
        .map(|x| unquote(x.trim()))
        .filter(|x| !x.is_empty())
        .collect()
}
