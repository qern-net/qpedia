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

    /// Hybrid search: combines vector cosine similarity with `ts_rank_cd`
    /// keyword ranking. `alpha` weights vector vs. BM25 (0..=1, default
    /// 0.7 — matches DESIGN.md §2.4).
    pub async fn hybrid_search(
        &self,
        tenant: &Tenant,
        query: &str,
        vector: Vec<f32>,
        alpha: f32,
        limit: i64,
    ) -> Result<Vec<SearchHit>> {
        let mut tx = self.begin_for(tenant).await?;
        let q_vec = Vector::from(vector);
        // websearch_to_tsquery is forgiving of free-form user queries;
        // returns empty tsquery for purely garbage input rather than
        // erroring.
        let rows = sqlx::query(
            "SELECT \
                 p.path, \
                 coalesce(p.title, p.path) AS title, \
                 left(p.content, 200) AS snippet, \
                 ($3::float * (1.0 - (p.embedding <=> $2::vector)) \
                  + (1.0 - $3::float) * ts_rank_cd(p.tsv, websearch_to_tsquery('english', $1))) \
                 AS score \
             FROM wiki_pages p \
             WHERE p.embedding IS NOT NULL \
             ORDER BY score DESC \
             LIMIT $4",
        )
        .bind(query)
        .bind(q_vec)
        .bind(alpha)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .context("hybrid search")?;
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
