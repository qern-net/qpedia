//! Sync job handler: pulls changed docs from an external connector and
//! ingests them via the normal pipeline (one Source row + Ingest job per
//! doc). See DESIGN.md §16 (External connectors).

use crate::runner::IngestContext;
use crate::runner::ingest_job;
use anyhow::{anyhow, Result};
use chrono::Utc;
use qpedia_connectors::{build as build_connector, RemoteDoc};
use qpedia_core::{
    acl::Acl,
    source::{Source, SourceStatus},
    tenant::Tenant,
    SourceId,
};
use qpedia_store::{
    blob::{BlobKind, BlobStorage},
    sqlite::{JobQueue, SourceStore},
};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

pub async fn run(ctx: &IngestContext, connector_id: &str) -> Result<()> {
    let cfg = ctx
        .db
        .get_connector(connector_id)
        .await?
        .ok_or_else(|| anyhow!("connector not found: {connector_id}"))?;
    if !cfg.enabled {
        info!(connector = %cfg.name, "sync: connector disabled — skipping");
        return Ok(());
    }

    let tenant = Tenant::new(cfg.tenant.clone());
    let folder_path = format!("/connectors/{}", sanitize_segment(&cfg.name));

    let connector = build_connector(&cfg)?;
    info!(connector = %cfg.name, kind = %cfg.kind, cursor = ?cfg.cursor, "sync: starting");

    let result = match connector.list_changed(cfg.cursor.as_deref()).await {
        Ok(r) => r,
        Err(e) => {
            let _ = ctx
                .db
                .update_connector_cursor(connector_id, cfg.cursor.as_deref(), Some(&e.to_string()))
                .await;
            return Err(anyhow!("list_changed: {e}"));
        }
    };

    info!(
        connector = %cfg.name,
        new_docs = result.docs.len(),
        "sync: list_changed returned"
    );

    // Resolve folder ACL once for the synthetic uploader. Falls back to
    // an admin-only ACL so connector content doesn't accidentally become
    // visible across groups.
    let upload_acl = ctx
        .db
        .resolve_folder_acl(&tenant, &folder_path)
        .await?
        .unwrap_or_else(|| Acl::from_iter(["admin".to_string()]));

    let mut ingested = 0usize;
    let mut errors = 0usize;
    for doc in &result.docs {
        match ingest_one(ctx, &tenant, &folder_path, &upload_acl, &cfg.name, doc).await {
            Ok(()) => ingested += 1,
            Err(e) => {
                errors += 1;
                warn!(remote_id = %doc.remote_id, error = %e, "sync: doc ingest failed");
            }
        }
    }

    let err_summary = if errors == 0 {
        None
    } else {
        Some(format!("{errors} of {} docs failed", result.docs.len()))
    };
    ctx.db
        .update_connector_cursor(
            connector_id,
            result.new_cursor.as_deref(),
            err_summary.as_deref(),
        )
        .await?;
    ctx.db
        .audit(
            &format!("connector:{}", cfg.kind),
            "connector.sync",
            Some(connector_id),
            Some(&serde_json::json!({
                "ingested": ingested,
                "errors": errors,
                "tenant": cfg.tenant,
                "name": cfg.name,
            })),
        )
        .await?;

    info!(connector = %cfg.name, ingested, errors, "sync: complete");
    Ok(())
}

async fn ingest_one(
    ctx: &IngestContext,
    tenant: &Tenant,
    folder_path: &str,
    upload_acl: &Acl,
    connector_name: &str,
    doc: &RemoteDoc,
) -> Result<()> {
    let connector = ctx
        .db
        .get_connector_by_name(tenant, connector_name)
        .await?
        .ok_or_else(|| anyhow!("connector vanished mid-sync: {connector_name}"))?;
    let connector_impl = build_connector(&connector)?;
    let dl = connector_impl.download(doc).await?;

    // Hash bytes for de-duplication and audit.
    let mut hasher = Sha256::new();
    hasher.update(&dl.bytes);
    let sha256 = hex::encode(hasher.finalize());

    let id = SourceId::new();
    let now = Utc::now();
    let src = Source {
        id: id.clone(),
        tenant: tenant.clone(),
        folder_path: folder_path.to_string(),
        filename: dl.filename.clone(),
        mime: dl.mime.clone(),
        sha256,
        size_bytes: dl.bytes.len() as u64,
        acl: upload_acl.clone(),
        status: SourceStatus::Pending,
        language: None,
        created_at: now,
        ingested_at: None,
        classification: None,
    };
    ctx.db.insert_source(&src).await?;

    let ext = std::path::Path::new(&dl.filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");
    ctx.blob.put(&id, BlobKind::Original, ext, dl.bytes).await?;

    let job = ingest_job(&id)?;
    ctx.db.enqueue(&job).await?;
    Ok(())
}

fn sanitize_segment(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect::<String>()
        .to_lowercase()
}
