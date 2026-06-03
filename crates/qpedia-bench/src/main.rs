//! Retrieval benchmark runner.
//!
//! Ingests a fixed corpus of wiki pages (with deliberately planted
//! near-duplicate distractors) into a throwaway `bench` tenant, runs a
//! labeled query set through the real `hybrid_search` ranking path, and
//! reports IR metrics (Recall@K, MRR, nDCG@10, Exact@1) overall and per
//! query category (semantic / exact / hybrid).
//!
//! The whole point is to MEASURE ranking changes (RRF constant, reranker,
//! tsvector config) rather than guess. See the wiki "Retrieval Benchmark".
//!
//! Gated on `QPEDIA_DB_URL` (a Postgres 17 + pgvector instance) and an
//! available embedder. Exits non-zero if a stored `baseline.json` exists
//! and any headline metric regresses beyond a small tolerance.
//!
//! Usage:
//!   QPEDIA_DB_URL=postgres://… cargo run -p qpedia-bench -- run
//!   QPEDIA_DB_URL=postgres://… cargo run -p qpedia-bench -- run --update-baseline

use anyhow::{Context, Result};
use clap::Parser;
use qpedia_core::tenant::Tenant;
use qpedia_embed::embedder_from_env;
use qpedia_pg_store::{PgStore, WikiPageUpsert};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Final-list cutoff used for Recall@K and the report.
const K: i64 = 10;
/// A regression beyond this absolute drop in a headline metric fails CI.
const REGRESSION_TOLERANCE: f64 = 0.02;

#[derive(Parser)]
#[command(name = "qpedia-bench", about = "Retrieval benchmark for Qpedia ranking")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Subcommand)]
enum Cmd {
    /// Ingest the corpus, run the queries, print + write the report.
    Run {
        /// Directory holding corpus/ and queries.jsonl (defaults to the
        /// crate's bundled `bench/`).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Overwrite baseline.json with this run's scores.
        #[arg(long)]
        update_baseline: bool,
    },
}

/// One labeled query. `qrels` maps a relevant page path to a graded
/// relevance (2 = the exact answer, 1 = acceptable supporting context).
#[derive(Debug, Deserialize)]
struct Query {
    id: String,
    category: String,
    query: String,
    qrels: BTreeMap<String, u8>,
}

#[derive(Debug, Default, Clone, Serialize)]
struct Metrics {
    #[serde(rename = "recall@10")]
    recall_at_k: f64,
    mrr: f64,
    #[serde(rename = "ndcg@10")]
    ndcg_at_10: f64,
    #[serde(rename = "exact@1")]
    exact_at_1: f64,
    #[serde(skip_serializing_if = "is_zero")]
    n: usize,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Serialize)]
struct Report {
    generated_at: String,
    config: Config,
    overall: Metrics,
    by_category: BTreeMap<String, Metrics>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    regressions: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Config {
    rrf_k: String,
    embed_model: String,
}

#[derive(Debug, Deserialize)]
struct Baseline {
    overall: BaselineMetrics,
}

#[derive(Debug, Deserialize)]
struct BaselineMetrics {
    #[serde(rename = "recall@10")]
    recall_at_k: f64,
    mrr: f64,
    #[serde(rename = "ndcg@10")]
    ndcg_at_10: f64,
    #[serde(rename = "exact@1")]
    exact_at_1: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "qpedia_bench=info,warn".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run {
            dir,
            update_baseline,
        } => run(dir, update_baseline).await,
    }
}

async fn run(dir: Option<PathBuf>, update_baseline: bool) -> Result<()> {
    let bench_dir = dir.unwrap_or_else(default_bench_dir);
    let corpus_dir = bench_dir.join("corpus");
    let queries_path = bench_dir.join("queries.jsonl");
    let baseline_path = bench_dir.join("baseline.json");

    let url = std::env::var("QPEDIA_DB_URL")
        .context("QPEDIA_DB_URL must be set to a Postgres 17 + pgvector instance")?;

    // The embedder must produce real vectors for the benchmark to mean
    // anything; a failure here is fatal (unlike the app, which degrades).
    let embedder = embedder_from_env("./data/models");
    println!("embedder: {} ({} dims)", embedder.name(), embedder.dimensions());

    let db = PgStore::connect(&url)
        .await
        .context("connect + run migrations")?;

    // Fresh, unique tenant per run so repeated/parallel runs never collide
    // and we always measure against exactly the bundled corpus.
    let tenant = unique_tenant();
    db.upsert_tenant(&tenant, "Retrieval Benchmark", None)
        .await
        .context("create bench tenant")?;
    println!("bench tenant: {}", tenant.as_str());

    // ── Ingest corpus ────────────────────────────────────────────────
    let pages = load_corpus(&corpus_dir).context("load corpus")?;
    println!("ingesting {} corpus pages…", pages.len());
    for page in &pages {
        let emb = embedder
            .embed(&[&page.content])
            .await
            .context("embed page")?
            .into_iter()
            .next()
            .unwrap_or_default();
        db.upsert_wiki_page(&tenant, page, emb)
            .await
            .with_context(|| format!("upsert {}", page.path))?;
    }

    // ── Run queries ──────────────────────────────────────────────────
    let queries = load_queries(&queries_path).context("load queries")?;
    println!("running {} queries…\n", queries.len());

    // Accumulate per-category and overall.
    let mut overall = Acc::default();
    let mut per_cat: BTreeMap<String, Acc> = BTreeMap::new();

    for q in &queries {
        let qv = embedder
            .embed(&[&q.query])
            .await?
            .into_iter()
            .next()
            .unwrap_or_default();
        let hits = db.hybrid_search(&tenant, &q.query, qv, K).await?;
        let ranked: Vec<String> = hits.into_iter().map(|h| h.path).collect();
        let s = score_query(q, &ranked);
        println!(
            "  [{}] {:<8} exact@1={} mrr={:.3} ndcg={:.3}  {}",
            q.id,
            q.category,
            if s.exact_at_1 { "1" } else { "0" },
            s.mrr,
            s.ndcg,
            truncate(&q.query, 48)
        );
        overall.push(&s);
        per_cat.entry(q.category.clone()).or_default().push(&s);
    }

    // ── Cleanup (best-effort; a fresh tenant is harmless if it lingers) ─
    for page in &pages {
        let _ = db.delete_wiki_page(&tenant, &page.path).await;
    }

    let report = Report {
        generated_at: chrono::Utc::now().to_rfc3339(),
        config: Config {
            rrf_k: std::env::var("QPEDIA_RRF_K").unwrap_or_else(|_| "60".into()),
            embed_model: embedder.name().to_string(),
        },
        overall: overall.finish(),
        by_category: per_cat
            .into_iter()
            .map(|(k, v)| (k, v.finish()))
            .collect(),
        regressions: Vec::new(),
    };

    print_report(&report);

    // ── Baseline comparison / update ─────────────────────────────────
    let mut report = report;
    if update_baseline {
        std::fs::write(&baseline_path, serde_json::to_string_pretty(&report.overall)?)
            .context("write baseline.json")?;
        println!("\nbaseline.json updated.");
    } else if baseline_path.exists() {
        report.regressions = check_regressions(&baseline_path, &report.overall)?;
    } else {
        println!("\n(no baseline.json — run with --update-baseline to set one)");
    }

    // Always write the latest report next to the baseline for inspection.
    std::fs::write(
        bench_dir.join("last-report.json"),
        serde_json::to_string_pretty(&report)?,
    )
    .ok();

    if !report.regressions.is_empty() {
        eprintln!("\nREGRESSION:");
        for r in &report.regressions {
            eprintln!("  - {r}");
        }
        std::process::exit(1);
    }

    println!("\nOK");
    Ok(())
}

/// Per-query computed scores.
struct QScore {
    recall: bool,
    mrr: f64,
    ndcg: f64,
    exact_at_1: bool,
}

/// Compute Recall@K, reciprocal rank, nDCG@K, and Exact@1 for one query
/// against its qrels.
fn score_query(q: &Query, ranked: &[String]) -> QScore {
    // First relevant rank (1-indexed) for MRR + recall.
    let first_rel = ranked
        .iter()
        .position(|p| q.qrels.contains_key(p))
        .map(|i| i + 1);

    let recall = first_rel.is_some();
    let mrr = first_rel.map(|r| 1.0 / r as f64).unwrap_or(0.0);

    // Exact@1: rank-1 page must be a grade-2 (the exact answer).
    let exact_at_1 = ranked
        .first()
        .and_then(|p| q.qrels.get(p))
        .map(|&g| g == 2)
        .unwrap_or(false);

    // nDCG@K with graded relevance. DCG = Σ rel_i / log2(i+1).
    let mut dcg = 0.0;
    for (i, path) in ranked.iter().take(K as usize).enumerate() {
        if let Some(&g) = q.qrels.get(path) {
            dcg += g as f64 / ((i as f64 + 2.0).log2());
        }
    }
    // Ideal DCG: qrels sorted by grade desc.
    let mut ideal: Vec<u8> = q.qrels.values().copied().collect();
    ideal.sort_unstable_by(|a, b| b.cmp(a));
    let mut idcg = 0.0;
    for (i, &g) in ideal.iter().take(K as usize).enumerate() {
        idcg += g as f64 / ((i as f64 + 2.0).log2());
    }
    let ndcg = if idcg > 0.0 { dcg / idcg } else { 0.0 };

    QScore {
        recall,
        mrr,
        ndcg,
        exact_at_1,
    }
}

#[derive(Default)]
struct Acc {
    n: usize,
    recall: f64,
    mrr: f64,
    ndcg: f64,
    exact: f64,
}

impl Acc {
    fn push(&mut self, s: &QScore) {
        self.n += 1;
        self.recall += s.recall as u8 as f64;
        self.mrr += s.mrr;
        self.ndcg += s.ndcg;
        self.exact += s.exact_at_1 as u8 as f64;
    }
    fn finish(self) -> Metrics {
        let n = self.n.max(1) as f64;
        Metrics {
            recall_at_k: self.recall / n,
            mrr: self.mrr / n,
            ndcg_at_10: self.ndcg / n,
            exact_at_1: self.exact / n,
            n: self.n,
        }
    }
}

fn check_regressions(baseline_path: &Path, current: &Metrics) -> Result<Vec<String>> {
    let raw = std::fs::read_to_string(baseline_path).context("read baseline.json")?;
    let base: Baseline = serde_json::from_str(&format!("{{\"overall\":{raw}}}"))
        .or_else(|_| serde_json::from_str(&raw))
        .context("parse baseline.json")?;
    let b = base.overall;
    let mut regressions = Vec::new();
    let checks = [
        ("recall@10", b.recall_at_k, current.recall_at_k),
        ("mrr", b.mrr, current.mrr),
        ("ndcg@10", b.ndcg_at_10, current.ndcg_at_10),
        ("exact@1", b.exact_at_1, current.exact_at_1),
    ];
    for (name, base_v, cur_v) in checks {
        if cur_v + REGRESSION_TOLERANCE < base_v {
            regressions.push(format!(
                "{name}: {cur_v:.3} < baseline {base_v:.3} (tolerance {REGRESSION_TOLERANCE})"
            ));
        }
    }
    Ok(regressions)
}

fn print_report(r: &Report) {
    println!("\n── Results ──────────────────────────────────");
    println!("config: rrf_k={} embed={}", r.config.rrf_k, r.config.embed_model);
    let o = &r.overall;
    println!(
        "overall (n={}): recall@10={:.3} mrr={:.3} ndcg@10={:.3} exact@1={:.3}",
        o.n, o.recall_at_k, o.mrr, o.ndcg_at_10, o.exact_at_1
    );
    for (cat, m) in &r.by_category {
        println!(
            "  {:<9} (n={}): recall@10={:.3} mrr={:.3} ndcg@10={:.3} exact@1={:.3}",
            cat, m.n, m.recall_at_k, m.mrr, m.ndcg_at_10, m.exact_at_1
        );
    }
}

// ── corpus / query loading ───────────────────────────────────────────

/// A corpus page is a Markdown file with optional YAML frontmatter
/// (`title:`, `kind:`, `tags:`). The path key is the file path relative
/// to corpus/, matching the qrels in queries.jsonl.
fn load_corpus(dir: &Path) -> Result<Vec<WikiPageUpsert>> {
    let mut pages = Vec::new();
    collect_md(dir, dir, &mut pages)?;
    anyhow::ensure!(!pages.is_empty(), "no corpus pages found under {dir:?}");
    Ok(pages)
}

fn collect_md(root: &Path, dir: &Path, out: &mut Vec<WikiPageUpsert>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {dir:?}"))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_md(root, &path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let raw = std::fs::read_to_string(&path)?;
            let (fm, body) = split_frontmatter(&raw);
            out.push(WikiPageUpsert {
                page_id: rel.clone(),
                path: rel.clone(),
                kind: fm.get("kind").cloned().unwrap_or_else(|| "concept".into()),
                title: fm.get("title").cloned().unwrap_or_else(|| rel.clone()),
                content: body,
                tags: fm
                    .get("tags")
                    .map(|t| {
                        t.trim_matches(|c| c == '[' || c == ']')
                            .split(',')
                            .map(|s| s.trim().trim_matches('"').to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                source_ids: vec![],
            });
        }
    }
    Ok(())
}

/// Minimal YAML-frontmatter split: a leading `---\n…\n---` block parsed as
/// flat `key: value` pairs. Enough for the benchmark corpus.
fn split_frontmatter(raw: &str) -> (BTreeMap<String, String>, String) {
    let mut map = BTreeMap::new();
    let trimmed = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            let fm = &rest[..end];
            for line in fm.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    map.insert(k.trim().to_string(), v.trim().to_string());
                }
            }
            let body = &rest[end + 4..];
            return (map, body.trim_start_matches('\n').to_string());
        }
    }
    (map, raw.to_string())
}

fn load_queries(path: &Path) -> Result<Vec<Query>> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {path:?}"))?;
    let mut queries = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let q: Query = serde_json::from_str(line)
            .with_context(|| format!("parse query line {}", i + 1))?;
        queries.push(q);
    }
    anyhow::ensure!(!queries.is_empty(), "no queries in {path:?}");
    Ok(queries)
}

fn default_bench_dir() -> PathBuf {
    // crates/qpedia-bench/bench/
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bench")
}

fn unique_tenant() -> Tenant {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    Tenant::new(format!("bench-{nanos:032}"))
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}
