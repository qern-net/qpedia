//! Google Drive connector. Enumerates and downloads files from a Drive,
//! feeding them through the normal qpedia ingest path. SSO-aligned: it
//! authenticates with an OAuth refresh token (the durable credential the
//! authorization-code flow produces — see `oauth.rs` and the
//! `/api/v1/connectors/google/*` endpoints), refreshing the short-lived
//! access token on demand.
//!
//! Config shape (`config_json`):
//!   {
//!     "client_id":      "<google oauth client id>",
//!     "client_secret":  "<google oauth client secret>",
//!     "refresh_token":  "<offline-access refresh token>",
//!     "folder_id":      "<optional: restrict to this Drive folder>",
//!     "include_shared": false
//!   }
//!
//! The refresh token can come from the Connect-Google-Drive flow (which
//! writes it here and records an `oauth_grants` row for audit/revocation)
//! or be supplied out-of-band (e.g. the Google OAuth Playground) for
//! self-hosters not running the SSO flow — mirroring how Confluence
//! takes an API token directly.
//!
//! Cursor = the most-recent `modifiedTime` seen (RFC 3339), so syncs
//! resume incrementally.

use crate::oauth;
use crate::{Connector, Downloaded, ListChanged, RemoteDoc};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use std::time::Duration;
use tokio::sync::Mutex;

const FILES_ENDPOINT: &str = "https://www.googleapis.com/drive/v3/files";
const PAGE_SIZE: usize = 100;

#[derive(Debug, Clone, Deserialize)]
struct GDriveConfig {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    #[serde(default)]
    folder_id: Option<String>,
    #[serde(default)]
    include_shared: bool,
}

struct CachedToken {
    access_token: String,
    expires_at: DateTime<Utc>,
}

pub struct GoogleDriveConnector {
    cfg: GDriveConfig,
    client: reqwest::Client,
    token: Mutex<Option<CachedToken>>,
}

impl GoogleDriveConnector {
    pub fn from_config(value: &serde_json::Value) -> Result<Self> {
        let cfg: GDriveConfig = serde_json::from_value(value.clone()).context(
            "gdrive config: expected {client_id, client_secret, refresh_token, folder_id?, include_shared?}",
        )?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .context("build gdrive http client")?;
        Ok(Self { cfg, client, token: Mutex::new(None) })
    }

    /// Return a valid access token, refreshing if the cached one is
    /// missing or within the skew window of expiry.
    async fn access_token(&self) -> Result<String> {
        let mut guard = self.token.lock().await;
        let still_valid = guard
            .as_ref()
            .map(|t| t.expires_at > Utc::now())
            .unwrap_or(false);
        if !still_valid {
            let tok = oauth::refresh(
                &self.client,
                &self.cfg.client_id,
                &self.cfg.client_secret,
                &self.cfg.refresh_token,
            )
            .await
            .context("refresh google access token")?;
            *guard = Some(CachedToken {
                access_token: tok.access_token,
                expires_at: tok.expires_at,
            });
        }
        Ok(guard.as_ref().unwrap().access_token.clone())
    }

    /// Build the `q` filter for files.list.
    fn list_query(&self, since: Option<&str>) -> String {
        let mut clauses = vec![
            "trashed = false".to_string(),
            "mimeType != 'application/vnd.google-apps.folder'".to_string(),
        ];
        if let Some(folder) = &self.cfg.folder_id {
            clauses.push(format!("'{}' in parents", folder.replace('\'', "")));
        }
        if let Some(ts) = since {
            // Drive wants RFC 3339 in single quotes.
            clauses.push(format!("modifiedTime > '{}'", ts.replace('\'', "")));
        }
        clauses.join(" and ")
    }
}

#[async_trait]
impl Connector for GoogleDriveConnector {
    fn kind(&self) -> &'static str {
        "gdrive"
    }

    async fn list_changed(&self, cursor: Option<&str>) -> Result<ListChanged> {
        let token = self.access_token().await?;
        let q = self.list_query(cursor);

        let mut docs = Vec::new();
        let mut max_seen: Option<DateTime<Utc>> = None;
        let mut page_token: Option<String> = None;

        loop {
            let mut params: Vec<(String, String)> = vec![
                ("q".into(), q.clone()),
                (
                    "fields".into(),
                    "nextPageToken,files(id,name,mimeType,modifiedTime,size)".into(),
                ),
                ("pageSize".into(), PAGE_SIZE.to_string()),
                ("orderBy".into(), "modifiedTime".into()),
            ];
            if self.cfg.include_shared {
                params.push(("supportsAllDrives".into(), "true".into()));
                params.push(("includeItemsFromAllDrives".into(), "true".into()));
                params.push(("corpora".into(), "allDrives".into()));
            }
            if let Some(pt) = &page_token {
                params.push(("pageToken".into(), pt.clone()));
            }

            let resp = self
                .client
                .get(FILES_ENDPOINT)
                .bearer_auth(&token)
                .query(&params)
                .send()
                .await?;
            let status = resp.status();
            let text = resp.text().await?;
            if !status.is_success() {
                return Err(anyhow!("gdrive list ({status}): {text}"));
            }
            let page: FileList = serde_json::from_str(&text)
                .map_err(|e| anyhow!("decode gdrive list: {e}; body: {text}"))?;

            for f in page.files {
                let when = f
                    .modified_time
                    .as_deref()
                    .and_then(|w| DateTime::parse_from_rfc3339(w).ok())
                    .map(|w| w.with_timezone(&Utc));
                if let Some(w) = when {
                    max_seen = Some(max_seen.map_or(w, |m| m.max(w)));
                }
                let (name, mime) = export_target(&f);
                docs.push(RemoteDoc {
                    remote_id: f.id,
                    name,
                    mime,
                    modified_at: when,
                    size_bytes: f.size.and_then(|s| s.parse::<u64>().ok()),
                });
            }

            match page.next_page_token {
                Some(t) => page_token = Some(t),
                None => break,
            }
        }

        Ok(ListChanged {
            docs,
            new_cursor: max_seen
                .map(|d| d.to_rfc3339())
                .or_else(|| cursor.map(|s| s.to_string())),
        })
    }

    async fn download(&self, doc: &RemoteDoc) -> Result<Downloaded> {
        let token = self.access_token().await?;

        // Google-native docs must be exported to a concrete format;
        // everything else is a direct media download.
        let url = if let Some(export_mime) = native_export_mime(&doc.mime) {
            format!(
                "{FILES_ENDPOINT}/{}/export?mimeType={}",
                doc.remote_id,
                urlencode(export_mime)
            )
        } else {
            format!(
                "{FILES_ENDPOINT}/{}?alt=media&supportsAllDrives=true",
                doc.remote_id
            )
        };

        let resp = self.client.get(&url).bearer_auth(&token).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("gdrive download ({status}): {text}"));
        }
        let bytes = resp.bytes().await.context("read gdrive body")?;

        // The mime we want downstream is the export target for native
        // docs, or the file's own mime otherwise.
        let out_mime = native_export_mime(&doc.mime)
            .map(|m| m.to_string())
            .unwrap_or_else(|| doc.mime.clone());

        Ok(Downloaded {
            bytes,
            mime: out_mime,
            filename: doc.name.clone(),
        })
    }
}

/// For a listed file decide the (filename, mime) we present to qpedia.
/// Google-native types get a synthetic extension matching their export
/// format so the right extractor runs; regular files keep their name +
/// mime.
fn export_target(f: &DriveFile) -> (String, String) {
    let base = sanitize_filename(f.name.as_deref().unwrap_or(&f.id));
    match native_export_mime(&f.mime_type) {
        Some("text/html") => (ensure_ext(&base, "html"), "text/html".into()),
        Some("text/csv") => (ensure_ext(&base, "csv"), "text/csv".into()),
        Some("text/plain") => (ensure_ext(&base, "txt"), "text/plain".into()),
        Some(other) => (base, other.to_string()),
        None => (base, f.mime_type.clone()),
    }
}

/// Map a Google-native mime to the format we export it as. Returns None
/// for ordinary binary files (downloaded directly).
fn native_export_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "application/vnd.google-apps.document" => Some("text/html"),
        "application/vnd.google-apps.spreadsheet" => Some("text/csv"),
        "application/vnd.google-apps.presentation" => Some("text/plain"),
        // Drawings, forms, etc. aren't usefully ingestable as text; skip
        // by treating them as plain text export (usually empty/minimal).
        m if m.starts_with("application/vnd.google-apps.") => Some("text/plain"),
        _ => None,
    }
}

fn ensure_ext(name: &str, ext: &str) -> String {
    if name.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
        name.to_string()
    } else {
        format!("{name}.{ext}")
    }
}

fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    cleaned.trim().replace(' ', "-").chars().take(200).collect()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------- Drive wire types (only the fields we read) ----------

#[derive(Debug, Deserialize)]
struct FileList {
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
    #[serde(default)]
    files: Vec<DriveFile>,
}

#[derive(Debug, Deserialize)]
struct DriveFile {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(rename = "mimeType")]
    mime_type: String,
    #[serde(default, rename = "modifiedTime")]
    modified_time: Option<String>,
    #[serde(default)]
    size: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_docs_get_exported() {
        assert_eq!(
            native_export_mime("application/vnd.google-apps.document"),
            Some("text/html")
        );
        assert_eq!(
            native_export_mime("application/vnd.google-apps.spreadsheet"),
            Some("text/csv")
        );
        assert_eq!(native_export_mime("application/pdf"), None);
    }

    #[test]
    fn export_target_appends_extension() {
        let f = DriveFile {
            id: "x".into(),
            name: Some("Quarterly Review".into()),
            mime_type: "application/vnd.google-apps.document".into(),
            modified_time: None,
            size: None,
        };
        let (name, mime) = export_target(&f);
        assert_eq!(name, "Quarterly-Review.html");
        assert_eq!(mime, "text/html");
    }

    #[test]
    fn binary_files_keep_name_and_mime() {
        let f = DriveFile {
            id: "y".into(),
            name: Some("report.pdf".into()),
            mime_type: "application/pdf".into(),
            modified_time: None,
            size: Some("1024".into()),
        };
        let (name, mime) = export_target(&f);
        assert_eq!(name, "report.pdf");
        assert_eq!(mime, "application/pdf");
    }

    #[test]
    fn list_query_includes_folder_and_since() {
        let cfg = GDriveConfig {
            client_id: "c".into(),
            client_secret: "s".into(),
            refresh_token: "r".into(),
            folder_id: Some("FOLDER1".into()),
            include_shared: false,
        };
        let conn = GoogleDriveConnector {
            cfg,
            client: reqwest::Client::new(),
            token: Mutex::new(None),
        };
        let q = conn.list_query(Some("2026-01-01T00:00:00Z"));
        assert!(q.contains("trashed = false"));
        assert!(q.contains("'FOLDER1' in parents"));
        assert!(q.contains("modifiedTime > '2026-01-01T00:00:00Z'"));
    }
}
