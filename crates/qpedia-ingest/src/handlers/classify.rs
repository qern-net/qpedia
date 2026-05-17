//! Classifier phase. See DESIGN.md §5.3.
//!
//! Sends the start of the extracted text to the configured LLM and asks for
//! a small JSON record: doc_type / language / sensitivity / hints. Updates
//! the source's `language` field and stores the full classification in the
//! audit trail. Future: also feed `hints` into the agent's priors.

use crate::runner::IngestContext;
use anyhow::{anyhow, Context, Result};
use qpedia_core::{source::SourceStatus, SourceId};
use qpedia_llm::{current_model, CompleteReq};
use qpedia_store::{
    blob::{BlobKind, BlobStorage},
    sqlite::SourceStore,
};
use tracing::info;

const CLASSIFIER_SYSTEM: &str = r#"You are a document classifier. Output ONE JSON object and nothing else.

Schema:
{
  "doc_type": "contract|report|email|slides|manual|invoice|form|code|other",
  "language": "<ISO 639-1 lowercase, e.g. en, es, fr>",
  "sensitivity": "low|medium|high",
  "hints": ["<3-7 short keywords or phrases>"]
}

Rules:
- Output starts with { and ends with }.
- No markdown code fences.
- No preamble or explanation.
- If the document is empty or unreadable, return doc_type="other" and language="und"."#;

const SNIPPET_CHARS: usize = 6000;

pub async fn run(ctx: &IngestContext, source_id: &SourceId) -> Result<()> {
    let llm = ctx
        .llm
        .as_ref()
        .ok_or_else(|| anyhow!("classify called without an LLM provider"))?;

    let src = ctx
        .db
        .get_source(source_id)
        .await?
        .ok_or_else(|| anyhow!("source not found: {source_id}"))?;

    ctx.db.update_status(source_id, SourceStatus::Classifying).await?;

    let extracted_bytes = ctx
        .blob
        .get(source_id, BlobKind::Extracted, "txt")
        .await
        .context("read extracted.txt")?;
    let text = String::from_utf8_lossy(&extracted_bytes);
    let snippet: String = text.chars().take(SNIPPET_CHARS).collect();

    if snippet.trim().is_empty() {
        // Nothing to classify (e.g. scanned PDF awaiting OCR). Move on with an empty record.
        let empty = serde_json::json!({"doc_type": "other", "language": "und", "sensitivity": "low", "hints": [], "skipped": "empty_extracted_text"});
        ctx.db.update_status(source_id, SourceStatus::Classified).await?;
        ctx.db
            .audit("qpedia-bot", "source.classified", Some(source_id.as_str()), Some(&empty))
            .await?;
        info!(id = %source_id, "classify skipped — empty extracted text");
        return Ok(());
    }

    let user_msg = format!(
        "Filename: {}\nMime: {}\nContent (first ~{} chars):\n{}",
        src.filename, src.mime, SNIPPET_CHARS, snippet
    );

    let req = CompleteReq::new(current_model())
        .system(CLASSIFIER_SYSTEM)
        .user(user_msg)
        .max_tokens(400)
        .temperature(0.0);

    let resp = llm
        .complete(req)
        .await
        .map_err(|e| anyhow!("classifier llm call: {e}"))?;

    let classification = parse_classification(&resp.content)
        .with_context(|| format!("parsing classifier output: {}", resp.content))?;

    if let Some(lang) = classification.get("language").and_then(|v| v.as_str()) {
        ctx.db.update_language(source_id, lang).await?;
    }
    ctx.db.update_classification(source_id, &classification).await?;

    // Auto-organize: move source into a folder named after its doc_type
    // if it is still in the root folder ("/"). This keeps the sources list
    // tidy without overriding any folder the user explicitly chose.
    if src.folder_path == "/" {
        if let Some(doc_type) = classification.get("doc_type").and_then(|v| v.as_str()) {
            let auto_folder = format!("/{}", doc_type.trim());
            ctx.db.update_folder_path(source_id, &auto_folder).await?;
            info!(id = %source_id, folder = %auto_folder, "auto-organized into folder by doc_type");
        }
    }

    ctx.db.update_status(source_id, SourceStatus::Classified).await?;
    ctx.db
        .audit("qpedia-bot", "source.classified", Some(source_id.as_str()), Some(&classification))
        .await?;

    info!(
        id = %source_id,
        tokens = resp.usage.input_tokens + resp.usage.output_tokens,
        classification = %classification,
        "classification complete"
    );
    Ok(())
}

fn parse_classification(text: &str) -> Result<serde_json::Value> {
    let trimmed = text.trim();
    // Strip leading/trailing markdown fences if the model added them anyway.
    let no_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    let cleaned = no_fence
        .strip_suffix("```")
        .unwrap_or(no_fence)
        .trim();

    let v: serde_json::Value =
        serde_json::from_str(cleaned).map_err(|e| anyhow!("not JSON: {e}"))?;
    if !v.is_object() {
        return Err(anyhow!("expected JSON object, got: {cleaned}"));
    }
    Ok(v)
}
