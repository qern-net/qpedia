//! DOCX (and other Office formats) via pandoc subprocess.
//!
//! pandoc handles .docx, .odt, .epub, .rtf, .pptx, .xlsx, etc. We pass `-f`
//! based on mime and write to a temp file (zip formats need seekable input).
//!
//! Requires `pandoc` on PATH. The runtime container image installs it; for
//! local dev without pandoc, extraction will fail with a clear error.

use crate::{Extraction, Extractor};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use std::process::Stdio;
use tokio::process::Command;

pub struct DocxExtractor;

fn pandoc_format(mime: &str) -> Option<&'static str> {
    match mime {
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => Some("docx"),
        "application/vnd.openxmlformats-officedocument.presentationml.presentation" => Some("pptx"),
        "application/vnd.oasis.opendocument.text" => Some("odt"),
        "application/rtf" | "text/rtf" => Some("rtf"),
        "application/epub+zip" => Some("epub"),
        _ => None,
    }
}

#[async_trait]
impl Extractor for DocxExtractor {
    fn handles_mime(&self, mime: &str) -> bool {
        pandoc_format(mime).is_some()
    }

    async fn extract(&self, mime: &str, bytes: Bytes) -> Result<Extraction> {
        let fmt = pandoc_format(mime).ok_or_else(|| anyhow!("unsupported mime: {mime}"))?;

        // Zip-based formats need a real file on disk; pipe doesn't seek.
        let dir = tempfile::tempdir().context("tempdir")?;
        let in_path = dir.path().join(format!("input.{fmt}"));
        tokio::fs::write(&in_path, &bytes).await.context("write tmp")?;

        let mut cmd = Command::new("pandoc");
        cmd.arg(&in_path)
            .args(["-f", fmt, "-t", "plain", "--wrap=none"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let child = cmd.spawn().map_err(|e| {
            anyhow!("failed to spawn pandoc: {e}. Install pandoc or run inside the qpedia container.")
        })?;
        let output = child.wait_with_output().await.context("pandoc wait")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!("pandoc failed ({}): {stderr}", output.status));
        }

        let text = String::from_utf8(output.stdout).context("pandoc utf8")?;
        Ok(Extraction {
            text,
            language: None,
            pages: Vec::new(),
            notes: vec![format!("extracted via pandoc -f {fmt}")],
        })
    }
}

