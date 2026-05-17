//! Wikipedia-style slug generation and uniqueness probing.
//!
//! [`slugify`] turns any user-supplied string into a URL-safe identifier
//! (lowercase, ASCII alphanumeric, dashes between words). [`unique_source_slug`]
//! and [`unique_wiki_path`] consult Postgres for collisions and append
//! `-2`, `-3`, ... until they find a free identifier. The dedup loop runs
//! inside the caller's tenant-scoped transaction, so it benefits from RLS
//! and concurrent inserts that race the loop will be caught by the
//! `UNIQUE (tenant_id, slug)` constraint.

use crate::PgStore;
use anyhow::{anyhow, Result};
use qpedia_core::tenant::Tenant;

/// Maximum slug suffix attempts. After this we give up — almost certainly
/// a sign of a bug rather than legitimate contention.
const MAX_ATTEMPTS: u32 = 1000;

/// Turn a human string into a slug: lowercase, ASCII alphanumeric, dashes
/// between word runs. Always returns a non-empty string (`"untitled"` if
/// every character was stripped). Caps at 80 chars so URLs stay readable.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(80));
    let mut last_dash = true;
    for ch in name.chars() {
        if out.len() >= 80 { break; }
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') { out.pop(); }
    if out.is_empty() {
        return "untitled".into();
    }
    out
}

/// Find a free slug for a source in `(tenant, folder_path)`. Note: the
/// uniqueness constraint is per-tenant (not per-folder) so a slug
/// collision in another folder still produces a `-N` suffix.
pub async fn unique_source_slug(
    store: &PgStore,
    tenant: &Tenant,
    desired: &str,
) -> Result<String> {
    let base = slugify(desired);
    probe_unique(store, tenant, &base, |tx, candidate| {
        Box::pin(async move {
            let row: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM sources WHERE slug = $1 LIMIT 1",
            )
            .bind(candidate)
            .fetch_optional(&mut **tx)
            .await?;
            Ok(row.is_none())
        })
    })
    .await
}

/// Find a free wiki page path. `kind` is the top-level directory
/// ("concepts", "entities", etc.) and `name` is the human-friendly title.
/// Returns e.g. `"concepts/revenue-forecasting.md"`.
pub async fn unique_wiki_path(
    store: &PgStore,
    tenant: &Tenant,
    kind: &str,
    name: &str,
) -> Result<String> {
    let kind = slugify(kind);
    let base = slugify(name);
    let base_path = format!("{kind}/{base}.md");
    probe_unique(store, tenant, &base_path, |tx, candidate| {
        Box::pin(async move {
            let row: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM wiki_pages WHERE path = $1 LIMIT 1",
            )
            .bind(candidate)
            .fetch_optional(&mut **tx)
            .await?;
            Ok(row.is_none())
        })
    })
    .await
}

/// Find a free connector name for `tenant`. Similar to source slugs but
/// scoped to the connectors table.
pub async fn unique_connector_name(
    store: &PgStore,
    tenant: &Tenant,
    desired: &str,
) -> Result<String> {
    let base = slugify(desired);
    probe_unique(store, tenant, &base, |tx, candidate| {
        Box::pin(async move {
            let row: Option<i64> = sqlx::query_scalar(
                "SELECT id FROM connectors WHERE name = $1 LIMIT 1",
            )
            .bind(candidate)
            .fetch_optional(&mut **tx)
            .await?;
            Ok(row.is_none())
        })
    })
    .await
}

/// Slugify a folder path: `/Finance/Q4 Reports/` → `/finance/q4-reports`.
/// Preserves the leading slash and the slash-separated structure.
pub fn slugify_folder(path: &str) -> String {
    if path.trim().is_empty() {
        return "/".into();
    }
    let parts: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(slugify)
        .collect();
    if parts.is_empty() {
        return "/".into();
    }
    format!("/{}", parts.join("/"))
}

// ---------- internal: dedup loop ----------

type ProbeFn<'a> = dyn for<'t> Fn(
        &'t mut sqlx::Transaction<'a, sqlx::Postgres>,
        &'t str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = sqlx::Result<bool>> + Send + 't>,
    > + Send
    + Sync;

async fn probe_unique<F>(
    store: &PgStore,
    tenant: &Tenant,
    base: &str,
    is_free: F,
) -> Result<String>
where
    F: for<'t, 'a> Fn(
        &'t mut sqlx::Transaction<'a, sqlx::Postgres>,
        &'t str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = sqlx::Result<bool>> + Send + 't>,
    >,
{
    let mut candidate = base.to_string();
    let mut n = 2u32;
    loop {
        let mut tx = store.begin_for(tenant).await?;
        let free = is_free(&mut tx, &candidate).await?;
        tx.commit().await.ok();
        if free {
            return Ok(candidate);
        }
        candidate = format!("{base}-{n}");
        n += 1;
        if n > MAX_ATTEMPTS {
            return Err(anyhow!(
                "no free slug after {MAX_ATTEMPTS} attempts for base {base:?}"
            ));
        }
    }
}

// Keep the trait-object alias from being flagged as unused while the
// public surface stabilizes.
#[allow(dead_code)]
fn _force_use(_: &ProbeFn<'_>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Quarterly Revenue Model"), "quarterly-revenue-model");
        assert_eq!(slugify("Hello, World!"), "hello-world");
        assert_eq!(slugify("  --leading and trailing--  "), "leading-and-trailing");
        assert_eq!(slugify(""), "untitled");
        assert_eq!(slugify("////"), "untitled");
        assert_eq!(slugify("Q4 2026 / Forecast"), "q4-2026-forecast");
    }

    #[test]
    fn slugify_caps_at_80() {
        let long = "x".repeat(200);
        assert_eq!(slugify(&long).len(), 80);
    }

    #[test]
    fn slugify_folder_paths() {
        assert_eq!(slugify_folder("/Finance/Q4 Reports/"), "/finance/q4-reports");
        assert_eq!(slugify_folder("Finance/Q4"), "/finance/q4");
        assert_eq!(slugify_folder(""), "/");
        assert_eq!(slugify_folder("/"), "/");
    }
}
