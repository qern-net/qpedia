//! Ingest job handler: walks a source through every phase that's currently
//! implemented in a single job tick. See DESIGN.md §5.1.

use crate::handlers::{classify, distill, embed};
use crate::runner::IngestContext;
use anyhow::{anyhow, Result};
use qpedia_core::{source::SourceStatus, SourceId};
use qpedia_store::{
    blob::{BlobKind, BlobStorage},
    sqlite::SourceStore,
};
use tracing::{info, warn};

pub async fn run(ctx: &IngestContext, source_id: &SourceId) -> Result<()> {
    loop {
        let src = ctx
            .db
            .get_source(source_id)
            .await?
            .ok_or_else(|| anyhow!("source not found: {source_id}"))?;
        let before = src.status;

        info!(id = %source_id, status = ?src.status, "ingest tick");

        match src.status {
            SourceStatus::Pending | SourceStatus::Extracting | SourceStatus::Failed => {
                extract_phase(ctx, source_id, &src.mime, &src.filename).await?;
            }
            SourceStatus::Extracted | SourceStatus::Classifying => {
                if ctx.llm.is_some() {
                    classify::run(ctx, source_id).await?;
                } else {
                    info!(id = %source_id, "no LLM configured — stopping at Extracted");
                    return Ok(());
                }
            }
            SourceStatus::Classified | SourceStatus::AgentDistilling => {
                if ctx.llm.is_some() {
                    distill::run(ctx, source_id).await?;
                } else {
                    info!(id = %source_id, "no LLM configured — stopping at Classified");
                    return Ok(());
                }
            }
            SourceStatus::Committed | SourceStatus::Embedding => {
                embed::run(ctx, source_id).await?;
            }
            SourceStatus::Done => {
                info!(id = %source_id, "ingest reached terminus (Done)");
                return Ok(());
            }
            other => {
                warn!(id = %source_id, status = ?other, "ingest no-op for status");
                return Ok(());
            }
        }

        // Guard against infinite loops if a phase didn't advance status.
        let after = ctx
            .db
            .get_source(source_id)
            .await?
            .map(|s| s.status)
            .unwrap_or(before);
        if std::mem::discriminant(&after) == std::mem::discriminant(&before) {
            return Ok(());
        }
    }
}

async fn extract_phase(
    ctx: &IngestContext,
    source_id: &SourceId,
    mime: &str,
    filename: &str,
) -> Result<()> {
    ctx.db.update_status(source_id, SourceStatus::Extracting).await?;

    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");

    let bytes = ctx.blob.get(source_id, BlobKind::Original, ext).await?;

    if !ctx.extractors.handles_mime(mime) {
        return Err(anyhow!("no extractor for mime: {mime}"));
    }

    let extraction = ctx.extractors.extract(mime, bytes).await?;
    ctx.blob.put_text(source_id, BlobKind::Extracted, &extraction.text).await?;

    let manifest = serde_json::json!({
        "language": extraction.language,
        "page_count": extraction.pages.len(),
        "char_count": extraction.text.len(),
        "notes": extraction.notes,
    });
    ctx.blob
        .put_text(source_id, BlobKind::Manifest, &serde_json::to_string_pretty(&manifest)?)
        .await?;

    ctx.db.update_status(source_id, SourceStatus::Extracted).await?;
    ctx.db
        .audit("qpedia-bot", "source.extracted", Some(source_id.as_str()), Some(&manifest))
        .await?;

    info!(
        id = %source_id,
        bytes = extraction.text.len(),
        notes = ?extraction.notes,
        "extraction complete"
    );
    Ok(())
}
