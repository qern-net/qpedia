//! Plain text + markdown passthrough extractor.

use crate::{Extraction, Extractor};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;

pub struct TextExtractor;

#[async_trait]
impl Extractor for TextExtractor {
    fn handles_mime(&self, mime: &str) -> bool {
        mime.starts_with("text/")
            || mime == "application/json"
            || mime == "application/xml"
    }

    async fn extract(&self, _mime: &str, bytes: Bytes) -> Result<Extraction> {
        let text = std::str::from_utf8(&bytes)
            .map_err(|e| anyhow!("non-utf8 text: {e}"))?
            .to_string();
        Ok(Extraction {
            text,
            language: None,
            pages: Vec::new(),
            notes: Vec::new(),
        })
    }
}
