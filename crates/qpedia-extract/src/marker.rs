//! Marker sidecar extractor — high-fidelity PDF via the optional
//! marker-pdf Python service (DESIGN.md §16, sidecar/marker/).
//!
//! `MarkerExtractor` only calls the remote sidecar. It does NOT embed a
//! `PdfExtractor` fallback — that would create a construction cycle:
//!   PdfExtractor::try_new → MarkerExtractor::try_new → PdfExtractor::try_new → ∞
//!
//! Fallback on sidecar failure is handled by the caller (`PdfExtractor`),
//! which already holds the pdfium-extracted text from phase 1 and returns
//! that when Marker returns an error.

use crate::{Extraction, Extractor};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;
use std::time::Duration;
use tracing::info;

pub struct MarkerExtractor {
    base_url: String,
    client: reqwest::Client,
}

impl MarkerExtractor {
    pub fn try_new(base_url: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            // Marker on CPU can take minutes per PDF.
            .timeout(Duration::from_secs(600))
            .build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client,
        })
    }
}

#[async_trait]
impl Extractor for MarkerExtractor {
    fn handles_mime(&self, mime: &str) -> bool {
        mime == "application/pdf" || mime == "application/x-pdf"
    }

    /// Call the Marker sidecar. Returns `Err` on any failure — the caller
    /// is responsible for fallback behaviour.
    async fn extract(&self, _mime: &str, bytes: Bytes) -> Result<Extraction> {
        let url = format!("{}/extract", self.base_url);
        let part = reqwest::multipart::Part::bytes(bytes.to_vec())
            .file_name("input.pdf")
            .mime_str("application/pdf")?;
        let form = reqwest::multipart::Form::new().part("file", part);

        let resp = self.client.post(&url).multipart(form).send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("marker {status}: {body}"));
        }
        let parsed: MarkerResponse = serde_json::from_str(&body)
            .map_err(|e| anyhow!("decode marker response: {e}; body: {body}"))?;

        let language = parsed
            .metadata
            .as_ref()
            .and_then(|m| m.get("language"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        info!(chars = parsed.markdown.len(), "marker extraction succeeded");
        Ok(Extraction {
            text: parsed.markdown,
            language,
            pages: Vec::new(),
            notes: vec!["extracted via marker sidecar".into()],
        })
    }
}

#[derive(Debug, Deserialize)]
struct MarkerResponse {
    markdown: String,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}
