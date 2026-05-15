//! Wiki linter — periodic health checks. See DESIGN.md §9.
//!
//! v1 covers the cheap deterministic checks that don't burn LLM calls:
//!   - orphans: pages with zero inbound `[[…]]` links
//!   - broken_links: `[[…]]` targets that don't resolve to a page
//!   - index_drift: index.md ↔ filesystem mismatch
//!   - stale_source_ids: frontmatter source_ids that no longer exist in DB
//!
//! Future v2 additions (DESIGN.md §9): contradictions (LLM clusters),
//! near-duplicates (cosine > 0.93 via Weaviate nearObject).

use anyhow::Result;
use chrono::Utc;
use qpedia_core::SourceId;
use qpedia_store::{sqlite::SourceStore, weaviate::WeaviateStore, SqliteStore, WikiRepo};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{info, warn};

/// Pairs with certainty >= this are flagged as near-duplicates.
pub const DUPLICATE_CERTAINTY: f32 = 0.93;

const SKIP_FROM_ORPHAN_CHECK: &[&str] = &["index.md", "log.md", "QPEDIA.md"];

#[derive(Debug, Default, Serialize)]
pub struct LintReport {
    pub generated_at: String,
    pub page_count: usize,
    pub orphans: Vec<String>,
    pub broken_links: Vec<BrokenLink>,
    pub index_drift: IndexDrift,
    pub stale_source_ids: Vec<String>,
    pub duplicates: Vec<Duplicate>,
}

impl LintReport {
    pub fn issue_count(&self) -> usize {
        self.orphans.len()
            + self.broken_links.len()
            + self.index_drift.missing_from_index.len()
            + self.index_drift.stale_in_index.len()
            + self.stale_source_ids.len()
            + self.duplicates.len()
    }
}

#[derive(Debug, Serialize)]
pub struct Duplicate {
    pub a: String,
    pub b: String,
    pub certainty: f32,
}

#[derive(Debug, Serialize)]
pub struct BrokenLink {
    pub page: String,
    pub target: String,
}

#[derive(Debug, Default, Serialize)]
pub struct IndexDrift {
    pub missing_from_index: Vec<String>,
    pub stale_in_index: Vec<String>,
}

pub struct Linter {
    pub wiki: WikiRepo,
    pub db: SqliteStore,
    pub weaviate: Option<Arc<WeaviateStore>>,
}

impl Linter {
    pub fn new(wiki: WikiRepo, db: SqliteStore, weaviate: Option<Arc<WeaviateStore>>) -> Self {
        Self { wiki, db, weaviate }
    }

    pub async fn run(&self) -> Result<LintReport> {
        let pages = self.wiki.list_pages("").await?;
        let page_set: HashSet<String> = pages.iter().cloned().collect();

        let mut report = LintReport {
            generated_at: Utc::now().to_rfc3339(),
            page_count: pages.len(),
            ..Default::default()
        };

        // Walk pages once; record content + outbound links.
        let mut inbound: HashMap<String, usize> = HashMap::new();
        let mut content_cache: HashMap<String, String> = HashMap::new();

        for path in &pages {
            let Some(content) = self.wiki.read_page(path).await? else { continue };
            for tgt in extract_wikilinks(&content) {
                let path_only = tgt.split('#').next().unwrap_or(&tgt).to_string();
                *inbound.entry(path_only.clone()).or_insert(0) += 1;
                if !page_set.contains(&path_only) {
                    report.broken_links.push(BrokenLink {
                        page: path.clone(),
                        target: tgt,
                    });
                }
            }
            content_cache.insert(path.clone(), content);
        }

        // Orphans: pages no one links to (excluding index/log/QPEDIA).
        for path in &pages {
            if SKIP_FROM_ORPHAN_CHECK.iter().any(|s| s == path) {
                continue;
            }
            if inbound.get(path).copied().unwrap_or(0) == 0 {
                report.orphans.push(path.clone());
            }
        }

        // Index drift: index.md ↔ disk.
        if let Some(index) = content_cache.get("index.md") {
            let listed: HashSet<String> = extract_wikilinks(index)
                .into_iter()
                .map(|t| t.split('#').next().unwrap_or(&t).to_string())
                .collect();
            for path in &pages {
                if SKIP_FROM_ORPHAN_CHECK.iter().any(|s| s == path) {
                    continue;
                }
                if !listed.contains(path) {
                    report.index_drift.missing_from_index.push(path.clone());
                }
            }
            for entry in &listed {
                if !page_set.contains(entry) {
                    report.index_drift.stale_in_index.push(entry.clone());
                }
            }
        }

        // Stale source_ids: frontmatter refs DB doesn't have.
        let mut all_referenced: HashSet<String> = HashSet::new();
        for content in content_cache.values() {
            for sid in extract_source_ids(content) {
                all_referenced.insert(sid);
            }
        }
        for sid in &all_referenced {
            if self.db.get_source(&SourceId::from(sid.clone())).await?.is_none() {
                report.stale_source_ids.push(sid.clone());
            }
        }
        report.stale_source_ids.sort();

        // Near-duplicates via Weaviate nearObject. Skip system pages
        // (they're intentionally distinct and won't have neighbors anyway).
        if let Some(weaviate) = &self.weaviate {
            let mut seen_pairs: HashSet<(String, String)> = HashSet::new();
            for path in &pages {
                if SKIP_FROM_ORPHAN_CHECK.iter().any(|s| s == path) {
                    continue;
                }
                match weaviate.nearest_neighbor(path).await {
                    Ok(Some((other, certainty))) if certainty >= DUPLICATE_CERTAINTY => {
                        let pair = sorted_pair(path, &other);
                        if seen_pairs.insert(pair.clone()) {
                            report.duplicates.push(Duplicate {
                                a: pair.0,
                                b: pair.1,
                                certainty,
                            });
                        }
                    }
                    Ok(_) => {}
                    Err(e) => warn!(path = %path, error = %e, "nearest_neighbor failed"),
                }
            }
            report
                .duplicates
                .sort_by(|x, y| y.certainty.partial_cmp(&x.certainty).unwrap_or(std::cmp::Ordering::Equal));
        }

        info!(
            pages = report.page_count,
            issues = report.issue_count(),
            orphans = report.orphans.len(),
            broken = report.broken_links.len(),
            duplicates = report.duplicates.len(),
            "lint complete"
        );
        Ok(report)
    }
}

fn sorted_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

// ---------- helpers ----------

/// Extract `[[wikilinks]]` from markdown, skipping inline code spans (single
/// backticks) and fenced code blocks (triple backticks). Without this, prose
/// like `` `[[path/to/page.md]]` `` in QPEDIA.md gets mis-flagged as broken.
fn extract_wikilinks(content: &str) -> Vec<String> {
    let chars: Vec<char> = content.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut in_inline = false;
    let mut in_fence = false;

    while i < chars.len() {
        // Triple-backtick fence toggles when not inside an inline span.
        if !in_inline
            && i + 2 < chars.len()
            && chars[i] == '`' && chars[i + 1] == '`' && chars[i + 2] == '`'
        {
            in_fence = !in_fence;
            i += 3;
            continue;
        }
        // Single backtick toggles inline code (only outside fences).
        if !in_fence && chars[i] == '`' {
            in_inline = !in_inline;
            i += 1;
            continue;
        }
        // [[wikilink]] only when not inside any code.
        if !in_inline && !in_fence
            && i + 1 < chars.len() && chars[i] == '[' && chars[i + 1] == '['
        {
            let mut j = i + 2;
            while j + 1 < chars.len() && !(chars[j] == ']' && chars[j + 1] == ']') {
                j += 1;
            }
            if j + 1 < chars.len() {
                let target: String = chars[i + 2..j].iter().collect::<String>().trim().to_string();
                if !target.is_empty() {
                    out.push(target);
                }
                i = j + 2;
                continue;
            } else {
                break;
            }
        }
        i += 1;
    }
    out
}

fn extract_source_ids(content: &str) -> Vec<String> {
    let trimmed = content.trim_start();
    let Some(after) = trimmed.strip_prefix("---") else { return Vec::new() };
    let Some(end) = after.find("\n---") else { return Vec::new() };
    let fm = &after[..end];
    let mut out = Vec::new();
    for line in fm.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("source_ids:") {
            let s = rest.trim().trim_start_matches('[').trim_end_matches(']');
            for x in s.split(',') {
                let x = x.trim().trim_matches('"').trim_matches('\'');
                if !x.is_empty() {
                    out.push(x.to_string());
                }
            }
        }
    }
    out
}
