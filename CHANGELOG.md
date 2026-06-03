# Changelog

All notable changes to **qpedia** (the OSS engine). Format: [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/), versioning:
[SemVer](https://semver.org/spec/v2.0.0.html).

## [1.2.0] — 2026-06-02

The consolidated public release of the engine: Postgres + pgvector
storage with RLS tenant isolation, an idempotent ingest pipeline, hybrid
search, an agentic chat retriever, broad extraction coverage, Firebase
auth with individual + invite-based org workspaces, and a connector
framework.

### Added

#### Extraction

- **HTML distillation.** HTML files ingest as clean, readable Markdown
  instead of raw tag soup. A new `HtmlExtractor` (registered ahead of the
  plain-text path, which would otherwise claim `text/html`) does a
  readability pass with `scraper` — selecting the main content container
  (`<article>`/`<main>`/common content ids+classes) and dropping
  nav/header/footer chrome — then converts that subtree to GitHub-flavoured
  Markdown with pandoc (`-native_divs-native_spans`, which also drops
  `<div>`/`<span>` wrappers and `<script>`/`<style>`). Falls back to the
  full document when no content container is found. Container selection is
  unit-tested.

- **Image OCR + vision description.** Images are no longer indexed by
  metadata alone — a vision-capable LLM reads them: text images
  (scans/screenshots/slides/tables) are transcribed; photos, diagrams,
  charts and maps are described in words; mixed content gets both. That
  text becomes the wiki page content (format/dimensions metadata stays as a
  trailer), so images are classified and searched by what they actually
  contain. New one-shot `LlmProvider::vision` (OpenAI chat-completions
  multimodal shape, base64 data URL) implemented for the OpenAI-compatible
  provider; the image branch of `extract_phase` calls it. Auto-enables for
  vision-capable providers (`gpt-4.1-mini` &c.); `QPEDIA_VISION_MODEL`
  overrides the model, `QPEDIA_VISION=0` disables it, and any error falls
  back to the metadata floor. Verified live: an Urdu names-list PNG was
  OCR'd (RTL preserved) and correctly classified.

- **Image metadata floor.** An `ImageExtractor` registers `image/*`, so
  uploads like `.jpg`/`.png`/`.jfif` that previously failed the job are
  indexed by metadata (format + pixel dimensions + byte size) and flow
  through the pipeline. Header-only read via `imagesize` (no full decode),
  graceful on unreadable bytes.

- **Audio/video metadata.** A `MediaExtractor` registers `audio/*` and
  `video/*` so media files stop dead-lettering: it indexes the container
  format, byte size, and — best-effort via `lofty` — duration and any
  title/artist tags. Speech-to-text transcription remains future work.

- **Zip ingestion.** A `.zip` source expands into a **locked folder named
  after the archive** (slugified, e.g. `foo.zip` → `foo-zip`), fanning out
  one child source + ingest job per entry and mirroring the archive's
  internal directory structure. Each child then flows through the normal
  pipeline (a PDF distills, a nested zip expands again, etc.). Guards
  against zip-slip (`enclosed_name`), encrypted entries, and zip-bombs
  (caps on entry count, per-entry size, and total uncompressed size);
  decompression runs on a blocking thread so the `!Send` zip reader never
  touches the async runtime. The container is marked `done` with an
  `archive.expanded` manifest.

#### Wiki + retrieval

- **Deeper wiki taxonomy.** The wiki agent organizes pages into a shallow,
  navigable hierarchy — `concepts/<category>/`, `entities/<type>/`
  (people/organizations/places/products/systems/works), `topics/<area>`
  hub pages that link down into a subject, and a hierarchical `index.md` —
  instead of four flat folders. Guardrails keep it from over-fragmenting:
  reuse existing categories, never nest deeper than category+page, and
  split pages by topic coherence. Compiled into the agent prompt, so it
  applies to all tenants on the next ingest; existing pages keep their flat
  paths until re-ingested. New-wiki seed (`QPEDIA.md`/`index.md`) updated to
  match.

- **Inline wiki citations now render.** The wiki agent emits
  `[^src:<source-id>]` markers to tie a fact to its source. The renderer
  previously only rewrote `[[wiki links]]`, so the inline markers leaked as
  raw text. They now render as numbered superscripts (first-appearance
  order; a repeated source reuses its number) that link to a **Sources
  cited** list at the foot of the page; each entry resolves to the source's
  filename and links to its original-file download.

- **Right-to-left (RTL) wiki rendering.** Wiki pages detect their dominant
  script (Arabic, Urdu, Farsi, Pashto, Hebrew, Syriac, Thaana, NKo, …) and
  set the container base direction accordingly; every block also carries
  `dir="auto"` so a *mixed* page self-orients per block. Markdown CSS uses
  logical properties so list indents and blockquote rules mirror correctly
  under RTL; code blocks are pinned LTR. Search-result titles/snippets also
  self-orient. No backend change.

- **List pagination.** The Sources file table paginates client-side
  (50/page, prev/next) — the folder tree still gets every row for its
  rollup counts. The wiki list caps each bucket at 50 with a per-bucket
  "show all" expander.

#### Auth + workspaces

- **Firebase / Google sign-in, enforced.** New `AuthMode::Session` — a
  session-cookie-gated mode that doesn't require a full OIDC issuer.
  Auto-selected when `QPEDIA_FIREBASE_PROJECT_ID` is set with no OIDC issuer
  (or `QPEDIA_AUTH_MODE=firebase`). Previously a Firebase login minted a
  session the default Dev-mode `User` extractor ignored, so sign-in was
  cosmetic; now the cookie is enforced on every request.
  - **Admin bootstrap:** `QPEDIA_ADMIN_EMAILS` (comma/space allowlist) — a
    login whose verified email matches gets the `admin` group, so the first
    operator can administer a fresh deployment before any Firebase custom
    claims exist.
  - **`/login` is the universal front door.** New public
    `GET /api/v1/auth/config` returns `{ mode, firebase }`; `/login`
    renders the right UI per mode — Firebase provider buttons, an OIDC
    "Continue with SSO" button, or a dev-mode notice.

- **Team / org workspaces, invite-only.** A user can create an
  **organization** workspace (they become its owner) and invite teammates
  by email; the invitee accepts via a tokened link and joins with the
  chosen role. A **workspace switcher** lists every workspace you belong to
  and switches the active one. The Admin tab gains a **Members & Invites**
  panel (invite, list/revoke pending invites, list/remove members;
  last-owner removal is refused). Joining a workspace other than your own
  individual one is *only* by org creation or invite — no domain/SSO
  surface, so zero takeover risk at this stage. New `workspace_members` +
  `workspace_invites` tables (migration 0006, RLS-isolated); endpoints under
  `/api/v1/workspaces` and `/api/v1/invites/:token`; accept page at
  `/invite/<token>`.

- **Workspace domain ownership + DNS-TXT verification.** New
  `workspace_domains` table (migration 0007, RLS-isolated) with a partial
  unique index so a domain can be *claimed* by many workspaces but
  *verified* by only one — enforced at the storage layer, so a second
  workspace's verify fails closed even under RLS row-hiding. An org admin
  adds a domain → gets a `qpedia-verify=<token>` TXT record to place in DNS
  → clicks **Verify**; the backend resolves the domain's TXT records via
  DNS-over-HTTPS (no extra DNS dependency) and, on a match, marks it
  verified. Admin → **Domains** panel; endpoints under
  `/api/v1/workspaces/domains`. PgStore: `claim_domain`, `verify_domain`,
  `domain_owner` (cross-tenant), `list_workspace_domains`, `delete_domain`.
  DoH parsing + domain normalization are unit-tested.

#### Connectors

- **Google Drive connector + OAuth foundation.** The second concrete
  connector after Confluence, plus shared credential plumbing.
  - New `oauth_grants` table (migration 0005, RLS tenant-isolated): durable
    `(tenant, provider, scope, subject)` → refresh-token mapping, with
    PgStore CRUD. `subject=''` is an org-level grant.
  - `qpedia-connectors::oauth` — Google OAuth 2.0 helper: `consent_url`
    (offline access), `exchange_code`, `refresh`.
  - `qpedia-connectors::gdrive` — `GoogleDriveConnector`: `list_changed`
    via Drive `files.list` (incremental on `modifiedTime`), `download` via
    `files.get?alt=media` with `files.export` for Google-native docs
    (Docs→HTML, Sheets→CSV, Slides→text). Registered as `"gdrive"`.
  - **Connect Google Drive** in Admin → Connectors: Google consent
    (read-only Drive) → callback exchanges the code for a refresh token,
    records the grant, and creates a `gdrive` connector the auto-sync
    scheduler picks up. New `GET /api/v1/connectors/google/{authorize,callback}`
    endpoints; `GOOGLE_OAUTH_CLIENT_ID` / `_SECRET` / `_REDIRECT_URL` env.
    Self-hosters can supply a refresh token in the connector config directly
    (mirrors Confluence's API-token path).

#### Operations

- **Visual processing queue.** The Admin tab has a live **Processing
  queue** panel (polls every 2s): counts by job state (queued / running /
  done / dead), the **running jobs grouped by worker** (kind · source ·
  running-for), the queued backlog, and an expandable list of recent dead
  jobs with their last error. Backed by an admin-only
  `GET /api/v1/admin/queue`.

- **Multi-worker job runner.** New `QPEDIA_WORKERS=N` env var (default `1`,
  clamped to `[1, 32]`) sets the size of the in-process ingest worker pool.
  Each worker has a distinct id (`worker-1`, …) so `jobs.locked_by` tells
  you which one holds a given lease. Concurrent claims are race-free because
  `claim_next_job` already used `SELECT … FOR UPDATE SKIP LOCKED LIMIT 1`.

- **Chat rate limiting.** `POST /api/v1/chat` is guarded by a per-tenant
  token bucket: default 30 requests/minute with a burst of 10, configurable
  via `QPEDIA_CHAT_RPM` / `QPEDIA_CHAT_BURST`. Once a tenant's bucket is
  empty the endpoint returns `429 Too Many Requests` with a `Retry-After`
  header and a JSON body carrying `retry_after_seconds`. The limiter is
  pluggable via `AppBuilder::with_chat_rate_limiter`.

- **Backup + restore scripts.** `scripts/backup.sh` captures the three
  durable stores in dependency order — Postgres (`pg_dump -Fc`), one
  `git bundle` per tenant wiki repo, and a tar of `/data/raw` — into a
  timestamped `./backups/<ts>/` with a manifest. `scripts/restore.sh`
  reverses it (raw → Postgres `pg_restore --clean` → `git clone` each
  tenant bundle), gated behind a confirmation. Both accept a direct DSN via
  `QPEDIA_PG_MODE=dsn`.

- **CI for migrations.** `.github/workflows/ci.yml` with two jobs: `check`
  (`cargo check --workspace --all-targets`) and `migrate` (spins up
  `pgvector/pgvector:pg17` as a service container, runs
  `cargo test --workspace --all-targets` against it). The migrate step
  exercises `crates/qpedia-pg-store/tests/smoke.rs`, which applies every
  migration to a fresh DB and runs a tenants → folders → folder-ACLs →
  sources → audit → wiki upsert + hybrid-search lifecycle, including a
  384-dim `vector(384)` round-trip. The smoke test gates on `QPEDIA_DB_URL`
  so local `cargo test` with no DB still passes.

- **Bulk ingest UX.** Drag an OS folder onto the upload panel, or pick one
  with **Upload folder (mirror)**: the OS subfolder structure is replicated
  under the selected folder as pinned folders (slugified server-side so
  `Q4 Reports` lands at `q4-reports`), and every file is uploaded into its
  mirrored location. The companion **Upload folder (AI organize)** button
  drops every file at `/` so the auto-organizer groups them into
  `/<doc_type>`. Either path reports live progress and a completion summary.

- **Source replace-in-place.** New `POST /api/v1/sources/:id/replace`
  multipart endpoint and a Replace button on each source row. Same slug,
  folder, and ACL are preserved; only the underlying bytes (and derived
  metadata) are overwritten, and the ingest pipeline re-runs from `Pending`.
  Existing wiki pages that reference the `source_id` are refreshed by the
  agent's `propose_patch` instead of being orphaned by a delete +
  re-upload. Identical bytes are a no-op. Audited as `source.replaced`.

### Changed

- **Every login starts in an isolated individual workspace** — including
  corporate-domain emails. Tenant resolution is: explicit `tenant_id` claim
  (set by the org/SSO flow) → else the user's individual `u-<uid>`; never
  the shared `default` tenant. The login also grants the user `admin` in
  their *own* individual workspace (RLS-scoped, never cross-tenant). Org/team
  is a separate, **domain-verified** flow.

### Fixed

- **Crashed-worker jobs are no longer orphaned in `running` forever.**
  `claim_next_job` only ever picked `state = 'queued'`, so the lease
  (`locked_until`) was written but never acted on — a worker that died
  mid-job left its in-flight jobs `running` indefinitely. The claim query
  now also reclaims `running` jobs whose lease has expired, and the lease
  was lengthened from 5 to 30 minutes so a legitimately slow job isn't
  reclaimed mid-run. Idempotent handlers make a reclaimed-but-partial job
  safe to re-run.

- **Genuine processing failures surface as `failed`.** When an ingest job
  exhausts its retries and goes `dead`, the source is marked `failed` with a
  `source.failed` audit note, instead of being stranded mid-pipeline. This
  complements `tainted` (unsupported/stopped): `failed` = a real error after
  retries.

- **Unsupported file types no longer hang as "extracting".** A source whose
  mime has no extractor previously returned a hard error, failing the ingest
  job and leaving the source stranded at `extracting`. It now degrades to a
  terminal `tainted` state with a `source.unsupported` audit note (the mime
  is recorded), and is re-drivable once an extractor for its type lands.

- **Firebase logins no longer fall back to the shared `default` tenant.**
  Previously a Firebase login with no `tenant_id` claim resolved to
  `default` — the same tenant dev/single-user ingestion writes to — so a
  freshly signed-in user saw pre-existing data. Tenant resolution now ends
  at an isolated individual tenant `u-<uid>`, never `default`. (RLS was
  always enforcing isolation; the bug was the resolver collapsing distinct
  users into one tenant.)

- **Missing logout button / no workspace indicator after login.** The
  `/login` page did a client-side navigation that didn't remount the root
  layout, so it never re-fetched the session. Login now does a full reload.
  The header shows a **Log out** button and a **workspace banner** that
  makes the active tenant + individual-vs-org mode unmistakable
  (`/api/v1/auth/me` now returns `tenant` + `tenant_kind`).

- **Bulk-upload progress is now visible.** Each folder-tree node shows a
  per-folder progress bar (`done/total`, rolled up over its subtree); the
  upload panel shows an overall progress bar during the POST loop and
  refreshes the tree every 10 files, so a 300+-file folder upload reports
  progress instead of looking frozen.

- **Workspace banner height jitter.** The banner and header are now
  fixed-height, single-line (`nowrap`, description ellipsizes first), so the
  bar height is stable across pages.

- **Lock indication on mirror uploads clarified** (no behavior change).
  Mirror/drag-folder uploads create *locked* (pinned) folders so the AI
  auto-organizer can't rearrange a hand-curated tree; the success toast,
  panel hint, and tree tooltip now say "🔒 locked" explicitly.

### Architecture (foundation)

- **Storage:** Postgres 17 + pgvector + tsvector. One database for every
  piece of structured state: tenants, sources, sessions, jobs, audit,
  folder ACLs, folders, connectors, oidc_pending, and the `wiki_pages`
  search index (vector + BM25 in one SQL query).
- **Tenant isolation:** Postgres Row Level Security policies. Every
  tenant-scoped query opens a transaction that does
  `SET LOCAL ROLE qpedia_app` + `set_config('qpedia.tenant', …)`; RLS
  rejects any cross-tenant read or write inside the tx, fail-closed.
- **Identifiers:** `BIGSERIAL` internal primary keys + tenant-unique
  Wikipedia-style slugs (`quarterly-revenue-model`) as public identifiers.
  Slug collisions resolve by appending `-2`, `-3`, … and probing Postgres.
- **Ingest pipeline:** Extract → Classify → Distill (agent) → Validate →
  Commit → Embed → Done. The ingest agent
  (`crates/qpedia-ingest/src/agent.rs`) is a bounded tool-using LLM loop
  (18 turns / 20 staged ops / 500 KB bundle / 50 KB per page) with tools
  `search_wiki`, `list_pages`, `read_page`, `read_source`, `propose_new`,
  `propose_patch`, `done`. A deterministic DiffBundle validator runs before
  every commit; the bundle lands as one git commit in the per-tenant wiki
  repo at `/data/wiki/<tenant>/`.
- **Wiki + retrieval:** per-tenant git repo of Markdown pages with YAML
  frontmatter. Hybrid search weights pgvector cosine similarity (HNSW) with
  `ts_rank_cd` keyword ranking (alpha = 0.7) in one SQL query. The agentic
  chat retriever (`crates/qpedia-retriever`) runs a bounded gather loop
  (`search_wiki` / `read_page` / `follow_links` / `done`) followed by
  streaming SSE synthesis, ACL-filtered at every tool call. The lint pass
  surfaces orphans, broken `[[wikilinks]]`, index drift, stale source IDs,
  near-duplicates (cosine ≥ 0.93 via pgvector self-join), and LLM-detected
  contradictions.
- **Auth + ACLs:** Firebase Auth federation (Google / Apple / Microsoft /
  GitHub / X / Facebook + enterprise OIDC SSO), verified via JWKS (RS256, no
  Admin SDK). Direct OIDC remains wired via `QPEDIA_OIDC_*`. Dev mode (no
  auth env): every request is `dev:admin`. ACLs are per-source, folder-level
  with closest-ancestor inheritance, and per-page as the union of source
  ACLs; the `admin` group passes everything.
- **Composable HTTP layer:** `qpedia_api::AppBuilder::from_env()` produces a
  composable application. Routes, typed state extensions, an `EventSink`
  (fires from every `db.write_audit` on a detached task after durable
  commit), a `TenantHook` (fires from `db.upsert_tenant`), and a chat rate
  limiter can all be registered before `.serve()`.
- **Embedded SPA:** `qpedia-api` serves a built SvelteKit SPA from
  `QPEDIA_WEB_DIR` with a SPA fallback, and runs API-only when no build is
  present.
- **Health check:** `GET /healthz` → `ok`.

### Crates

| Crate | Purpose |
|---|---|
| `qpedia-core` | Domain types: `Tenant`, `SourceId`, `Acl`, `Source`, `Job`, `DiffBundle`. |
| `qpedia-store` | Filesystem-only primitives: `BlobStore` (raw + extracted blobs), per-tenant `WikiRepoStore` (gix-backed). |
| `qpedia-pg-store` | Postgres + pgvector. All SQL, migrations, RLS plumbing, `EventSink` and `TenantHook` registries. |
| `qpedia-extract` | PDF / DOCX / HTML / image / media / zip extraction. Marker sidecar client. |
| `qpedia-llm` | LLM provider abstraction (Anthropic, OpenAI, OpenAI-compat, OpenRouter). |
| `qpedia-embed` | fastembed-rs wrapper (bge-small-en-v1.5). |
| `qpedia-connectors` | Connector trait + Confluence Cloud + Google Drive + OAuth helper. |
| `qpedia-ingest` | Job runner + phase handlers + the ingest agent + validator. |
| `qpedia-retriever` | Two-phase agentic chat retriever (gather + streaming synthesis). |
| `qpedia-lint` | Wiki lint pass (orphans, broken links, drift, duplicates, contradictions). |
| `qpedia-api` | Composable HTTP layer (`AppBuilder` lib + thin binary). |
| `qpedia-cli` | CLI smoke tests + local-dev helpers. |

### Known limitations

- English-only embedder + tsvector (`bge-small-en-v1.5`; `'english'`
  full-text config). Cross-language wiki support is future work.
- Speech-to-text transcription for audio/video is not yet implemented (only
  the metadata floor).
