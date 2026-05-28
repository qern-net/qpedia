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

use anyhow::{anyhow, Result};
use chrono::Utc;
use qpedia_core::{tenant::Tenant, SourceId};
use qpedia_llm::{current_model, CompleteReq, LlmProvider};
use qpedia_pg_store::PgStore;
use qpedia_store::WikiRepo;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{info, warn};

/// Pairs with certainty >= this are flagged as near-duplicates.
pub const DUPLICATE_CERTAINTY: f32 = 0.93;

/// Contradiction detection budget (env-overridable).
const DEFAULT_MAX_CLUSTERS: usize = 8;
const DEFAULT_MAX_PAGES_PER_CLUSTER: usize = 6;
const PAGE_EXCERPT_CHARS: usize = 1500;

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
    pub contradictions: Vec<Contradiction>,
}

impl LintReport {
    pub fn issue_count(&self) -> usize {
        self.orphans.len()
            + self.broken_links.len()
            + self.index_drift.missing_from_index.len()
            + self.index_drift.stale_in_index.len()
            + self.stale_source_ids.len()
            + self.duplicates.len()
            + self.contradictions.len()
    }
}

#[derive(Debug, Serialize)]
pub struct Duplicate {
    pub a: String,
    pub b: String,
    pub certainty: f32,
}

#[derive(Debug, Serialize)]
pub struct Contradiction {
    pub tag: String,
    pub pages: Vec<String>,
    pub summary: String,
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
    pub db: PgStore,
    pub llm: Option<Arc<dyn LlmProvider>>,
    pub tenant: Tenant,
}

impl Linter {
    pub fn new(
        wiki: WikiRepo,
        db: PgStore,
        llm: Option<Arc<dyn LlmProvider>>,
        tenant: Tenant,
    ) -> Self {
        Self { wiki, db, llm, tenant }
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
            if self
                .db
                .get_source_in(&self.tenant, &SourceId::from(sid.clone()))
                .await?
                .is_none()
            {
                report.stale_source_ids.push(sid.clone());
            }
        }
        report.stale_source_ids.sort();

        // Near-duplicates via pgvector self-join. The skip-list of system
        // pages is enforced here by filtering the returned pairs.
        match self
            .db
            .near_duplicates(&self.tenant, DUPLICATE_CERTAINTY, 200)
            .await
        {
            Ok(pairs) => {
                for (a, b, sim) in pairs {
                    if SKIP_FROM_ORPHAN_CHECK.iter().any(|s| *s == a || *s == b) {
                        continue;
                    }
                    report.duplicates.push(Duplicate { a, b, certainty: sim });
                }
                report.duplicates.sort_by(|x, y| {
                    y.certainty.partial_cmp(&x.certainty).unwrap_or(std::cmp::Ordering::Equal)
                });
            }
            Err(e) => warn!(error = %e, "near_duplicates query failed"),
        }

        // Contradiction detection: cluster by shared tag, ask the LLM
        // per-cluster. Bounded by env knobs to control cost.
        if self.llm.is_some() {
            match self.detect_contradictions(&pages, &content_cache).await {
                Ok(cs) => report.contradictions = cs,
                Err(e) => warn!(error = %e, "contradiction detection failed"),
            }
        }

        info!(
            pages = report.page_count,
            issues = report.issue_count(),
            orphans = report.orphans.len(),
            broken = report.broken_links.len(),
            duplicates = report.duplicates.len(),
            contradictions = report.contradictions.len(),
            "lint complete"
        );
        Ok(report)
    }

    async fn detect_contradictions(
        &self,
        pages: &[String],
        content_cache: &HashMap<String, String>,
    ) -> Result<Vec<Contradiction>> {
        let Some(llm) = &self.llm else { return Ok(Vec::new()) };

        let max_clusters = std::env::var("QPEDIA_LINT_MAX_CLUSTERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_CLUSTERS);
        let max_pages_per_cluster = std::env::var("QPEDIA_LINT_MAX_PAGES_PER_CLUSTER")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(DEFAULT_MAX_PAGES_PER_CLUSTER);

        // Build tag -> [paths] map.
        let mut by_tag: HashMap<String, Vec<String>> = HashMap::new();
        for path in pages {
            if SKIP_FROM_ORPHAN_CHECK.iter().any(|s| s == path) { continue; }
            let Some(content) = content_cache.get(path) else { continue };
            for tag in extract_tags(content) {
                by_tag.entry(tag).or_default().push(path.clone());
            }
        }

        // Keep clusters with >=2 pages, sorted by size desc, capped.
        let mut clusters: Vec<(String, Vec<String>)> = by_tag
            .into_iter()
            .filter(|(_, ps)| ps.len() >= 2)
            .collect();
        clusters.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
        clusters.truncate(max_clusters);

        let mut out: Vec<Contradiction> = Vec::new();
        for (tag, mut paths) in clusters {
            paths.truncate(max_pages_per_cluster);
            let user_msg = build_cluster_prompt(&tag, &paths, content_cache);
            let req = CompleteReq::new(current_model())
                .system(CONTRADICTION_SYSTEM)
                .user(user_msg)
                .max_tokens(800)
                .temperature(0.0);
            let resp = match llm.complete(req).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(tag = %tag, error = %e, "contradiction llm call failed");
                    continue;
                }
            };
            match parse_contradiction_response(&resp.content) {
                Ok(items) => {
                    for item in items {
                        // Drop empty/garbage responses.
                        if item.pages.len() < 2 || item.summary.trim().is_empty() {
                            continue;
                        }
                        out.push(Contradiction {
                            tag: tag.clone(),
                            pages: item.pages,
                            summary: item.summary,
                        });
                    }
                }
                Err(e) => warn!(tag = %tag, error = %e, "couldn't parse contradiction JSON; got: {}", resp.content),
            }
        }

        Ok(out)
    }
}

const CONTRADICTION_SYSTEM: &str = r#"You are reviewing wiki pages that share a topic tag.
Identify contradictions BETWEEN the pages — claims in one page that
directly contradict claims in another. Don't flag stylistic differences
or different angles on the same fact — only real contradictions.

Output ONE JSON array, nothing else:
[
  {
    "pages": ["path/a.md", "path/b.md"],
    "summary": "<one sentence describing the contradiction>"
  }
]

If there are no contradictions, output [].
- Output starts with [ and ends with ].
- No markdown fences, no preamble.
- Each finding's `pages` must reference paths from the inputs.
"#;

fn build_cluster_prompt(tag: &str, paths: &[String], cache: &HashMap<String, String>) -> String {
    let mut s = String::new();
    s.push_str(&format!("Shared tag: {tag}\n\nPAGES:\n\n"));
    for (i, path) in paths.iter().enumerate() {
        s.push_str(&format!("--- {} {}\n", i + 1, path));
        let body = cache.get(path).map(|c| c.as_str()).unwrap_or("");
        let body = strip_frontmatter_for_prompt(body);
        let excerpt: String = body.chars().take(PAGE_EXCERPT_CHARS).collect();
        s.push_str(&excerpt);
        if !s.ends_with('\n') { s.push('\n'); }
        s.push('\n');
    }
    s
}

fn strip_frontmatter_for_prompt(content: &str) -> &str {
    let trimmed = content.trim_start();
    let Some(after) = trimmed.strip_prefix("---") else { return content };
    match after.find("\n---") {
        Some(end) => after[end + "\n---".len()..].trim_start(),
        None => content,
    }
}

#[derive(Debug, Deserialize)]
struct ContradictionOut {
    pages: Vec<String>,
    summary: String,
}

fn parse_contradiction_response(text: &str) -> Result<Vec<ContradictionOut>> {
    let trimmed = text.trim();
    // Strip optional code fences.
    let cleaned = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim_end_matches("```")
        .trim();
    // Seek the first '['.
    let start = cleaned.find('[').ok_or_else(|| anyhow!("no JSON array"))?;
    let candidate = &cleaned[start..];
    let v: Vec<ContradictionOut> =
        serde_json::from_str(candidate).map_err(|e| anyhow!("not JSON: {e}"))?;
    Ok(v)
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

fn extract_tags(content: &str) -> Vec<String> {
    let trimmed = content.trim_start();
    let Some(after) = trimmed.strip_prefix("---") else { return Vec::new() };
    let Some(end) = after.find("\n---") else { return Vec::new() };
    let fm = &after[..end];
    let mut out = Vec::new();
    for line in fm.lines() {
        let line = line.trim_start();
        if let Some(rest) = line.strip_prefix("tags:") {
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
