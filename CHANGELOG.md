# Changelog

All notable changes to **qpedia** (the OSS engine). Format: [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/), versioning:
[SemVer](https://semver.org/spec/v2.0.0.html).

The private SaaS overlay `qpedia-pvt` ships its own changelog.

## [Unreleased]

### Added

- **Right-to-left (RTL) wiki rendering.** Wiki pages now detect their
  dominant script (Arabic, Urdu, Farsi, Pashto, Hebrew, Syriac, Thaana,
  NKo, …) and set the container base direction accordingly; every block
  (paragraph, heading, list item, table cell, blockquote) also carries
  `dir="auto"` so a *mixed* page self-orients per block — an Arabic quote
  inside an English page flows right-to-left while the page stays LTR, and
  vice-versa. Markdown CSS switched to logical properties so list indents
  and blockquote rules mirror correctly under RTL; code blocks are pinned
  LTR. Search-result titles/snippets also self-orient. No backend change.

- **Image files no longer dead-letter on ingest** (ROADMAP Band 6.0). A
  new `ImageExtractor` registers `image/*`, so uploads like `.jpg`/`.png`/
  `.jfif` that previously failed the job with `no extractor for mime:
  image/jpeg` are now indexed by metadata (format + pixel dimensions +
  byte size) and flow through the pipeline. Header-only read via
  `imagesize` (no full decode), graceful on unreadable bytes. OCR of image
  *contents* is staged as Band 6.1 (Marker/surya sidecar). Already-failed
  images are stuck at `extracting`, so **Admin → Resume stalled** re-drives
  them (or just re-upload).

- **`STORAGE-MODEL.md` + ROADMAP Bands 5 & 6.** Design note on serve-to vs
  index-in-place: keep upload, make connectors the primary path, persist
  an additive **origin back-reference** on `sources` (the one near-term
  schema change), demote blob storage to a *cache* for connector sources,
  and add a `localfs` zero-copy connector for self-hosted — with
  source-ACL passthrough explicitly deferred. Band 6 tracks extraction
  coverage (image OCR, HTML *distillation* file/tree/remote, zip→folder
  expansion, xlsx/email).

- **Inline wiki citations now render.** The wiki agent emits
  `[^src:<source-id>]` markers to tie a fact to the source it came from,
  but the page renderer only rewrote `[[wiki links]]` and showed the
  frontmatter `source_ids` as chips — so the inline markers leaked as raw
  text (`marked` has no footnote support). They now render as numbered
  superscripts (first-appearance order; a repeated source reuses its
  number) that link to a **Sources cited** list at the foot of the page;
  each entry resolves to the source's filename (best-effort via
  `getSource`, falling back to the slug) and links to its original-file
  download.

- **Domain verification — DNS-TXT method** (Band 4.2). An org admin adds
  a domain → gets a `qpedia-verify=<token>` TXT record to place in DNS →
  clicks **Verify**; the backend resolves the domain's TXT records via
  DNS-over-HTTPS (no extra DNS dependency) and, on a match, marks it
  verified. Admin → **Domains** panel (add / show TXT / verify / remove).
  Endpoints under `/api/v1/workspaces/domains`. A domain verified by one
  org cannot be verified by another (storage-level guarantee from 4.2's
  partial unique index). DoH parsing + domain normalization are
  unit-tested.

- **Workspace domain ownership foundation** (Band 4.2).
  New `workspace_domains` table (migration 0007, RLS-isolated) with a
  partial unique index so a domain can be *claimed* by many workspaces
  but *verified* by only one — enforced at the storage layer, so a
  second workspace's verify fails closed even under RLS row-hiding.
  PgStore: `claim_domain`, `verify_domain` (via = `dns` |
  `microsoft_entra` | `google_workspace` | `sso`), `domain_owner`
  (cross-tenant), `list_workspace_domains`, `delete_domain`. The
  **IdP-admin** verification methods (Microsoft/Google) are the next
  step. `AUTH-DESIGN.md`
  reworked: **IdP-admin auto-verification** is now the primary path
  (Entra `wids` Global-Admin + Graph `verifiedDomains`; Google Directory
  API `domains.list`), with **DNS-TXT** as the fallback — and the
  security rule restated: claim only IdP-*verified* domains, gated on
  confirming the user is a directory admin.

- **Team / org workspaces, invite-only** (Band 4.1 — `AUTH-DESIGN.md`
  stage S1). A user can create an **organization** workspace (they
  become its owner) and invite teammates by email; the invitee accepts
  via a tokened link and joins with the chosen role. A **workspace
  switcher** in the header lists every workspace you belong to and
  switches the active one (re-points the session). The Admin tab gains
  a **Members & Invites** panel (invite, list/revoke pending invites,
  list/remove members; last-owner removal is refused). Joining a
  workspace other than your own individual one is *only* by org
  creation or invite — no domain/SSO surface yet, so zero takeover
  risk at this stage. New `workspace_members` + `workspace_invites`
  tables (migration 0006, RLS-isolated); endpoints under
  `/api/v1/workspaces` and `/api/v1/invites/:token`; accept page at
  `/invite/<token>`.

### Changed

- **Every login now starts in an isolated individual workspace** —
  including corporate-domain emails. Dropped the `QPEDIA_ORG_DOMAINS`
  env-var domain→org mapping (a self-serve product shouldn't decide org
  membership from a server env var). Tenant resolution is now just:
  explicit `tenant_id` claim (set by the future org/SSO flow) → else the
  user's individual `u-<uid>`. The login also grants the user `admin` in
  their *own* individual workspace (RLS-scoped, never cross-tenant), so
  they can manage it. Org/team is a separate, **domain-verified** flow —
  see the new `AUTH-DESIGN.md` (model + full security matrix +
  recommendation to buy the SAML/OIDC federation layer, GCIP/WorkOS, and
  build only the workspace/membership/domain-verification policy in-app).
  Staged as Band 4 in `ROADMAP.md`.

### Fixed

- **Bulk-upload progress is now visible** (live-test feedback). Each
  folder-tree node shows a per-folder progress bar (`done/total`, rolled
  up over its subtree) that fills as sources reach `done`; the Sources
  list already polls every 2 s while the queue is draining. The upload
  panel shows an overall progress bar during the POST loop and refreshes
  the tree every 10 files, so a 300+-file folder upload reports progress
  instead of looking frozen.
- **Workspace banner height jitter.** The individual/organization banner
  could grow/shrink vertically as its description text reflowed to two
  lines depending on page width. The banner and header are now
  fixed-height, single-line (`nowrap`, description ellipsizes first), so
  the bar height is stable across pages.
- **Lock indication on mirror uploads clarified** (no behavior change).
  Mirror/drag-folder uploads already create *locked* (pinned) folders so
  the AI auto-organizer can't rearrange a hand-curated tree; the success
  toast, panel hint, and tree tooltip now say "🔒 locked" explicitly, and
  the README documents the three upload modes and what a locked folder
  means.

- **Firebase logins no longer fall back to the shared `default` tenant.**
  Previously a Firebase login with no `tenant_id` claim and no
  domain-matched tenant resolved to `default` — the same tenant
  dev/single-user ingestion writes to — so a freshly signed-in user saw
  that pre-existing data. Tenant resolution now: `tenant_id` claim →
  `QPEDIA_ORG_DOMAINS` match (shared org tenant, auto-provisioned) →
  registered `tenants.email_domain` → **isolated individual tenant
  `u-<uid>`**. Never `default`. (RLS was always enforcing isolation;
  the bug was the resolver collapsing distinct users into one tenant.)
- **Missing logout button / no workspace indicator after login.** The
  `/login` page did a client-side navigation that didn't remount the
  root layout, so it never re-fetched the session — the header kept
  showing "login". Login now does a full reload. The header shows a
  **Log out** button, and a new **workspace banner** makes the active
  tenant + individual-vs-org mode unmistakable (`/api/v1/auth/me` now
  returns `tenant` + `tenant_kind`).

### Added

- **`QPEDIA_ORG_DOMAINS`** — comma/space list of corporate email
  domains that resolve to a shared org tenant (slug of the domain).
  Everyone else gets an isolated individual workspace; public providers
  must not be listed.

- **Firebase / Google sign-in, enforced.** New `AuthMode::Session` — a
  session-cookie-gated mode that doesn't require a full OIDC issuer.
  Auto-selected when `QPEDIA_FIREBASE_PROJECT_ID` is set with no OIDC
  issuer (or `QPEDIA_AUTH_MODE=firebase`). Previously a Firebase login
  minted a session that the default Dev-mode `User` extractor ignored,
  so sign-in was cosmetic; now the cookie is enforced on every request.
  - **Admin bootstrap:** `QPEDIA_ADMIN_EMAILS` (comma/space allowlist) —
    a login whose verified email matches gets the `admin` group, so the
    first operator can administer a fresh deployment before any Firebase
    custom claims exist.
  - **Frontend build config** wired end-to-end: `web/.env(.example)` for
    `VITE_FIREBASE_*`, the Dockerfile web stage takes them as build args,
    and docker-compose passes them through from the top-level `.env`.
    `firebase.ts` also reads an optional `VITE_FIREBASE_APP_ID`.
  - `.env.example` documents the three modes (dev | firebase | oidc) and
    the new vars.
  - **`/login` is now the universal front door.** New public
    `GET /api/v1/auth/config` returns `{ mode, firebase }`; the header
    "login" link points at `/login` (not the OIDC-only `/auth/login`),
    and `/login` renders the right UI per mode — Firebase provider
    buttons, an OIDC "Continue with SSO" button, or a dev-mode notice.
    Fixes "OIDC routes not active in this auth mode" when signing in
    under Firebase.

- **Google Drive connector + SSO-aligned OAuth foundation** (Band 2.3).
  The second concrete connector after Confluence, plus the credential
  plumbing the rest of the connector line will share.
  - New `oauth_grants` table (migration 0005, RLS tenant-isolated):
    durable `(tenant, provider, scope, subject)` → refresh-token
    mapping, with PgStore CRUD. `subject=''` is an org-level grant.
  - `qpedia-connectors::oauth` — Google OAuth 2.0 helper:
    `consent_url` (offline access), `exchange_code`, `refresh`.
  - `qpedia-connectors::gdrive` — `GoogleDriveConnector`: `list_changed`
    via Drive `files.list` (incremental on `modifiedTime`), `download`
    via `files.get?alt=media` with `files.export` for Google-native docs
    (Docs→HTML, Sheets→CSV, Slides→text). Registered as `"gdrive"`.
  - **Connect Google Drive** in the Admin → Connectors card. Click →
    Google consent (read-only Drive) → callback exchanges the code for a
    refresh token, records the grant, and creates a `gdrive` connector
    that the existing auto-sync scheduler picks up. Optional folder-id
    restriction. New `GET /api/v1/connectors/google/{authorize,callback}`
    endpoints; `GOOGLE_OAUTH_CLIENT_ID` / `_SECRET` / `_REDIRECT_URL`
    env. Self-hosters not running SSO can supply a refresh token in the
    connector config directly (mirrors Confluence's API-token path).
  - The Admin tab also gains a general Connectors list (sync / delete).

  Note: Firebase Auth establishes identity but does not expose OAuth
  refresh tokens, so durable Drive access uses this separate
  authorization-code flow on the same Google account — see ROADMAP
  "Vision threads: SSO-aligned connectors".

- **Backup + restore scripts** (Band 3.3). `scripts/backup.sh` captures
  the three durable stores in dependency order — Postgres (`pg_dump
  -Fc`), one `git bundle` per tenant wiki repo, and a tar of
  `/data/raw` — into a timestamped `./backups/<ts>/` with a manifest.
  `scripts/restore.sh` reverses it (raw → Postgres `pg_restore --clean`
  → `git clone` each tenant bundle), gated behind a confirmation. Both
  default to the compose `postgres` service + `./data` bind-mount and
  accept a direct DSN via `QPEDIA_PG_MODE=dsn`. README's Backup section
  now points at them.

- **Chat rate limiting** (Band 3.4). `POST /api/v1/chat` is guarded by
  a per-tenant token bucket: default 30 requests/minute with a burst of
  10, configurable via `QPEDIA_CHAT_RPM` / `QPEDIA_CHAT_BURST`. Once a
  tenant's bucket is empty the endpoint returns `429 Too Many Requests`
  with a `Retry-After` header and a JSON body carrying
  `retry_after_seconds`. The limiter is in-process (fleet-wide effective
  limit is `N × QPEDIA_CHAT_RPM` across N replicas); `qpedia-pvt` swaps
  in a Redis-backed implementation via the new
  `AppBuilder::with_chat_rate_limiter`. Caps runaway LLM spend per
  tenant.

- **Multi-worker job runner** (Band 3.2). New `QPEDIA_WORKERS=N` env
  var (default `1`, clamped to `[1, 32]`) sets the size of the
  in-process ingest worker pool. Each worker has a distinct id
  (`worker-1`, `worker-2`, …) so `jobs.locked_by` tells you exactly
  which one holds a given lease. Concurrent claims are race-free
  because `claim_next_job` already used `SELECT … FOR UPDATE SKIP
  LOCKED LIMIT 1`; this commit simply spawns more polling tasks.
  Operators with bursty uploads or slow agent loops can scale ingest
  throughput linearly with no config drama.

- **CI for migrations** (Band 3.5). New `.github/workflows/ci.yml`
  with two jobs: `check` (`cargo check --workspace --all-targets`,
  ~30s after cache) and `migrate` (spins up
  `pgvector/pgvector:pg17` as a service container, runs
  `cargo test --workspace --all-targets` against it). The migrate
  step exercises a new `crates/qpedia-pg-store/tests/smoke.rs`
  integration test that applies every migration to a fresh DB and
  runs a tenants → folders → folder-ACLs → sources (insert / status
  / language / classification / replace-in-place / delete) → audit →
  wiki upsert + hybrid-search lifecycle, including a 384-dim
  `vector(384)` round-trip. Schema drift, RLS over-tightening, and
  pgvector / tsvector regressions now turn the build red on the
  same PR that introduces them. The smoke test gates on
  `QPEDIA_DB_URL` so local `cargo test` with no DB still passes
  (prints `skip:`).

- **Bulk ingest UX** (Band 2.2). Drag an OS folder onto the upload
  panel, or pick one with the new **Upload folder (mirror)** button:
  the OS subfolder structure is replicated under the selected qpedia
  folder as pinned folders (using slugified names returned by the
  server so `Q4 Reports` lands at `q4-reports`), and every file is
  uploaded into its mirrored location. The companion **Upload folder
  (AI organize)** button drops every file at `/` instead, letting the
  existing `classify.rs` auto-organizer group them into
  `/<doc_type>`. Either path reports live progress (`Uploaded
  N / M…`) and a one-line summary on completion. Drag-and-drop on the
  upload panel handles flat-file batches and recursive folder trees
  via the standard `webkitGetAsEntry` API.

- **Source replace-in-place** (Band 2.1). New
  `POST /api/v1/sources/:id/replace` multipart endpoint and matching
  Replace button on each source row. Same slug, same folder, same ACL
  are preserved; only the underlying bytes (and the metadata derived
  from them — filename, mime, sha256, size) are overwritten and the
  ingest pipeline re-runs from `Pending`. Existing wiki pages that
  reference this `source_id` are refreshed by the agent's
  `propose_patch` instead of being orphaned by a delete + re-upload.
  Identical bytes are a no-op (returns 200 with the unchanged row).
  Audited as `source.replaced`.

## [1.0.1] — 2026-05-28

Patch release. Downstream-only — no API or behaviour change for users
already on `v1.0.0`.

### Fixed

- Pin `pgvector = "=0.4.1"` in the workspace. `pgvector 0.4.2` (released
  upstream after v1.0.0) bumped its `sqlx` integration to v0.9, which
  splits our v0.8 dependency graph into two `sqlx-core` versions and
  breaks the `qpedia-pg-store` build for any downstream that resolves
  the patch range fresh (notably `qpedia-pvt` consuming via git tag).
  Pinning to `=0.4.1` keeps the graph unified. We'll un-pin in a future
  release once `qpedia-pg-store` migrates to `sqlx 0.9`.
- Workspace `license` field is now `Apache-2.0` (was leftover
  `proprietary` from pre-public scaffolding). Matches the project's
  declared OSS license; no functional change.

## [1.0.0] — 2026-05-28

First public release. The codebase is library-shaped (`qpedia_api`) and
binary-shaped (`qpedia-api`) so downstream overlays — notably
`qpedia-pvt`, our private SaaS — can compose without forking. See
[`OPEN-CORE.md`](OPEN-CORE.md) for the open-core philosophy and the
open / private boundary.

### Architecture

- **Storage:** Postgres 17 + pgvector + tsvector. One database for
  every piece of structured state: tenants, sources, sessions, jobs,
  audit, folder ACLs, folders, connectors, oidc_pending, and the
  `wiki_pages` search index (vector + BM25 in one SQL query).
- **Tenant isolation:** Postgres Row Level Security policies. Every
  tenant-scoped query opens a transaction that does
  `SET LOCAL ROLE qpedia_app` + `set_config('qpedia.tenant', …)`;
  RLS rejects any cross-tenant read or write inside the tx, fail-closed.
- **Identifiers:** `BIGSERIAL` internal primary keys + tenant-unique
  Wikipedia-style slugs (`quarterly-revenue-model`) as public
  identifiers. Slug collisions resolve by appending `-2`, `-3`, … and
  probing Postgres.
- **Two containers** to self-host: `app` + `postgres` (pgvector). An
  optional third (`marker`) ships behind `docker compose --profile
  marker` for high-fidelity PDF extraction.

### Ingest pipeline

- Extract → Classify → Distill (agent) → Validate → Commit → Embed → Done.
- **Extractors:** pdfium for PDFs (with Marker sidecar fallback for
  scanned/image-only PDFs; `QPEDIA_MARKER_PREFER=1` to route every PDF
  through Marker first), pandoc for DOCX, plain-text passthrough.
- **Classifier:** LLM emits doc_type / language / sensitivity / hints
  as JSON; classifier metadata is stored on the source row and used
  later by the agent priors.
- **Ingest agent** (`crates/qpedia-ingest/src/agent.rs`): bounded
  tool-using LLM loop (18 turns / 20 staged ops / 500 KB bundle / 50 KB
  per page). Tools: `search_wiki`, `list_pages`, `read_page`,
  `read_source`, `propose_new`, `propose_patch`, `done`.
- **Validator:** deterministic DiffBundle validator runs before every
  commit; rejects bundles with bad paths, exceeded size caps, or
  empty operations.
- **Atomic commit:** the bundle lands as one git commit in the
  per-tenant wiki repo at `/data/wiki/<tenant>/`.
- **Embed phase:** `wiki_pages` upserts via fastembed-rs
  (`bge-small-en-v1.5`, 384-dim, downloaded on first use).

### Wiki + retrieval

- **Per-tenant git repo** under `/data/wiki/<tenant>/`. Pages are
  markdown with YAML frontmatter (`title`, `kind`, `source_ids`,
  `tags`).
- **Hybrid search:** single SQL query weights pgvector cosine
  similarity (HNSW) with `ts_rank_cd` keyword ranking (alpha = 0.7).
- **Agentic chat retriever** (`crates/qpedia-retriever`): bounded
  gather loop (`search_wiki` / `read_page` / `follow_links` / `done`)
  followed by streaming SSE synthesis. ACL-filtered at every tool
  call.
- **Lint pass:** orphans, broken `[[wikilinks]]`, index drift, stale
  source IDs, near-duplicates (cosine ≥ 0.93 via pgvector self-join),
  and LLM-detected contradictions clustered by shared tag.

### Auth + ACLs

- **Firebase Auth federation** as the v2 recommended path. One project
  fronts Google / Apple / Microsoft / GitHub / X (Twitter) / Facebook
  plus enterprise OIDC SSO. Backend verifies ID tokens via JWKS
  (RS256, no Admin SDK needed) and mints its own opaque session cookie.
- **Direct OIDC (legacy)** still wired — `QPEDIA_OIDC_*` env vars
  enable the full auth-code + PKCE flow.
- **Dev mode** (no auth env set): every request is `dev:admin`.
- **ACLs:** per-source ACL stored alongside the source; folder-level
  ACL with closest-ancestor inheritance; wiki page ACL is the union
  of its source ACLs. The `admin` group passes everything.

### File Explorer tree

- Per-tenant **`folders`** table with a `pinned` flag (RLS-isolated).
  Folders may be explicit (a row, possibly empty) or implicit (derived
  from `sources.folder_path`). `list_folders` unions them so the UI
  shows both.
- Sources tab and Admin tab share the same `FolderTree.svelte`
  component: `+` new folder, lock/unlock against AI, delete empty,
  HTML5 drag-and-drop file moves.
- **Auto-pin on manual action:** folders you create are pinned and
  the AI auto-organize (in `classify.rs` and `lint.rs`) skips moving
  files into them. Files dragged into any non-root folder are already
  exempt because auto-organize only acts on `/`.

### External connectors

- Trait-based connector framework in `qpedia-connectors`.
- **Confluence Cloud** shipped as the first concrete connector.
- **Auto-sync scheduler:** every `QPEDIA_SYNC_INTERVAL_SECS` (default
  300s), the scheduler enqueues a Sync job for each enabled connector
  whose `last_run_at` is older than `QPEDIA_SYNC_STALE_SECS`
  (default 900s).
- Sync handler downloads each remote doc, mints a slug, persists raw
  bytes, and enqueues an Ingest job. Cursor + last-error per
  connector for incremental polling.

### Public library surface (the `qpedia-pvt` overlay shape)

- `qpedia_api::AppBuilder::from_env()` produces a composable HTTP
  application. Overlays add routes, inject typed services, register
  event sinks and tenant lifecycle hooks:

  ```rust
  AppBuilder::from_env().await?
      .with_state_extension(billing)
      .with_routes(billing_router())
      .with_event_sink(siem_sink)
      .with_tenant_hook(provisioning_hook)
      .serve().await
  ```

- **`EventSink` trait** (in `qpedia-pg-store::events`) fires from
  every `db.write_audit(...)` call site — HTTP routes *and*
  background-job handlers — on a detached task after the row is
  durably committed. A slow sink can never delay or fail the
  originating handler.
- **`TenantHook` trait** fires from `db.upsert_tenant(...)` the same
  way; `/api/v1/admin/bootstrap` inherits firing automatically.
- **`@qern/qpedia-web`** Svelte library — the same SvelteKit project
  at `web/` exposes its `src/lib/` as a published Svelte package so
  `web-pvt` can import components, the API client, and the OSS theme
  tokens (`@qern/qpedia-web/app.css`) — re-skin via CSS-variable
  redefinition without forking pages.

### Operational

- **Local dev:** the API auto-loads `.env` via dotenvy. Canonical DSN
  variable is `QPEDIA_DB_URL`; `QPEDIA_DATABASE_URL` and `DATABASE_URL`
  are accepted as fallbacks.
- **Docker:** `app` service's DB URL is owned by `docker-compose.yml`
  via `environment:` (built from `${QPEDIA_DB_PASSWORD}`), not by
  `.env`, so the host and container can share one `.env` with
  different `QPEDIA_DB_URL` values.
- **Health check:** `GET /healthz` → `ok`.
- **Embedded SPA:** `qpedia-api` serves the built SvelteKit SPA from
  `QPEDIA_WEB_DIR` (default `/app/web` in container, `./web/build`
  otherwise) with a SPA fallback.

### Crates

| Crate | Purpose |
|---|---|
| `qpedia-core` | Domain types: `Tenant`, `SourceId`, `Acl`, `Source`, `Job`, `DiffBundle`. |
| `qpedia-store` | Filesystem-only primitives: `BlobStore` (raw + extracted blobs), per-tenant `WikiRepoStore` (gix-backed). |
| `qpedia-pg-store` | Postgres + pgvector. All SQL, migrations, RLS plumbing, `EventSink` and `TenantHook` registries. |
| `qpedia-extract` | PDF / DOCX / text extraction. Marker sidecar client. |
| `qpedia-llm` | LLM provider abstraction (Anthropic, OpenAI, OpenAI-compat, OpenRouter). |
| `qpedia-embed` | fastembed-rs wrapper (bge-small-en-v1.5). |
| `qpedia-connectors` | Connector trait + Confluence Cloud impl + framework for premium connectors. |
| `qpedia-ingest` | Job runner + phase handlers + the ingest agent + validator. |
| `qpedia-retriever` | Two-phase agentic chat retriever (gather + streaming synthesis). |
| `qpedia-lint` | Wiki lint pass (orphans, broken links, drift, duplicates, contradictions). |
| `qpedia-api` | Composable HTTP layer (`AppBuilder` lib + thin binary). |
| `qpedia-cli` | CLI smoke tests + local-dev helpers. |

### Known limitations

- English-only embedder + tsvector (`bge-small-en-v1.5`; `'english'`
  full-text config). Cross-language wiki support is on
  [`ROADMAP.md`](ROADMAP.md) Band 2.6.
- Single-worker job runner per process (multi-worker on Band 3.2).
- No first-class HA / read replica configuration shipped — the schema
  permits, deployer's job.

[1.0.1]: https://github.com/qern-net/qpedia/releases/tag/v1.0.1
[1.0.0]: https://github.com/qern-net/qpedia/releases/tag/v1.0.0
[Unreleased]: https://github.com/qern-net/qpedia/compare/v1.0.1...HEAD
