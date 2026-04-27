//! PDF text extraction via `pdfium-render` (Rust bindings to Chrome's PDFium).
//!
//! Requires the pdfium dynamic library at runtime. Search order:
//!   1. `$QPEDIA_PDFIUM_DIR`
//!   2. directory of the running binary
//!   3. `./vendor/pdfium`
//!   4. `./pdfium`
//!   5. system library path
//!
//! Run `scripts/fetch-pdfium.sh` once to populate `vendor/pdfium/`.

use crate::{Extraction, Extractor, PageBreak};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use pdfium_render::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

pub struct PdfExtractor {
    pdfium: Arc<Pdfium>,
}

impl PdfExtractor {
    pub fn try_new() -> Result<Self> {
        let bindings = bind_pdfium().context("loading pdfium")?;
        Ok(Self { pdfium: Arc::new(Pdfium::new(bindings)) })
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

    async fn extract(&self, _mime: &str, bytes: Bytes) -> Result<Extraction> {
        let pdfium = self.pdfium.clone();
        let bytes = bytes.to_vec();

        tokio::task::spawn_blocking(move || -> Result<Extraction> {
            let doc = pdfium
                .load_pdf_from_byte_slice(&bytes, None)
                .map_err(|e| anyhow!("pdf load: {e}"))?;

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

            let mut notes = Vec::new();
            if text.trim().is_empty() {
                notes.push("pdf has no text layer; OCR required".into());
            }

            Ok(Extraction {
                text,
                language: None,
                pages,
                notes,
            })
        })
        .await
        .map_err(|e| anyhow!("pdf join: {e}"))?
    }
}
