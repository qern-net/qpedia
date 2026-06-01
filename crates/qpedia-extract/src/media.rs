//! Audio/video metadata extractor (ROADMAP Band 6.5 floor; transcription is
//! Band 6.6).
//!
//! Registers `audio/*` and `video/*` so media files stop dead-lettering as
//! `tainted`. Like the image extractor, this is the *metadata floor*: we
//! index the container format, byte size, and — best-effort via `lofty` —
//! the duration and any embedded title/artist tags, as non-empty searchable
//! text so the source flows through the pipeline.
//!
//! Actual speech-to-text (so the *content* of a talk/clip lands in the wiki)
//! is a separate sidecar step (Band 6.6, Whisper). Until then a media file is
//! findable by this metadata plus its filename (on the `sources` row).

use crate::{Extraction, Extractor};
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;

pub struct MediaExtractor;

#[async_trait]
impl Extractor for MediaExtractor {
    fn handles_mime(&self, mime: &str) -> bool {
        mime.starts_with("audio/") || mime.starts_with("video/")
    }

    async fn extract(&self, mime: &str, bytes: Bytes) -> Result<Extraction> {
        let kind = if mime.starts_with("video/") { "video" } else { "audio" };
        let format = mime.split('/').nth(1).unwrap_or(mime);

        let mut fields = vec![format!("format={format}"), format!("bytes={}", bytes.len())];
        let mut notes =
            vec!["media indexed by metadata only (transcription pending — Band 6.6)".to_string()];

        // Best-effort container metadata. Never fail the source on a probe
        // error — degrade to format + size.
        match read_media_meta(&bytes) {
            Ok(meta) => fields.extend(meta),
            Err(e) => notes.push(format!("metadata probe failed: {e}")),
        }

        let text = format!("[{kind}] {}", fields.join(" "));
        Ok(Extraction { text, language: None, pages: Vec::new(), notes })
    }
}

/// Read duration + title/artist tags from an audio/video container.
fn read_media_meta(bytes: &[u8]) -> Result<Vec<String>> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::Accessor;

    let cursor = std::io::Cursor::new(bytes);
    let tagged = Probe::new(cursor).guess_file_type()?.read()?;

    let mut out = Vec::new();
    let secs = tagged.properties().duration().as_secs();
    if secs > 0 {
        out.push(format!("duration={}:{:02}", secs / 60, secs % 60));
    }
    if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
        if let Some(t) = tag.title() {
            out.push(format!("title=\"{}\"", t.trim()));
        }
        if let Some(a) = tag.artist() {
            out.push(format!("artist=\"{}\"", a.trim()));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_audio_and_video() {
        let e = MediaExtractor;
        assert!(e.handles_mime("video/mp4"));
        assert!(e.handles_mime("audio/mpeg"));
        assert!(e.handles_mime("audio/mp3"));
        assert!(!e.handles_mime("image/png"));
        assert!(!e.handles_mime("application/pdf"));
    }

    #[tokio::test]
    async fn garbage_bytes_do_not_fail() {
        // Unreadable container → no duration, but still non-empty text + no error.
        let out = MediaExtractor
            .extract("video/mp4", Bytes::from_static(b"not really an mp4"))
            .await
            .unwrap();
        assert!(out.text.starts_with("[video]"));
        assert!(out.text.contains("format=mp4"));
        assert!(out.notes.iter().any(|n| n.contains("probe failed")));
    }
}
