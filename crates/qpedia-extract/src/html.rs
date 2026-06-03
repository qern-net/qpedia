//! HTML distillation extractor (ROADMAP Band 6.2).
//!
//! Raw HTML through the plain-text path (or `pandoc -f html` on the *whole*
//! page) keeps the chrome — nav, header, footer, cookie banners, scripts. This
//! extractor does a *readability* pass first: it picks the main content
//! container (`<article>`/`<main>`/common content ids & classes), dropping the
//! surrounding chrome, then converts just that subtree to Markdown with pandoc
//! (which also drops `<script>`/`<style>`). Much cleaner source text for the
//! wiki agent than the boilerplate-laden full page.
//!
//! This is "readability-lite" — container selection, not full text-density
//! scoring. It handles modern semantic / well-classed pages well; a page with
//! no content container falls back to the whole document (still pandoc-cleaned).

use crate::{Extraction, Extractor};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use scraper::{Html, Selector};
use std::process::Stdio;
use tokio::process::Command;

pub struct HtmlExtractor;

#[async_trait]
impl Extractor for HtmlExtractor {
    fn handles_mime(&self, mime: &str) -> bool {
        mime == "text/html" || mime == "application/xhtml+xml"
    }

    async fn extract(&self, _mime: &str, bytes: Bytes) -> Result<Extraction> {
        let html = String::from_utf8_lossy(&bytes).into_owned();
        let (title, content_html) = distill(&html);
        let md = html_to_markdown(&content_html).await?;
        let text = match &title {
            Some(t) => format!("# {t}\n\n{md}"),
            None => md,
        };
        Ok(Extraction {
            text,
            language: None,
            pages: Vec::new(),
            notes: vec!["distilled HTML (readability) → markdown via pandoc".into()],
        })
    }
}

/// (title, inner-HTML of the best content container). Falls back to the whole
/// document when nothing better is found.
fn distill(html: &str) -> (Option<String>, String) {
    let doc = Html::parse_document(html);

    let title = Selector::parse("title")
        .ok()
        .and_then(|s| doc.select(&s).next())
        .map(|e| e.text().collect::<String>().trim().to_string())
        .filter(|t| !t.is_empty());

    // First container with real text wins; ordered most- to least-specific.
    const CANDIDATES: &[&str] = &[
        "article",
        "main",
        "[role=\"main\"]",
        "#content",
        "#main",
        "#main-content",
        ".post-content",
        ".entry-content",
        ".article-content",
        ".content",
        "body",
    ];
    for sel_str in CANDIDATES {
        if let Ok(sel) = Selector::parse(sel_str) {
            if let Some(el) = doc.select(&sel).next() {
                let words = el.text().collect::<String>().split_whitespace().count();
                if words >= 10 {
                    return (title, el.inner_html());
                }
            }
        }
    }
    (title, html.to_string())
}

/// Convert an HTML fragment to GitHub-flavoured Markdown via pandoc. The
/// `-native_divs-native_spans` reader extensions drop `<div>`/`<span>`
/// wrappers; pandoc ignores `<script>`/`<style>` content.
async fn html_to_markdown(html: &str) -> Result<String> {
    let dir = tempfile::tempdir().context("tempdir")?;
    let in_path = dir.path().join("input.html");
    tokio::fs::write(&in_path, html).await.context("write tmp html")?;

    let mut cmd = Command::new("pandoc");
    cmd.arg(&in_path)
        .args(["-f", "html-native_divs-native_spans", "-t", "gfm", "--wrap=none"])
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
    let md = String::from_utf8(output.stdout).context("pandoc utf8")?;
    Ok(collapse_blank_lines(&md))
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = false;
    for line in s.lines() {
        let l = line.trim_end();
        if l.is_empty() {
            if !blank {
                out.push('\n');
            }
            blank = true;
        } else {
            out.push_str(l);
            out.push('\n');
            blank = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!doctype html><html><head><title>My Title</title></head>
    <body>
      <nav><a href="/">HomeLink</a> <a href="/about">AboutLink</a></nav>
      <header>SiteHeaderJunk</header>
      <main><article>
        <h1>Real Heading</h1>
        <p>This is the genuine main content paragraph with enough words to clear the threshold.</p>
        <ul><li>PointOne</li><li>PointTwo</li></ul>
      </article></main>
      <footer>FooterJunk Copyright</footer>
      <script>tracking_code()</script>
    </body></html>"#;

    #[test]
    fn handles_html_mimes() {
        let e = HtmlExtractor;
        assert!(e.handles_mime("text/html"));
        assert!(e.handles_mime("application/xhtml+xml"));
        assert!(!e.handles_mime("text/plain"));
        assert!(!e.handles_mime("application/pdf"));
    }

    #[test]
    fn distill_selects_main_content_and_drops_chrome() {
        let (title, content) = distill(PAGE);
        assert_eq!(title.as_deref(), Some("My Title"));
        // main content present:
        assert!(content.contains("genuine main content"), "content: {content}");
        assert!(content.contains("PointOne"));
        // chrome excluded (it lives outside <main>/<article>):
        assert!(!content.contains("HomeLink"), "nav leaked: {content}");
        assert!(!content.contains("SiteHeaderJunk"), "header leaked");
        assert!(!content.contains("FooterJunk"), "footer leaked");
        assert!(!content.contains("tracking_code"), "script leaked");
    }

    #[test]
    fn distill_falls_back_when_no_container() {
        let bare = "<html><body><p>just a few words here in a bare body element</p></body></html>";
        let (_t, content) = distill(bare);
        assert!(content.contains("just a few words"));
    }
}
