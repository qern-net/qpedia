# Storage model: serve-to vs index-in-place

> Status: **design** (2026-05-30). Decision captured; near-term work is
> one additive migration (§5). The bigger reference/zero-copy fork is
> staged, not committed. Companion to [`DESIGN.md`](DESIGN.md) §5 and
> [`ROADMAP.md`](ROADMAP.md) Band 5.

## 0. TL;DR

"Do documents need to be *served to* Qpedia, or can we *point at a folder
and index in place*?" collapses into two **independent** axes:

| Axis | Options | Where we are today |
|---|---|---|
| **Trigger** | push (upload) ↔ pull (connector / watch) | both exist |
| **Ownership** | copy-and-own ↔ reference-and-cache | always copy-and-own |

We already have point-at-a-folder ingestion — the connector path. The
real decision is the *ownership* axis, and the answer is **not binary**:

1. **Keep upload.** It's the right onboarding/ad-hoc/self-hosted path.
2. **Make connectors/point-at-folder the primary enterprise path.** Nobody
   hand-uploads 50k files.
3. **Do now (cheap, additive):** persist an **origin back-reference** on
   `sources`. This is the thing that ossifies if we wait.
4. **Reframe blob storage as a *cache*, not the system of record** — for
   connector-sourced docs. Extracted text + embeddings + the wiki are the
   product; they're kept regardless and are small.
5. **Build a `localfs` zero-copy connector** as the concrete "index in
   situ" mode for self-hosted (true zero-copy, originals served from the
   mount).
6. **Defer** source-ACL passthrough — it's a separate, hard band.

## 1. What is actually true in the code today

- **Upload (push):** `POST /sources` → hash → `BlobStore.put(Original)` →
  `sources` row → enqueue ingest. Blob = system of record.
- **Connector (pull):** `handlers/sync.rs` enumerates a remote folder
  (`Connector::list_changed`), and for each doc `download()`s the **full
  bytes**, hashes them, mints a slug, inserts a `Source`, and
  `blob.put(Original, …)` — i.e. **pull-and-copy**, identical to an
  upload once landed.
- **The remote linkage is discarded.** `RemoteDoc` carries `remote_id` +
  `modified_at`, but the `sources` row stores **none of it** — no
  `connector_id`, no `remote_id`, no `etag`/version. After ingest a Drive
  file is indistinguishable from an upload.
- **Change detection** leans on a connector-level opaque `cursor` plus
  `sha256` dedup — not a per-source backpointer. We **cannot** re-fetch a
  specific original from the source, because we never recorded where it
  came from.
- **Downloads** (`sourceOriginalUrl`, the citation "↓" links) serve from
  the blob copy.
- **The wiki + embeddings are derived artifacts** in Postgres + the
  per-tenant git repo. They're kept no matter what; the copy/reference
  debate only touches **raw originals + the extracted-text cache**.

## 2. The narrowing insight

Because the derived artifacts (wiki pages, embeddings, extracted text) are
small, owned, and the actual value-add, the only thing "stop copying"
touches is **the raw original bytes**. So the question shrinks to:

> For a connector/in-place source, do we **retain the original as record**,
> or **reference the source and cache the bytes** for performance?

## 3. Why the trajectory favors point-at-folder

The enterprise pitch is "connect your Drive / SharePoint / Confluence →
get a living wiki." That is inherently pull/in-place. Upload remains the
convenience path (one-offs, the drag-a-folder onboarding, files not in a
connected system). Connectors are the primary path and are already
underway (Confluence ✅, GDrive ✅, SharePoint stub).

## 4. Two distinct "in situ" stories — don't conflate them

1. **Self-hosted: `localfs` connector over a bind-mounted folder.** The
   *cleanest* in-place mode. Qpedia reads for extraction, stores derived
   artifacts, and **serves originals straight from the mount** — the mount
   *is* the store, so originals are always present, no re-fetch, no auth.
   True zero-copy. Cheap to build; great OSS story. We don't have this
   connector yet — highest-leverage in-place move.
2. **Cloud (Drive / SharePoint): reference + cache.** We **cannot** avoid
   fetching bytes to extract — "in situ" here only changes whether we
   *retain* them as record. So: reference the source + cache the bytes,
   gated on the origin linkage in §5.

## 5. The one near-term decision: persist origin linkage (additive)

Add to `sources` (additive, non-breaking; defaults make existing rows
`uploaded`):

```sql
ALTER TABLE sources
  ADD COLUMN origin          text NOT NULL DEFAULT 'uploaded',  -- uploaded | connector | localfs
  ADD COLUMN connector_id    text,            -- FK-ish to connectors.id, null for uploads
  ADD COLUMN remote_id       text,            -- opaque per connector (Drive fileId, path, …)
  ADD COLUMN remote_version  text;            -- etag / mtime / revision for change detection
-- precise incremental sync + dedup across re-enumeration:
CREATE UNIQUE INDEX sources_origin_remote
  ON sources (tenant_id, connector_id, remote_id)
  WHERE remote_id IS NOT NULL;
```

This unlocks, **with no commitment to dropping copies yet**:

- **Precise incremental sync.** Re-ingest the file whose `remote_version`
  changed, instead of re-enumerate-and-hope-sha-dedup-catches-it. Feeds
  the existing replace-in-place path.
- **Deletion propagation.** Gone at source → tombstone the `Source` and
  its derived pages (policy-gated; see §7).
- **The prerequisite for ever dereferencing.** You can't fetch-on-demand
  what you never recorded the address of.

`sync.rs::ingest_one` already has `doc.remote_id` in hand — it just needs
to write it onto the `Source` instead of discarding it.

## 6. Serving originals: cache-or-refetch

Once origin linkage exists, the download endpoint generalizes:

- `origin = uploaded`  → serve from blob (today's behavior; blob *is*
  record).
- `origin = localfs`   → stream from the mounted path (zero-copy).
- `origin = connector` → serve from blob **cache** if present; otherwise
  **re-fetch** via the connector and re-populate the cache.

That lets blob storage become an **evictable performance/availability
cache** for connector sources (TTL or LRU), rather than the retainer of
record — without ever breaking a download link. Re-OCR is avoided because
**extracted text is always kept** (it's tiny vs. the original).

## 7. Change & deletion propagation

- **Changed** (`remote_version` differs): enqueue a re-ingest keyed on the
  existing slug — exactly the replace-in-place cascade.
- **Deleted at source:** tombstone (`status = removed`) + run the existing
  remove cascade on derived pages. Whether to *also* purge the cached
  original is a **retention policy** (default: purge connector cache on
  source deletion; keep uploads until explicitly deleted).

## 8. Honest caveats (don't oversell "we never copy")

- **The derived wiki is itself retained, transformed data.** It
  summarizes and quotes sources; embeddings encode them. The defensible
  claim is **"we don't retain duplicate *originals* as a system of
  record,"** not "we never copy." Get that framing right before it goes
  on a website.
- **Source-ACL passthrough is a separate hard problem.** "Index in place
  and defer to Drive/SharePoint ACLs at query time" is the holy-grail
  enterprise ask — and Glean-class hard, made worse because a derived
  wiki page **blends facts from many differently-permissioned sources**.
  For now keep Qpedia's own folder-ACL + tenant isolation; do **not**
  promise source-ACL passthrough. Own band when a customer demands it.
- **Availability coupling.** Pure reference (no cache) couples every
  download to source reachability + valid creds. The §6 cache is the
  mitigation; don't go cache-less.

## 9. Staged plan (see ROADMAP Band 5)

| Stage | Item | Risk |
|---|---|---|
| S0 | **Origin-linkage migration** + populate from `sync.rs`. | low (additive) |
| S1 | **Incremental sync** on `remote_version`; **deletion → tombstone**. | low |
| S2 | **Blob-as-cache** for `origin=connector`: download = serve-or-refetch; make the original cache evictable. | med |
| S3 | **`localfs` zero-copy connector** (bind-mount; serve originals from path; watch for changes). | med |
| S4 | **Source-ACL passthrough** (deferred; own band, customer-driven). | high |

## 10. Related: extraction coverage

In-place indexing is only as good as what we can extract. Images, HTML
distillation, and archive (zip) expansion are tracked separately in
[`ROADMAP.md`](ROADMAP.md) Band 6 — they're an extraction-subsystem
concern (`qpedia-extract`), orthogonal to the storage-ownership decision
here.
