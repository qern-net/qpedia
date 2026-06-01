//! Document extraction: PDF, DOCX, HTML, images (OCR), etc.
//! See DESIGN.md §5.2.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub mod text;
pub mod pdf;
pub mod docx;
pub mod image;
pub mod media;
pub mod marker;

pub use text::TextExtractor;
pub use pdf::PdfExtractor;
pub use docx::DocxExtractor;
pub use image::ImageExtractor;
pub use media::MediaExtractor;
pub use marker::MarkerExtractor;

#[async_trait]
pub trait Extractor: Send + Sync {
    fn handles_mime(&self, mime: &str) -> bool;
    async fn extract(&self, mime: &str, bytes: Bytes) -> Result<Extraction>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extraction {
    pub text: String,
    pub language: Option<String>,
    pub pages: Vec<PageBreak>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageBreak {
    pub page: u32,
    pub char_offset: usize,
}

/// Registry of extractors. Picks the first one that handles the mime.
pub struct ExtractorRegistry {
    extractors: Vec<Box<dyn Extractor>>,
}

impl ExtractorRegistry {
    pub fn new() -> Self {
        Self { extractors: Vec::new() }
    }

    /// All built-in extractors. PDF support is optional: if pdfium isn't
    /// available, PDFs will be rejected at extract-time but text/docx still work.
    ///
    /// When `QPEDIA_MARKER_URL` is set, a MarkerExtractor is registered
    /// ahead of `PdfExtractor` for PDFs. Marker failures fall back to
    /// pdfium so a broken sidecar doesn't break ingestion.
    pub fn with_default() -> Self {
        let mut reg = Self::new();
        reg.register(Box::new(TextExtractor));

        // PdfExtractor wires up Marker internally as an OCR fallback when
        // QPEDIA_MARKER_URL is set — no need to register MarkerExtractor
        // separately in the registry.
        match PdfExtractor::try_new() {
            Ok(pdf) => reg.register(Box::new(pdf)),
            Err(e) => tracing::warn!(error = %e, "PdfExtractor disabled — run scripts/fetch-pdfium.sh"),
        }
        reg.register(Box::new(DocxExtractor));
        // Images: index metadata so they don't dead-letter. OCR (Band 6.1)
        // is a separate sidecar capability layered on later.
        reg.register(Box::new(ImageExtractor));
        // Audio/video: index container metadata (duration/tags). Transcription
        // (Band 6.6) is a separate sidecar step.
        reg.register(Box::new(MediaExtractor));
        reg
    }

    pub fn register(&mut self, e: Box<dyn Extractor>) {
        self.extractors.push(e);
    }

    pub async fn extract(&self, mime: &str, bytes: Bytes) -> Result<Extraction> {
        for ex in &self.extractors {
            if ex.handles_mime(mime) {
                return ex.extract(mime, bytes).await;
            }
        }
        Err(anyhow!("no extractor for mime: {mime}"))
    }

    pub fn handles_mime(&self, mime: &str) -> bool {
        self.extractors.iter().any(|e| e.handles_mime(mime))
    }
}

impl Default for ExtractorRegistry {
    fn default() -> Self { Self::with_default() }
}

// Concrete impls planned next (DESIGN.md §5.2, ROADMAP Band 6):
//   HtmlExtractor (readability distillation, not raw pandoc)  — 6.2
//   ImageOcr      (Marker/surya sidecar — images + scanned PDFs) — 6.1
//   XlsxExtractor / PptxExtractor (pandoc/calamine)            — 6.5
//   EmailExtractor (mail-parser, eml/msg)                      — 6.5
//   ImageExtractor (metadata floor) is registered above        — 6.0 ✅
