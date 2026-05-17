//! PDF text extraction via `pdfium-render` (Rust bindings to Chrome's PDFium).
//!
//! Two-pass strategy:
//!   1. Extract the text layer directly (fast, lossless).
//!   2. If the text layer is empty or very sparse (< MIN_CHARS_PER_PAGE average),
//!      delegate to the Marker sidecar if configured (`QPEDIA_MARKER_URL`).
//!      Marker handles scanned/image-only PDFs via its own OCR pipeline.
//!      If Marker is not configured, the empty extraction is returned with a
//!      warning note so the operator knows OCR is needed.
//!
//! Requires the pdfium dynamic library at runtime. Search order:
//!   1. `$QPEDIA_PDFIUM_DIR`
//!   2. directory of the running binary
//!   3. `./vendor/pdfium`
//!   4. `./pdfium`
//!   5. system library path

use crate::{Extraction, Extractor, MarkerExtractor, PageBreak};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use pdfium_render::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};

/// Average characters per page below this threshold → treat as image-only PDF.
const MIN_CHARS_PER_PAGE: usize = 20;

pub struct PdfExtractor {
    pdfium: Arc<Pdfium>,
    /// Optional Marker sidecar for OCR fallback on image-only PDFs.
    /// Boxed to break the recursive type cycle (MarkerExtractor contains PdfExtractor).
    marker: Option<Box<dyn Extractor>>,
}

impl PdfExtractor {
    pub fn try_new() -> Result<Self> {
        let bindings = bind_pdfium().context("loading pdfium")?;
        // Wire up Marker as OCR fallback if the env var is set.
        let marker = std::env::var("QPEDIA_MARKER_URL")
            .ok()
            .filter(|u| !u.trim().is_empty())
            .and_then(|url| match MarkerExtractor::try_new(url.trim().to_string()) {
                Ok(m) => {
                    info!("PdfExtractor: Marker sidecar available for OCR fallback");
                    Some(Box::new(m) as Box<dyn Extractor>)
                }
                Err(e) => {
                    warn!(error = %e, "PdfExtractor: Marker unavailable; image-only PDFs will have empty text");
                    None
                }
            });
        Ok(Self { pdfium: Arc::new(Pdfium::new(bindings)), marker })
    }
}

fn bind_pdfium() -> Result<Box<dyn PdfiumLibraryBindings>> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Ok(p) = std::env::var("QPEDIA_PDFIUM_DIR") {
        candidates.push(PathBuf::from(p));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.to_path_buf());
        }
    }
    candidates.push(PathBuf::from("./vendor/pdfium"));
    candidates.push(PathBuf::from("./pdfium"));

    for dir in &candidates {
        let lib = Pdfium::pdfium_platform_library_name_at_path(dir);
        if let Ok(b) = Pdfium::bind_to_library(&lib) {
            info!(path = %lib.display(), "pdfium loaded");
            return Ok(b);
        }
    }

    Pdfium::bind_to_system_library()
        .map_err(|e| anyhow!("pdfium not found in candidate paths or on system: {e}"))
}

#[async_trait]
impl Extractor for PdfExtractor {
    fn handles_mime(&self, mime: &str) -> bool {
        mime == "application/pdf" || mime == "application/x-pdf"
    }

    async fn extract(&self, mime: &str, bytes: Bytes) -> Result<Extraction> {
        let pdfium = self.pdfium.clone();
        let bytes_vec = bytes.to_vec();

        // Phase 1: extract text layer in a blocking task.
        let extraction = tokio::task::spawn_blocking(move || -> Result<Extraction> {
            let doc = pdfium
                .load_pdf_from_byte_slice(&bytes_vec, None)
                .map_err(|e| anyhow!("pdf load: {e}"))?;

            let page_count = doc.pages().len() as usize;
            let mut text = String::new();
            let mut pages = Vec::new();

            for (i, page) in doc.pages().iter().enumerate() {
                let offset = text.len();
                pages.push(PageBreak { page: (i + 1) as u32, char_offset: offset });

                let page_text = page
                    .text()
                    .map_err(|e| anyhow!("page text: {e}"))?
                    .all();

                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(&page_text);
            }

            let avg_chars = if page_count > 0 {
                text.trim().len() / page_count
            } else {
                0
            };

            let notes = if avg_chars < MIN_CHARS_PER_PAGE {
                vec![format!(
                    "pdf text layer sparse ({avg_chars} chars/page avg); image-only PDF — OCR needed"
                )]
            } else {
                Vec::new()
            };

            Ok(Extraction { text, language: None, pages, notes })
        })
        .await
        .map_err(|e| anyhow!("pdf join: {e}"))??;

        // Phase 2: if text layer is sparse, delegate to Marker for OCR.
        let page_count = extraction.pages.len().max(1);
        let avg_chars = extraction.text.trim().len() / page_count;

        if avg_chars < MIN_CHARS_PER_PAGE {
            if let Some(marker) = &self.marker {
                info!(
                    avg_chars,
                    "pdf text layer sparse — delegating to Marker sidecar for OCR"
                );
                // On Marker failure, fall back to the pdfium extraction we
                // already have (phase 1). This avoids re-running pdfium and
                // breaks the construction cycle.
                match marker.extract(mime, bytes).await {
                    Ok(e) => return Ok(e),
                    Err(e) => {
                        warn!(error = %e, "marker failed; using pdfium text-layer result as fallback");
                        // extraction already contains the pdfium result — return it below.
                    }
                }
            } else {
                warn!(
                    avg_chars,
                    "pdf text layer sparse and QPEDIA_MARKER_URL not set — \
                     start the marker sidecar and set QPEDIA_MARKER_URL to enable OCR"
                );
            }
        }

        Ok(extraction)
    }
}
