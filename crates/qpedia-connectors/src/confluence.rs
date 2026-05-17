//! Confluence Cloud connector. Pulls pages from a single space and feeds
//! them to qpedia as text/html sources. Cursor = most-recent
//! page.version.when seen so we resume on the next sync.
//!
//! Auth: Basic with email + API token (Atlassian's standard for Cloud).
//! Create a token at id.atlassian.com -> Account settings -> Security ->
//! API tokens.
//!
//! Config shape (config_json):
//!   {
//!     "base_url":  "https://<your-domain>.atlassian.net",
//!     "email":     "you@example.com",
//!     "api_token": "<token>",
//!     "space_key": "ENG"
//!   }

use crate::{Connector, Downloaded, ListChanged, RemoteDoc};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chrono::DateTime;
use serde::Deserialize;
use std::time::Duration;

const PAGE_LIMIT: usize = 100;

#[derive(Debug, Clone, Deserialize)]
struct ConfluenceConfig {
    base_url: String,
    email: String,
    api_token: String,
    space_key: String,
}

pub struct ConfluenceConnector {
    cfg: ConfluenceConfig,
    client: reqwest::Client,
    auth_header: String,
}

impl ConfluenceConnector {
    pub fn from_config(value: &serde_json::Value) -> Result<Self> {
        let cfg: ConfluenceConfig = serde_json::from_value(value.clone())
            .context("confluence config: expected {base_url, email, api_token, space_key}")?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .context("build confluence http client")?;
        let auth = B64.encode(format!("{}:{}", cfg.email, cfg.api_token));
        let auth_header = format!("Basic {auth}");
        Ok(Self { cfg, client, auth_header })
    }

    fn api_url(&self, path: &str) -> String {
        format!(
            "{}/wiki/rest/api{}",
            self.cfg.base_url.trim_end_matches('/'),
            path
        )
    }
}

#[async_trait]
impl Connector for ConfluenceConnector {
    fn kind(&self) -> &'static str { "confluence" }

    async fn list_changed(&self, cursor: Option<&str>) -> Result<ListChanged> {
        // Cursor: RFC 3339 timestamp of latest page.version.when seen so far.
        let since = cursor.and_then(|s| DateTime::parse_from_rfc3339(s).ok());

        let mut docs = Vec::new();
        let mut max_seen: Option<chrono::DateTime<chrono::Utc>> = None;
        let mut start = 0usize;

        loop {
            let url = self.api_url(&format!(
                "/content?spaceKey={}&type=page&start={}&limit={}&expand=version",
                urlencoding(&self.cfg.space_key),
                start,
                PAGE_LIMIT,
            ));
            let resp = self
                .client
                .get(&url)
                .header("Authorization", &self.auth_header)
                .header("Accept", "application/json")
                .send()
                .await?;
            let status = resp.status();
            let text = resp.text().await?;
            if !status.is_success() {
                return Err(anyhow!("confluence list ({status}): {text}"));
            }
            let page: ContentList = serde_json::from_str(&text)
                .map_err(|e| anyhow!("decode list: {e}; body: {text}"))?;

            let count = page.results.len();
            for item in page.results {
                let when_raw = item.version.as_ref().and_then(|v| v.when.as_deref());
                let when = when_raw.and_then(|w| DateTime::parse_from_rfc3339(w).ok());

                // Skip pages older than `since` (incremental sync).
                if let (Some(s), Some(w)) = (since, when) {
                    if w <= s { continue; }
                }
                if let Some(w) = when {
                    let w_utc = w.with_timezone(&chrono::Utc);
                    max_seen = Some(max_seen.map_or(w_utc, |m| m.max(w_utc)));
                }

                let title_str = item.title.as_deref().unwrap_or(&item.id);
                docs.push(RemoteDoc {
                    remote_id: item.id.clone(),
                    name: format!("{}.html", sanitize_filename(title_str)),
                    mime: "text/html".into(),
                    modified_at: when.map(|w| w.with_timezone(&chrono::Utc)),
                    size_bytes: None,
                });
            }

            if count < PAGE_LIMIT {
                break;
            }
            start += PAGE_LIMIT;
        }

        Ok(ListChanged {
            docs,
            new_cursor: max_seen.map(|d| d.to_rfc3339()).or_else(|| cursor.map(|s| s.to_string())),
        })
    }

    async fn download(&self, doc: &RemoteDoc) -> Result<Downloaded> {
        let url = self.api_url(&format!(
            "/content/{}?expand=body.export_view,version",
            doc.remote_id,
        ));
        let resp = self
            .client
            .get(&url)
            .header("Authorization", &self.auth_header)
            .header("Accept", "application/json")
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(anyhow!("confluence download ({status}): {text}"));
        }
        let item: ContentItem = serde_json::from_str(&text)
            .map_err(|e| anyhow!("decode item: {e}; body: {text}"))?;

        let html_body = item
            .body
            .as_ref()
            .and_then(|b| b.export_view.as_ref())
            .map(|ev| ev.value.clone())
            .unwrap_or_default();

        // Wrap with a tiny HTML shell so downstream text extractors get a
        // proper document. We keep the source bytes as text/html; the
        // existing TextExtractor passes it through and the markdown
        // extractor (and the LLM distiller) can read it.
        let title = item.title.unwrap_or_else(|| doc.name.clone());
        let html = format!(
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title></head><body>{}</body></html>",
            escape_html(&title), html_body
        );

        Ok(Downloaded {
            bytes: bytes::Bytes::from(html.into_bytes()),
            mime: "text/html".into(),
            filename: doc.name.clone(),
        })
    }
}

fn urlencoding(s: &str) -> String {
    // Tiny URL-encoder for path-segment-safe values.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '-' })
        .collect();
    cleaned.trim().replace(' ', "-").chars().take(200).collect()
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

// ---------- Confluence wire types (only the fields we touch) ----------

#[derive(Debug, Deserialize)]
struct ContentList {
    results: Vec<ContentItem>,
}

#[derive(Debug, Deserialize)]
struct ContentItem {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    version: Option<ContentVersion>,
    #[serde(default)]
    body: Option<ContentBody>,
}

#[derive(Debug, Deserialize)]
struct ContentVersion {
    #[serde(default)]
    when: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ContentBody {
    #[serde(default)]
    export_view: Option<ContentBodyView>,
}

#[derive(Debug, Deserialize)]
struct ContentBodyView {
    #[serde(default)]
    value: String,
}
