//! Archive (zip) expansion handler — ROADMAP Band 6.4.
//!
//! A `.zip` source isn't a document; it's a container. Instead of running it
//! through `extract_phase` (which would have no extractor and mark it
//! `tainted`), we **expand** it: create a locked folder named after the
//! archive and fan out one new Source + ingest job per entry, mirroring the
//! archive's internal directory structure. Each entry then flows through the
//! normal pipeline on its own (a PDF distills, an image gets metadata, an
//! unsupported type degrades to `tainted`, a nested zip expands again).
//!
//! This is the server-side analogue of the client's "mirror upload" — the
//! folders it creates are pinned so the AI auto-organizer leaves them alone.
//!
//! Guards (zip-bomb / zip-slip):
//! - skip entries whose path escapes the target (`enclosed_name` is None),
//! - skip encrypted entries (we can't read them),
//! - cap entry count, per-entry size, and total uncompressed size.

use crate::runner::{ingest_job, IngestContext};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use qpedia_core::{
    source::{Source, SourceStatus},
    tenant::Tenant,
    SourceId,
};
use qpedia_pg_store::{slugify_folder, unique_source_slug};
use qpedia_store::blob::{BlobKind, BlobStorage};
use sha2::{Digest, Sha256};
use std::io::Read;
use tracing::{info, warn};

const MAX_ENTRIES: usize = 2000;
const MAX_TOTAL_UNCOMPRESSED: u64 = 1024 * 1024 * 1024; // 1 GiB — zip-bomb guard
const MAX_ENTRY_BYTES: u64 = 200 * 1024 * 1024; // 200 MiB per file

/// Mimes routed to archive expansion instead of text extraction.
pub fn is_archive_mime(mime: &str) -> bool {
    matches!(
        mime,
        "application/zip" | "application/x-zip-compressed" | "application/x-zip"
    )
}

/// One safe, decompressed entry: relative path (forward-slashed) + bytes.
struct ZipEntry {
    rel: String,
    bytes: Vec<u8>,
}

/// Synchronously decompress a zip into owned entries, applying the
/// zip-slip / zip-bomb guards. Runs inside `spawn_blocking` so the `!Send`
/// zip reader never touches the async runtime. Returns
/// (entries, total_entry_count, skip_notes).
fn read_entries(data: bytes::Bytes) -> Result<(Vec<ZipEntry>, usize, Vec<String>)> {
    let reader = std::io::Cursor::new(data);
    let mut zip = zip::ZipArchive::new(reader).context("open zip archive")?;
    let n = zip.len();
    if n > MAX_ENTRIES {
        return Err(anyhow!("archive has {n} entries, exceeds cap of {MAX_ENTRIES}"));
    }

    let mut out: Vec<ZipEntry> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut total: u64 = 0;

    for i in 0..n {
        let mut entry = zip.by_index(i).context("read zip entry")?;
        let name = entry.name().to_string();
        if entry.is_dir() {
            continue;
        }
        if entry.encrypted() {
            notes.push(format!("skipped encrypted: {name}"));
            continue;
        }
        // zip-slip guard: None for path-traversal / absolute paths.
        let safe = match entry.enclosed_name() {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                notes.push(format!("skipped unsafe path: {name}"));
                continue;
            }
        };
        let size = entry.size();
        if size > MAX_ENTRY_BYTES {
            notes.push(format!("skipped oversize ({size} bytes): {name}"));
            continue;
        }
        total += size;
        if total > MAX_TOTAL_UNCOMPRESSED {
            return Err(anyhow!(
                "archive uncompressed size exceeds {MAX_TOTAL_UNCOMPRESSED} bytes (zip-bomb guard)"
            ));
        }
        let mut buf = Vec::with_capacity(size as usize);
        entry.read_to_end(&mut buf).context("read entry bytes")?;
        let rel = safe.to_string_lossy().replace('\\', "/");
        out.push(ZipEntry { rel, bytes: buf });
    }
    Ok((out, n, notes))
}

pub async fn expand(
    ctx: &IngestContext,
    tenant: &Tenant,
    source_id: &SourceId,
    src: &Source,
) -> Result<()> {
    ctx.db
        .update_status(tenant, source_id, SourceStatus::Extracting)
        .await?;

    let zip_ext = std::path::Path::new(&src.filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("zip");
    let bytes = ctx.blob.get(source_id, BlobKind::Original, zip_ext).await?;

    // Locked folder named after the archive (slugified → e.g. "foo.zip" →
    // "foo-zip"), placed beside the archive in the tree.
    let base = std::path::Path::new(&src.filename)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&src.filename);
    let raw_folder = if src.folder_path == "/" {
        format!("/{base}")
    } else {
        format!("{}/{}", src.folder_path, base)
    };
    let root_folder = slugify_folder(&raw_folder);
    ctx.db
        .create_folder(tenant, &root_folder, true, "qpedia-bot")
        .await?;

    // Decompress synchronously off the async runtime. The `zip` reader holds
    // a `dyn Read` (`!Send`), so it must never cross an `.await`; we pull out
    // owned (path, bytes) entries here and do all DB/blob work afterwards.
    let (entries, n_total, notes) =
        tokio::task::spawn_blocking(move || read_entries(bytes))
            .await
            .context("zip decompress task")??;

    let mut created = 0usize;
    for e in entries {
        // Mirror the entry's internal directory structure under root_folder.
        let (parent_rel, fname) = match e.rel.rsplit_once('/') {
            Some((p, f)) => (p.to_string(), f.to_string()),
            None => (String::new(), e.rel.clone()),
        };
        let entry_folder = if parent_rel.is_empty() {
            root_folder.clone()
        } else {
            let f = slugify_folder(&format!("{root_folder}/{parent_rel}"));
            ctx.db.create_folder(tenant, &f, true, "qpedia-bot").await?;
            f
        };

        let mime = mime_guess::from_path(&fname)
            .first_or_octet_stream()
            .essence_str()
            .to_string();
        let ext = std::path::Path::new(&fname)
            .extension()
            .and_then(|x| x.to_str())
            .unwrap_or("bin")
            .to_string();
        let sha256 = {
            let mut h = Sha256::new();
            h.update(&e.bytes);
            hex::encode(h.finalize())
        };

        let slug = unique_source_slug(&ctx.db, tenant, &fname).await?;
        let child_id = SourceId::from(slug);
        let child = Source {
            id: child_id.clone(),
            tenant: tenant.clone(),
            folder_path: entry_folder,
            filename: fname,
            mime,
            sha256,
            size_bytes: e.bytes.len() as u64,
            acl: src.acl.clone(),
            status: SourceStatus::Pending,
            language: None,
            created_at: Utc::now(),
            ingested_at: None,
            classification: None,
        };
        ctx.db.insert_source(&child).await?;
        ctx.blob
            .put(&child_id, BlobKind::Original, &ext, bytes::Bytes::from(e.bytes))
            .await?;
        ctx.db.enqueue(tenant, &ingest_job(tenant, &child_id)?).await?;
        created += 1;
    }
    let n = n_total;
    let skipped = notes.len();

    // The container is fully processed: it has no wiki page of its own (its
    // children carry the content), so mark it Done with a manifest note.
    let manifest = serde_json::json!({
        "archive": src.filename,
        "folder": root_folder,
        "entries_total": n,
        "ingested": created,
        "skipped": skipped,
        "notes": notes,
    });
    ctx.blob
        .put_text(
            source_id,
            BlobKind::Manifest,
            &serde_json::to_string_pretty(&manifest)?,
        )
        .await?;
    ctx.db
        .update_status(tenant, source_id, SourceStatus::Done)
        .await?;
    ctx.db
        .write_audit(
            tenant,
            "qpedia-bot",
            "archive.expanded",
            Some(source_id.as_str()),
            Some(&manifest),
        )
        .await?;
    if skipped > 0 {
        warn!(id = %source_id, created, skipped, "archive expanded with skipped entries");
    } else {
        info!(id = %source_id, created, folder = %root_folder, "archive expanded");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_mime_routing() {
        assert!(is_archive_mime("application/zip"));
        assert!(is_archive_mime("application/x-zip-compressed"));
        assert!(!is_archive_mime("application/pdf"));
        assert!(!is_archive_mime("image/jpeg"));
        assert!(!is_archive_mime("application/epub+zip")); // pandoc handles epub
    }
}
