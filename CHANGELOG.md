# Changelog

All notable changes to **qpedia** (the OSS engine). Format: [Keep a
Changelog](https://keepachangelog.com/en/1.1.0/), versioning:
[SemVer](https://semver.org/spec/v2.0.0.html).

The private SaaS overlay `qpedia-pvt` ships its own changelog.

## [Unreleased]

### Added

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
