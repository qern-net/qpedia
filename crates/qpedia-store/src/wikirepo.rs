//! Git-backed wiki repository. See DESIGN.md §2.1, §2.3.
//!
//! v0 implementation shells out to `git` CLI for commit creation. This is
//! deliberate: gix's commit API is significantly more involved and the
//! per-commit cost (~50ms subprocess) is irrelevant at our throughput.
//! A future swap to `gix` keeps this module's surface unchanged.

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use qpedia_core::{tenant::Tenant, wiki::{DiffBundle, DiffOp}};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// QPEDIA.md template — the schema/style guide loaded into every agent call.
const QPEDIA_MD: &str = r#"# QPEDIA.md — Wiki Style & Operations Guide

This file governs how the LLM (qpedia-bot) writes and maintains this wiki.

## Page kinds
- **concepts/**: ideas, processes, frameworks. Title = noun phrase.
- **entities/**: people, companies, products, systems. Title = proper noun.
- **comparisons/**: "X vs Y" syntheses. Created when 2+ entities or concepts overlap.
- **summaries/**: one page per raw source. Created on ingest.

## Writing rules
- Every page is standalone-readable. Never write "see above" — link instead.
- Every claim has a source citation: `[^src:<source_id>]`.
- Prefer bullet lists over paragraphs for fact-dense content.
- Page length target: 300–2000 words. Split if longer.
- New terms introduced → create a concept page → link.

## Linking
- Use `[[path/to/page.md]]` for internal links.
- Every page should have ≥2 outbound links (unless it's a leaf summary).
- When adding a fact that contradicts an existing page, add a `> ⚠ contradicts [[...]]` callout.

## Ingest protocol
1. Read the source.
2. search_wiki for related pages.
3. Create one summary page at `summaries/<source_id>.md`.
4. Update or create concept/entity pages referenced by the source.
5. Update `index.md` (alpha-sorted catalog).
6. Append one line to `log.md`.

## Forbidden
- Never rewrite content you can't derive from the sources.
- Never delete pages with external inbound links — mark deprecated instead.
- Never modify QPEDIA.md itself.
"#;

const INDEX_MD: &str = r#"# Index

LLM-maintained catalog of every page in this wiki, alpha-sorted by category.

## Summaries
<!-- one entry per ingested source -->

## Concepts
<!-- ideas, processes, frameworks -->

## Entities
<!-- people, companies, products, systems -->

## Comparisons
<!-- "X vs Y" syntheses -->
"#;

const LOG_MD: &str = r#"# Log

Append-only chronological history of ingests, queries, and lint passes.

"#;

#[derive(Clone)]
pub struct WikiRepo {
    root: PathBuf,
    author_name: String,
    author_email: String,
}

impl WikiRepo {
    /// Open the repo at `root`, initializing + seeding it on first use.
    pub async fn open_or_init(
        root: impl AsRef<Path>,
        author_name: impl Into<String>,
        author_email: impl Into<String>,
    ) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root).await.context("create wiki dir")?;

        let author_name = author_name.into();
        let author_email = author_email.into();

        // Sanity: git on PATH?
        if Command::new("git").arg("--version").output().await.is_err() {
            return Err(anyhow!("`git` not found on PATH — install git or use the qpedia container image"));
        }

        let repo = Self { root: root.clone(), author_name, author_email };

        if !root.join(".git").exists() {
            info!(path = %root.display(), "initializing wiki repo");
            repo.git(&["init", "-q", "-b", "main"]).await?;
            repo.git(&["config", "user.name", &repo.author_name]).await?;
            repo.git(&["config", "user.email", &repo.author_email]).await?;
            // Scoped to this repo (no global config writes).

            // Seed required files.
            tokio::fs::write(root.join("QPEDIA.md"), QPEDIA_MD).await?;
            tokio::fs::write(root.join("index.md"), INDEX_MD).await?;
            tokio::fs::write(root.join("log.md"), LOG_MD).await?;
            for d in &["concepts", "entities", "comparisons", "summaries", "_meta"] {
                let dir = root.join(d);
                tokio::fs::create_dir_all(&dir).await?;
                tokio::fs::write(dir.join(".gitkeep"), "").await?;
            }
            repo.git(&["add", "-A"]).await?;
            repo.git(&["commit", "-q", "-m", "qpedia: initial wiki seed"]).await?;
            info!("wiki repo seeded");
        }

        Ok(repo)
    }

    pub fn root(&self) -> &Path { &self.root }

    pub async fn read_page(&self, path: &str) -> Result<Option<String>> {
        let safe = self.safe_join(path)?;
        match tokio::fs::read_to_string(&safe).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn list_pages(&self, prefix: &str) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let scope = if prefix.is_empty() {
            self.root.clone()
        } else {
            self.safe_join(prefix)?
        };
        if !scope.exists() {
            return Ok(out);
        }
        walk_md(&self.root, &scope, &mut out)?;
        out.sort();
        Ok(out)
    }

    /// Markdown paths touched by the HEAD commit (works for the root commit
    /// too via `git show --name-only`). Returns paths relative to the repo
    /// root, with forward slashes.
    pub async fn changed_in_head(&self) -> Result<Vec<String>> {
        let out = self.git(&["show", "--pretty=format:", "--name-only", "HEAD"]).await?;
        Ok(out
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .filter(|l| l.ends_with(".md"))
            .map(|l| l.replace('\\', "/"))
            .collect())
    }

    /// Filesystem-based text search over wiki pages. Returns matches with
    /// `(path, title, snippet)`. Title is the first H1, falling back to the
    /// frontmatter `title` or the file path. Snippet is ~160 chars around
    /// the first hit. Stand-in until Weaviate hybrid search is wired.
    pub async fn search_text(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let pages = self.list_pages("").await?;
        let mut hits = Vec::new();
        for path in pages {
            if hits.len() >= limit { break; }
            let Some(content) = self.read_page(&path).await? else { continue };
            let lower = content.to_lowercase();
            let Some(pos) = lower.find(&q) else { continue };

            let title = extract_title(&content).unwrap_or_else(|| path.clone());
            let start = pos.saturating_sub(80);
            let end = (pos + q.len() + 80).min(content.len());
            let mut snippet = content[start..end].replace('\n', " ");
            snippet = snippet.chars().take(200).collect();
            hits.push(SearchHit { path, title, snippet });
        }
        Ok(hits)
    }

    /// Apply a DiffBundle as a single git commit. Returns the commit sha.
    pub async fn commit_bundle(&self, bundle: &DiffBundle) -> Result<String> {
        let mut total_bytes = 0usize;

        // Validate up-front: paths safe, sizes within caps.
        for op in &bundle.operations {
            match op {
                DiffOp::Create { path, content, .. } |
                DiffOp::Patch  { path, new_content: content, .. } => {
                    if content.len() > 50 * 1024 {
                        return Err(anyhow!("page {path} exceeds 50KB cap ({} bytes)", content.len()));
                    }
                    self.safe_join(path)?;  // validates path
                    total_bytes += content.len();
                }
                DiffOp::Delete { path, .. } => { self.safe_join(path)?; }
                DiffOp::Link   { from, to, .. } => {
                    self.safe_join(from)?;
                    self.safe_join(to)?;
                }
            }
        }
        if total_bytes > 500 * 1024 {
            return Err(anyhow!("bundle exceeds 500KB cap ({total_bytes} bytes)"));
        }

        // Apply ops to working tree.
        for op in &bundle.operations {
            match op {
                DiffOp::Create { path, content, .. } => {
                    let full = self.safe_join(path)?;
                    if let Some(parent) = full.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    tokio::fs::write(&full, content).await
                        .with_context(|| format!("write {path}"))?;
                }
                DiffOp::Patch { path, new_content, .. } => {
                    let full = self.safe_join(path)?;
                    tokio::fs::write(&full, new_content).await
                        .with_context(|| format!("patch {path}"))?;
                }
                DiffOp::Delete { path, .. } => {
                    let full = self.safe_join(path)?;
                    if full.exists() {
                        tokio::fs::remove_file(&full).await
                            .with_context(|| format!("delete {path}"))?;
                    } else {
                        warn!(%path, "delete target missing; skipping");
                    }
                }
                DiffOp::Link { .. } => {
                    // Edges are stored in frontmatter (`links_out`); no separate file.
                    // The page Patch op carrying the new frontmatter does the work.
                }
            }
        }

        // Stage everything (covers creates, patches, deletes).
        self.git(&["add", "-A"]).await?;

        // Anything to commit?
        let status = self.git(&["status", "--porcelain"]).await?;
        if status.trim().is_empty() {
            return Err(anyhow!("commit_bundle: no changes to commit"));
        }

        let msg = if bundle.summary.is_empty() {
            format!("ingest({})", bundle.ingest_id)
        } else {
            format!("ingest({}): {}", bundle.ingest_id, bundle.summary)
        };
        self.git(&["commit", "-q", "-m", &msg]).await?;
        let sha = self.git(&["rev-parse", "HEAD"]).await?.trim().to_string();
        info!(sha = %sha, ops = bundle.operations.len(), "wiki commit landed");
        Ok(sha)
    }

    // ---------- internals ----------

    fn safe_join(&self, rel: &str) -> Result<PathBuf> {
        let rel = rel.trim_start_matches('/');
        if rel.is_empty() {
            return Err(anyhow!("empty path"));
        }
        if rel.starts_with(".git") || rel.contains("/.git/") {
            return Err(anyhow!("path under .git/ not allowed: {rel}"));
        }
        let p = Path::new(rel);
        for comp in p.components() {
            use std::path::Component::*;
            match comp {
                Normal(_) => {}
                CurDir => {}
                ParentDir => return Err(anyhow!("path traversal not allowed: {rel}")),
                RootDir | Prefix(_) => return Err(anyhow!("absolute path not allowed: {rel}")),
            }
        }
        Ok(self.root.join(p))
    }

    async fn git(&self, args: &[&str]) -> Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .await
            .with_context(|| format!("spawn git {args:?}"))?;
        if !out.status.success() {
            return Err(anyhow!(
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

fn walk_md(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" { continue; }
        if path.is_dir() {
            walk_md(root, &path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

/// Convenience helper for callers that want a current ISO-8601 timestamp string.
pub fn now_iso() -> String {
    Utc::now().to_rfc3339()
}

/// Manages one git repo per tenant. Lazy-initializes on first lookup so
/// every tenant — including ones that materialize on a user's first
/// sign-in — gets a fresh wiki without manual setup.
#[derive(Clone)]
pub struct WikiRepoStore {
    root: PathBuf,
    author_name: String,
    author_email: String,
    cache: Arc<Mutex<HashMap<String, WikiRepo>>>,
}

impl WikiRepoStore {
    pub fn new(
        root: impl AsRef<Path>,
        author_name: impl Into<String>,
        author_email: impl Into<String>,
    ) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            author_name: author_name.into(),
            author_email: author_email.into(),
            cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Open or initialize the wiki repo for `tenant`. Repos live under
    /// `<root>/<tenant>/`, one git history per tenant. WikiRepo is
    /// cheap-Clone (PathBuf + a couple of strings), so callers get
    /// their own copy without needing Arc.
    pub async fn get(&self, tenant: &Tenant) -> Result<WikiRepo> {
        {
            let guard = self.cache.lock().await;
            if let Some(r) = guard.get(tenant.as_str()) {
                return Ok(r.clone());
            }
        }
        let path = self.root.join(tenant.as_str());
        let repo = WikiRepo::open_or_init(&path, &self.author_name, &self.author_email).await?;
        let mut guard = self.cache.lock().await;
        guard
            .entry(tenant.as_str().to_string())
            .or_insert_with(|| repo.clone());
        Ok(repo)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    pub snippet: String,
}

fn extract_title(content: &str) -> Option<String> {
    // Prefer first markdown H1 outside the frontmatter block.
    let mut in_fm = false;
    let mut seen_fm_open = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed == "---" {
            if !seen_fm_open { in_fm = true; seen_fm_open = true; continue; }
            if in_fm { in_fm = false; continue; }
        }
        if in_fm { continue; }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
    }
    // Fallback: frontmatter title field.
    let mut in_fm2 = false;
    let mut opens = 0;
    for line in content.lines() {
        let t = line.trim();
        if t == "---" { opens += 1; in_fm2 = opens == 1; continue; }
        if !in_fm2 { continue; }
        if let Some(rest) = t.strip_prefix("title:") {
            return Some(rest.trim().trim_matches('"').trim_matches('\'').to_string());
        }
    }
    None
}
