//! Ingest job handler: walks a source through every phase that's currently
//! implemented in a single job tick. See SPEC-v2.md §5.1.

use crate::handlers::{classify, distill, embed};
use crate::runner::IngestContext;
use anyhow::{anyhow, Result};
use qpedia_core::{source::SourceStatus, tenant::Tenant, SourceId};
use qpedia_store::blob::{BlobKind, BlobStorage};
use tracing::{info, warn, Instrument};

/// Whether a single pipeline step advanced the source (so the loop should
/// re-evaluate) or reached a point where this job tick is finished.
enum Step {
    /// The step did work; re-read status and continue while it keeps advancing.
    Continue,
    /// Terminal for this tick (Done, Tainted, unsupported, no-LLM, or a no-op
    /// status) — return `Ok(())` immediately.
    Stop,
}

pub async fn run(ctx: &IngestContext, tenant: &Tenant, source_id: &SourceId) -> Result<()> {
    loop {
        let src = ctx
            .db
            .get_source_in(tenant, source_id)
            .await?
            .ok_or_else(|| anyhow!("source not found: {source_id}"))?;
        let before = src.status;

        info!(id = %source_id, tenant = %src.tenant, status = ?src.status, "ingest tick");

        // One span per source-status transition, as a child of the active
        // `Job_Span` (Req 5.4). It records the originating status now and the
        // resulting status once the step completes; `otel.name` gives the OTel
        // span a stable, status-derived name.
        let span = tracing::info_span!(
            "Pipeline_Stage",
            otel.name = %format!("stage {before:?}"),
            "source.id" = %source_id,
            "source.status.from" = ?before,
            "source.status.to" = tracing::field::Empty,
        );

        let step = step_once(ctx, tenant, source_id, &src)
            .instrument(span.clone())
            .await?;

        let after = ctx
            .db
            .get_source_in(tenant, source_id)
            .await?
            .map(|s| s.status)
            .unwrap_or(before);
        span.record("source.status.to", tracing::field::debug(after));

        match step {
            Step::Stop => return Ok(()),
            Step::Continue => {
                if std::mem::discriminant(&after) == std::mem::discriminant(&before) {
                    return Ok(());
                }
            }
        }
    }
}

/// Execute exactly one pipeline step for `src`'s current status. Returns
/// [`Step::Stop`] for terminal/no-op statuses (Done, Tainted-on-no-LLM,
/// unsupported type, unknown status) and [`Step::Continue`] when it performed
/// work that may advance the source to the next status.
async fn step_once(
    ctx: &IngestContext,
    tenant: &Tenant,
    source_id: &SourceId,
    src: &qpedia_core::source::Source,
) -> Result<Step> {
    match src.status {
        SourceStatus::Pending | SourceStatus::Extracting | SourceStatus::Failed => {
            if super::archive::is_archive_mime(&src.mime) {
                // A zip isn't a document — expand it into child sources.
                super::archive::expand(ctx, tenant, source_id, src).await?;
            } else {
                extract_phase(ctx, tenant, source_id, &src.mime, &src.filename).await?;
            }
        }
        SourceStatus::Extracted | SourceStatus::Classifying => {
            if ctx.llm.is_some() {
                classify::run(ctx, tenant, source_id).await?;
            } else {
                info!(id = %source_id, "no LLM configured — marking Tainted at Extracted");
                ctx.db.update_status(tenant, source_id, SourceStatus::Tainted).await?;
                ctx.db.write_audit(tenant, "qpedia-bot", "source.tainted", Some(source_id.as_str()),
                    Some(&serde_json::json!({"reason": "no LLM configured", "stopped_at": "extracted"}))).await?;
                return Ok(Step::Stop);
            }
        }
        SourceStatus::Classified | SourceStatus::AgentDistilling => {
            if ctx.llm.is_some() {
                distill::run(ctx, tenant, source_id).await?;
            } else {
                info!(id = %source_id, "no LLM configured — marking Tainted at Classified");
                ctx.db.update_status(tenant, source_id, SourceStatus::Tainted).await?;
                ctx.db.write_audit(tenant, "qpedia-bot", "source.tainted", Some(source_id.as_str()),
                    Some(&serde_json::json!({"reason": "no LLM configured", "stopped_at": "classified"}))).await?;
                return Ok(Step::Stop);
            }
        }
        SourceStatus::Committed | SourceStatus::Embedding => {
            embed::run(ctx, tenant, source_id).await?;
        }
        SourceStatus::Done => {
            info!(id = %source_id, "ingest reached terminus (Done)");
            return Ok(Step::Stop);
        }
        other => {
            warn!(id = %source_id, status = ?other, "ingest no-op for status");
            return Ok(Step::Stop);
        }
    }
    Ok(Step::Continue)
}

async fn extract_phase(
    ctx: &IngestContext,
    tenant: &Tenant,
    source_id: &SourceId,
    mime: &str,
    filename: &str,
) -> Result<()> {
    ctx.db.update_status(tenant, source_id, SourceStatus::Extracting).await?;

    let ext = std::path::Path::new(filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");

    let bytes = ctx.blob.get(source_id, BlobKind::Original, ext).await?;

    // Unsupported type: degrade gracefully to a terminal `tainted` state with
    // a clear note, rather than returning Err (which fails the job and leaves
    // the source stranded at `extracting`, masquerading as in-progress). A
    // tainted source is re-drivable once an extractor for its mime lands
    // (e.g. zip → Band 6.4, video/audio transcription → Band 6.6).
    if !ctx.extractors.handles_mime(mime) {
        info!(id = %source_id, %mime, "no extractor for mime — marking tainted (unsupported type)");
        let note = serde_json::json!({
            "reason": "no extractor for mime",
            "mime": mime,
            "stopped_at": "extracting",
        });
        ctx.db.update_status(tenant, source_id, SourceStatus::Tainted).await?;
        ctx.db
            .write_audit(tenant, "qpedia-bot", "source.unsupported", Some(source_id.as_str()), Some(&note))
            .await?;
        return Ok(());
    }

    let extraction = ctx.extractors.extract(mime, bytes.clone()).await?;
    let mut text = extraction.text;
    let mut notes = extraction.notes;

    // Image OCR / vision description (Band 6.1): for images, the extractor
    // only yields metadata. If a vision model is available, ask the LLM to
    // transcribe any text and/or describe the picture, and use that as the
    // page content (metadata kept as a trailer). Best-effort: on any error we
    // keep the metadata floor so the source still ingests.
    if mime.starts_with("image/") {
        if let (Some(llm), Some(vmodel)) = (&ctx.llm, qpedia_llm::vision_model()) {
            match describe_image(llm.as_ref(), &vmodel, mime, &bytes).await {
                Ok(desc) if !desc.trim().is_empty() => {
                    info!(id = %source_id, model = %vmodel, chars = desc.len(), "image described via vision");
                    notes.push(format!("vision: OCR/description via {vmodel}"));
                    text = format!("{}\n\n---\n{text}", desc.trim());
                }
                Ok(_) => notes.push("vision: empty result; kept metadata only".into()),
                Err(e) => {
                    info!(id = %source_id, error = %e, "vision failed; keeping metadata only");
                    notes.push(format!("vision failed ({e}); kept metadata only"));
                }
            }
        }
    }

    ctx.blob.put_text(source_id, BlobKind::Extracted, &text).await?;

    let manifest = serde_json::json!({
        "language": extraction.language,
        "page_count": extraction.pages.len(),
        "char_count": text.len(),
        "notes": notes,
    });
    ctx.blob
        .put_text(source_id, BlobKind::Manifest, &serde_json::to_string_pretty(&manifest)?)
        .await?;

    ctx.db.update_status(tenant, source_id, SourceStatus::Extracted).await?;
    ctx.db
        .write_audit(tenant, "qpedia-bot", "source.extracted", Some(source_id.as_str()), Some(&manifest))
        .await?;

    info!(id = %source_id, bytes = text.len(), notes = ?notes, "extraction complete");
    Ok(())
}

/// Ask a vision-capable LLM to OCR and/or describe an image, returning plain
/// text suitable as wiki source content.
async fn describe_image(
    llm: &dyn qpedia_llm::LlmProvider,
    model: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<String> {
    const PROMPT: &str = "You are extracting an image's content for a knowledge base. \
If the image contains text (a scan, screenshot, slide, table, or document), transcribe ALL of it faithfully, preserving structure and reading order. \
If it is a photo, diagram, chart, map, or illustration, describe in detail what it depicts: subjects, setting, any visible text/labels, and — for charts/tables — the data and relationships. \
If it is mixed, do both. Output only the transcription/description as plain text or markdown, with no preamble or commentary.";

    let req = qpedia_llm::VisionReq {
        model: model.to_string(),
        prompt: PROMPT.to_string(),
        image_mime: mime.to_string(),
        image_bytes: bytes.to_vec(),
        max_tokens: 4096,
    };
    llm.vision(req).await.map_err(|e| anyhow!("{e}"))
}
