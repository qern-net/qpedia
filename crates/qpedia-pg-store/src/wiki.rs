//! Wiki pages: denormalized search index. Canonical content still lives
//! in the per-tenant git repo; rows here exist so the hybrid query can
//! join BM25 (tsvector) with vector similarity (pgvector) in one SQL
//! statement. See SPEC-v2.md §3.

use crate::PgStore;
use anyhow::{Context, Result};
use chrono::Utc;
use pgvector::Vector;
use qpedia_core::tenant::Tenant;
use sqlx::Row;

#[derive(Debug, Clone)]
pub struct WikiPageUpsert {
    pub page_id: String,
    pub path: String,
    pub kind: String,
    pub title: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    pub snippet: String,
    pub score: f32,
}

impl PgStore {
    /// Insert-or-replace a wiki page row.
    pub async fn upsert_wiki_page(
        &self,
        tenant: &Tenant,
        page: &WikiPageUpsert,
        embedding: Vec<f32>,
    ) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        let vec = Vector::from(embedding);
        // Column names must match migrations/0001_init.sql: the table has no
        // `page_id` (identity is (tenant_id, path)) and the array column is
        // `source_slugs`. `page.source_ids` carries slugs (SourceId == slug).
        sqlx::query(
            "INSERT INTO wiki_pages \
             (tenant_id, path, kind, title, content, tags, source_slugs, embedding, updated_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8, now()) \
             ON CONFLICT (tenant_id, path) DO UPDATE SET \
               kind         = EXCLUDED.kind, \
               title        = EXCLUDED.title, \
               content      = EXCLUDED.content, \
               tags         = EXCLUDED.tags, \
               source_slugs = EXCLUDED.source_slugs, \
               embedding    = EXCLUDED.embedding, \
               updated_at   = now()",
        )
        .bind(tenant.as_str())
        .bind(&page.path)
        .bind(&page.kind)
        .bind(&page.title)
        .bind(&page.content)
        .bind(&page.tags)
        .bind(&page.source_ids)
        .bind(vec)
        .execute(&mut *tx)
        .await
        .context("upsert wiki_page")?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_wiki_page(&self, tenant: &Tenant, path: &str) -> Result<()> {
        let mut tx = self.begin_for(tenant).await?;
        sqlx::query("DELETE FROM wiki_pages WHERE path = $1")
            .bind(path)
            .execute(&mut *tx)
            .await
            .context("delete wiki_page")?;
        tx.commit().await?;
        Ok(())
    }

    /// Hybrid search via **Reciprocal Rank Fusion** (RRF).
    ///
    /// Runs two retrievers independently — dense (pgvector cosine) and
    /// lexical (tsvector / `ts_rank_cd`) — and fuses their *ranked lists*
    /// by position, not by raw score:
    ///
    /// ```text
    /// rrf(d) = Σ_retrievers 1 / (k + rank_r(d))
    /// ```
    ///
    /// This sidesteps the scale mismatch that made the old weighted sum
    /// effectively vector-only: cosine is bounded `[0,1]` while
    /// `ts_rank_cd` is unbounded but tiny (~0.0x), so `alpha*cosine +
    /// (1-alpha)*ts_rank_cd` drowned the lexical signal. RRF rewards
    /// documents *both* retrievers rank highly, so exact-token matches
    /// (error codes, versions, flag/slug names) can win where embeddings
    /// alone collapse the distinction. See the wiki "Retrieval" page.
    ///
    /// `k` (the rank-decay constant) comes from `QPEDIA_RRF_K` (default
    /// 60). Lower (≈20–30) sharpens precision; higher (≈80–100) favors
    /// recall/consensus. A larger candidate pool than `limit` is pulled
    /// from each retriever before fusion so the lists actually overlap.
    pub async fn hybrid_search(
        &self,
        tenant: &Tenant,
        query: &str,
        vector: Vec<f32>,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        let mut tx = self.begin_for(tenant).await?;
        let q_vec = Vector::from(vector);
        let rrf_k = rrf_k_from_env();
        // Pool: each retriever returns more candidates than the final limit
        // so the two ranked lists overlap enough for fusion to mean
        // something. Clamped so a huge `limit` can't explode the scan.
        let pool: i64 = (limit * 4).clamp(50, 200);
        // websearch_to_tsquery is forgiving of free-form input and returns
        // an empty tsquery for garbage — then `tsv @@ q` matches nothing and
        // RRF degrades cleanly to vector-only, which is the right behavior.
        let rows = sqlx::query(
            "WITH vec AS ( \
                 SELECT path, row_number() OVER (ORDER BY embedding <=> $1::vector) AS rank \
                 FROM wiki_pages \
                 WHERE embedding IS NOT NULL \
                 ORDER BY embedding <=> $1::vector \
                 LIMIT $2 \
             ), \
             kw AS ( \
                 SELECT path, row_number() OVER ( \
                            ORDER BY ts_rank_cd(tsv, websearch_to_tsquery('english', $3)) DESC \
                        ) AS rank \
                 FROM wiki_pages \
                 WHERE tsv @@ websearch_to_tsquery('english', $3) \
                 ORDER BY ts_rank_cd(tsv, websearch_to_tsquery('english', $3)) DESC \
                 LIMIT $2 \
             ), \
             fused AS ( \
                 SELECT path, SUM(1.0 / ($4::float + rank)) AS rrf \
                 FROM (SELECT path, rank FROM vec \
                       UNION ALL \
                       SELECT path, rank FROM kw) u \
                 GROUP BY path \
             ) \
             SELECT p.path, \
                    coalesce(p.title, p.path) AS title, \
                    left(p.content, 200) AS snippet, \
                    f.rrf AS score \
             FROM fused f \
             JOIN wiki_pages p ON p.path = f.path \
             ORDER BY f.rrf DESC \
             LIMIT $5",
        )
        .bind(q_vec)
        .bind(pool)
        .bind(query)
        .bind(rrf_k as f64)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .context("hybrid search (rrf)")?;
        tx.commit().await.ok();

        Ok(rows
            .into_iter()
            .map(|r| SearchHit {
                path: r.get("path"),
                title: r.get("title"),
                snippet: r.get("snippet"),
                score: r.get::<f64, _>("score") as f32,
            })
            .collect())
    }

    /// Near-duplicate pairs above `min_similarity`. Used by the lint pass.
    pub async fn near_duplicates(
        &self,
        tenant: &Tenant,
        min_similarity: f32,
        limit: i64,
    ) -> Result<Vec<(String, String, f32)>> {
        let mut tx = self.begin_for(tenant).await?;
        let rows = sqlx::query(
            "SELECT a.path AS pa, b.path AS pb, \
                    (1.0 - (a.embedding <=> b.embedding))::float AS sim \
             FROM wiki_pages a \
             JOIN wiki_pages b ON a.path < b.path \
             WHERE a.embedding IS NOT NULL \
               AND b.embedding IS NOT NULL \
               AND (1.0 - (a.embedding <=> b.embedding)) >= $1 \
             ORDER BY sim DESC \
             LIMIT $2",
        )
        .bind(min_similarity as f64)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .context("near_duplicates")?;
        tx.commit().await.ok();

        Ok(rows
            .into_iter()
            .map(|r| {
                let s: f64 = r.get("sim");
                (r.get("pa"), r.get("pb"), s as f32)
            })
            .collect())
    }
}

// Silence unused-import warning until callers wire this up.
#[allow(dead_code)]
fn _ts() -> chrono::DateTime<Utc> { Utc::now() }

/// RRF rank-decay constant from `QPEDIA_RRF_K` (default 60, per Cormack et
/// al. 2009). Clamped to a sane range so a typo can't disable fusion.
/// Lower → top ranks dominate (precision); higher → flatter, rewards
/// cross-retriever consensus (recall).
fn rrf_k_from_env() -> f64 {
    std::env::var("QPEDIA_RRF_K")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .map(|k| k.clamp(1.0, 1000.0))
        .unwrap_or(60.0)
}
