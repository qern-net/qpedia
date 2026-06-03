//! Image metadata extractor (ROADMAP Band 6.0).
//!
//! Registers `image/*` so image uploads stop dead-lettering with
//! "no extractor for mime: image/jpeg". This is the metadata *floor*: we
//! index the format, pixel dimensions, and byte size as (non-empty)
//! searchable text so the source flows through the pipeline instead of
//! failing the job.
//!
//! OCR of the image *contents* is a separate step (Band 6.1), routed
//! through the Marker sidecar (same class of work as scanned-PDF OCR).
//! Until that lands an image is findable by this metadata plus its
//! filename (the filename lives on the `sources` row, not here — the
//! `Extractor` trait only sees the bytes + mime).
//!
//! `imagesize` reads only the header (no full decode), so this is fast and
//! safe on adversarial input.

use crate::{Extraction, Extractor};
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;

pub struct ImageExtractor;

#[async_trait]
impl Extractor for ImageExtractor {
    fn handles_mime(&self, mime: &str) -> bool {
        mime.starts_with("image/")
    }

    async fn extract(&self, mime: &str, bytes: Bytes) -> Result<Extraction> {
        let format = mime.strip_prefix("image/").unwrap_or(mime);
        let dims = imagesize::blob_size(bytes.as_ref()).ok();
        let dim_str = dims
            .as_ref()
            .map(|d| format!("{}x{}", d.width, d.height))
            .unwrap_or_else(|| "unknown".to_string());

        // Must be non-empty: the classify handler skips sources with empty
        // extracted text, which would strand the image at `extracted`.
        let text = format!(
            "[image] format={format} dimensions={dim_str} bytes={}",
            bytes.len()
        );
        let mut notes = vec!["image indexed by metadata only (no OCR yet — Band 6.1)".to_string()];
        if dims.is_none() {
            notes.push("could not read image dimensions from header".to_string());
        }
        Ok(Extraction { text, language: None, pages: Vec::new(), notes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext() -> ImageExtractor { ImageExtractor }

    #[test]
    fn handles_all_image_subtypes() {
        let e = ext();
        assert!(e.handles_mime("image/jpeg"));
        assert!(e.handles_mime("image/png"));
        assert!(e.handles_mime("image/webp"));
        assert!(!e.handles_mime("application/pdf"));
        assert!(!e.handles_mime("text/plain"));
    }

    #[tokio::test]
    async fn reads_png_dimensions() {
        // Minimal 1x1 PNG.
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let out = ext().extract("image/png", Bytes::from(png.to_vec())).await.unwrap();
        assert!(out.text.contains("dimensions=1x1"), "got: {}", out.text);
        assert!(out.text.contains("format=png"));
        assert!(!out.text.is_empty());
    }

    #[tokio::test]
    async fn garbage_bytes_do_not_fail() {
        // Unreadable header → dimensions=unknown, but still non-empty text
        // and no error (the job must not dead-letter).
        let out = ext().extract("image/jpeg", Bytes::from_static(b"not really a jpeg")).await.unwrap();
        assert!(out.text.contains("dimensions=unknown"));
        assert!(out.notes.iter().any(|n| n.contains("dimensions")));
    }
}
