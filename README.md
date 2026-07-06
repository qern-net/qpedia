# Qpedia

An LLM-powered knowledge base that turns uploaded documents into a searchable, linked wiki. Inspired by Karpathy's LLMWiki proposal. Apache-2.0.

Upload a PDF, Word doc, or HTML page. Qpedia extracts the text, classifies it, runs an agentic loop to write or update wiki pages, and makes the knowledge available through hybrid search and a chat interface.

> ### ☁️ Don't want to run it yourself?
> **[Qpedia Cloud → qpedia.cloud](https://qpedia.cloud)** is the fully managed, hosted version.
> Connect your Google Drive, SharePoint, or Confluence and get a living, AI-maintained wiki in minutes — no servers, no Postgres to babysit, enterprise SSO and premium connectors included. Free to start. See [pricing](https://qpedia.cloud/pricing).
>
> This repository is the open-source **engine** behind it (Apache-2.0). Self-host it, or let us run it for you.

**Docs:** [CHANGELOG](CHANGELOG.md) · [Architecture](DESIGN.md) · [Agents](AGENTS.md) · [Platform integration & SDK](docs/INTEGRATION.md) · [Project wiki](https://github.com/qern-net/qpedia/wiki)

---

## Architecture

Two containers. That's it.

```
┌─────────────────────── app (Rust) ───────────────────────────┐
│  axum HTTP API · ingest workers · retriever · linter         │
│  git     — wiki markdown at /data/wiki (one repo per tenant) │
│  /data/raw — uploaded docs + extracted text                  │
│  fastembed-rs (bge-small-en-v1.5) — local embeddings         │
│  tesseract + pdfium + pandoc — extraction                    │
│  SvelteKit static assets served by axum                      │
└──────────────────────────────┬───────────────────────────────┘
                               │ SQL (sqlx, pool, RLS)
┌──────────────────────────────▼─── postgres + pgvector ──────┐
│  tenants · sources · jobs · sessions · audit · folder_acls  │
│  folders · connectors · oidc_pending                        │
│  wiki_pages — vector(384) + tsvector hybrid search          │
└─────────────────────────────────────────────────────────────┘
```

Volumes: bind-mounts `./data/wiki`, `./data/raw`, `./data/models`; named volume `postgres-data` for the database. Optional third container: the Marker high-fidelity PDF sidecar (off by default; opt in with `--profile marker`).

---

## Qpedia as a foundational layer

Qpedia is not only an end-user product — it is the **foundational knowledge layer that other AI applications build on**. It owns the hard, amortizable parts of a retrieval system (ingestion, extraction, classification, the LLM-authored wiki, embeddings, hybrid search, and RAG chat) so an application on top doesn't have to reimplement any of it.

External applications integrate over the stable **`/api/v1` HTTP boundary** — never by reaching into Qpedia's database. The contract is versioned in this repo at [`contracts/qpedia-openapi.yaml`](contracts/qpedia-openapi.yaml) (OpenAPI 3.1); it is the canonical, sanctioned surface every consumer codes against. The platform pattern is:

- **API boundary, not schema coupling.** Consumers call `/api/v1` (ingest → search → chat). They run their own Postgres *schema* in the same instance (e.g. the RFP app owns `rfp`) but never read Qpedia's tables via cross-schema SQL.
- **Tenancy lines up.** Each consumer tenant maps to a Qpedia workspace; both sides share the OIDC issuer, and Postgres RLS enforces isolation end-to-end.
- **Machine-to-machine auth.** External calls authenticate via an [`ExternalAuthProvider`](crates/qpedia-api/src/auth.rs) registered by the deployment overlay. The provider returns a `User` with tenant + groups, so RLS scoping is identical to a user session. The OSS engine ships no concrete implementation; overlays supply the scheme their consumers need (service tokens, OAuth 2 client-credentials JWTs, etc.).
- **Graceful degradation.** A consumer that loses its Qpedia connection keeps operating on its own data and re-syncs later; Qpedia is an additive layer, not a hard runtime dependency.

Applications building on this layer today:

| App | What it adds on top of Qpedia |
|---|---|
| **qpedia-rfp** (`qproc`) | RFP/tender aggregation & response assembly — the first external application; ingests opportunities into Qpedia and reads them back via search/chat. |
| **qcodia** | The same "distilled, linked knowledge layer" idea applied to source code (symbol graph + code wiki) for whole-repo PR review and spec/codegen governance. Its hub is an in-process Qpedia overlay (reusing the wiki + summary embeddings); its resident forge adapters call `/api/v1` externally. |

> Building a new application on Qpedia? Start with the **[platform integration & SDK guide](docs/INTEGRATION.md)**, then the contract at `contracts/qpedia-openapi.yaml` and the platform notes in [`DESIGN.md`](DESIGN.md).

---

## Quick Start

This repository is the **engine** — the Rust workspace. Production
container images, `docker-compose.yml`, and the SvelteKit frontend are
maintained in the deployment overlay; for a one-command hosted instance
use **[Qpedia Cloud](https://qpedia.cloud)**.

To run the engine from source you need a recent Rust toolchain and a
PostgreSQL 17 instance with the `pgvector` extension.

```bash
cp .env.example .env
# Set OPENAI_API_KEY (or ANTHROPIC_API_KEY), and point QPEDIA_DB_URL at a
# Postgres 17 + pgvector instance (sslmode=disable for local).
docker compose up -d        # starts Postgres 17 + pgvector (the only dependency)
cargo run -p qpedia-api
```

The API comes up on the address in `QPEDIA_BIND` (default
`0.0.0.0:8080`). In dev mode (no Firebase configured) every request is the
`dev:admin` user — no login required. With no SPA build present the server
runs API-only; point `QPEDIA_WEB_DIR` at a built frontend to serve a UI.

The bundled `docker-compose.yml` brings up **only** Postgres — this repo is
the engine, with no app image of its own. The production image, frontend,
and full multi-container stack live in the deployment overlay.

---

## Configuration

All config is via environment variables, loaded from `.env` by Docker Compose.

### LLM Provider

**Qpedia is BYOL — bring your own LLM.** Qpedia does not ship or resell
inference: you supply a provider key (or an OpenAI-compatible / on-prem
endpoint) and Qpedia calls *your* account. With no provider configured,
ingestion stops at `Extracted` (no wiki distillation). A metered, Cloud-managed
LLM option is planned for the hosted tier but BYOL stays first-class on every
plan. The
validated/supported models (cloud + open-weight), reviewed each quarter, are in
[`APPROVED-MODELS.md`](APPROVED-MODELS.md).

Auto-detected from whichever API key is present. Set `QPEDIA_LLM_PROVIDER` to override.

| Variable | Purpose |
|---|---|
| `QPEDIA_LLM_PROVIDER` | `anthropic` \| `openai` \| `openrouter` \| `openai-compatible` |
| `QPEDIA_LLM_MODEL` | Override the per-provider default model |
| `ANTHROPIC_API_KEY` | Anthropic direct (default: `claude-haiku-4-5`) |
| `OPENAI_API_KEY` | OpenAI direct (default: `gpt-5.4-mini`) |
| `OPENROUTER_API_KEY` | OpenRouter (default: `anthropic/claude-haiku-4-5`) |
| `QPEDIA_LLM_BASE_URL` | Base URL for OpenAI-compatible endpoint (vLLM, Ollama, LM Studio) |
| `QPEDIA_LLM_API_KEY` | API key for OpenAI-compatible endpoint |

### Storage

| Variable | Default | Purpose |
|---|---|---|
| `QPEDIA_DATA_DIR` | `/data` | Root for blobs (`raw/`), wiki repos (`wiki/`), and the embedder model cache (`models/`) |
| `QPEDIA_DB_URL` | — | Postgres DSN. In Docker `docker-compose.yml` builds this from `QPEDIA_DB_PASSWORD` pointed at the `postgres` service. For local `cargo run`, point at `127.0.0.1:5432`. |
| `QPEDIA_DB_PASSWORD` | `qpedia-dev` | Postgres password — single source of truth (also drives the `postgres` service in compose) |
| `QPEDIA_DB_MAX_CONN` | `16` | sqlx pool max connections |
| `QPEDIA_WIKI_AUTHOR_NAME` | `qpedia-bot` | Git commit author name |
| `QPEDIA_WIKI_AUTHOR_EMAIL` | `bot@qpedia.local` | Git commit author email |

### Embedding

| Variable | Default | Purpose |
|---|---|---|
| `QPEDIA_EMBED_MODEL` | `bge-small-en-v1.5` | Local embedding model (`bge-small` or `bge-base`) |
| `QPEDIA_EMBED_CACHE_DIR` | `/data/models` | Model download cache |

### Auth

| Variable | Default | Purpose |
|---|---|---|
| `QPEDIA_AUTH_MODE` | `dev` (if no OIDC issuer set) | `dev` bypasses auth; every request is `dev:admin` |
| `QPEDIA_OIDC_ISSUER` | — | OIDC issuer URL (Auth0, Okta, Entra, Keycloak) |
| `QPEDIA_OIDC_CLIENT_ID` | — | OIDC client ID |
| `QPEDIA_OIDC_CLIENT_SECRET` | — | OIDC client secret |
| `QPEDIA_OIDC_REDIRECT_URL` | — | Callback URL, e.g. `http://localhost:8080/auth/callback` |
| `QPEDIA_OIDC_GROUPS_CLAIM` | `groups` | JWT claim that carries group membership |

### Other

| Variable | Default | Purpose |
|---|---|---|
| `QPEDIA_BIND` | `0.0.0.0:8080` | Listen address |
| `RUST_LOG` | `qpedia=info,tower_http=info` | Log filter |
| `QPEDIA_MARKER_URL` | — | Optional high-fidelity PDF sidecar (see below) |
| `QPEDIA_RRF_K` | `60` | Hybrid-search rank-decay constant for Reciprocal Rank Fusion. Lower (≈20–30) sharpens precision; higher (≈80–100) favors recall/consensus. Clamped to `1..=1000`. |
| `QPEDIA_QUEUE_DEPTH_SAMPLE_SECS` | `15` | How often (seconds) to sample pending-job counts and record the `jobs.queue.depth` OTel gauge. Set to 0 or omit to use the default. |

---

## Ingest Pipeline

Every uploaded document moves through a state machine. Each transition is **idempotent** — a worker crash at any point is safe; on restart the job re-enters from the last recorded state.

```
Pending → Extracting → Extracted
                            ↓ (requires LLM)
                       Classifying → Classified
                                          ↓
                                   AgentDistilling → Committed
                                                          ↓
                                                     Embedding → Done

Any step → Failed (retried up to max_attempts, then Dead)
No LLM   → Tainted (resumable once LLM is configured)
```

**Tainted** sources are visible in the Admin panel and can be re-enqueued with one click once an LLM is configured.

### Extraction

| Format | Tool |
|---|---|
| PDF (text layer present) | pdfium-render — fast direct extraction |
| PDF (image-only / scanned) | pdfium-render detects sparse text layer (< 20 chars/page avg) → delegates to Marker sidecar (requires `QPEDIA_MARKER_URL`). If Marker fails or is not configured, returns the sparse pdfium result with a warning. |
| DOCX / HTML | pandoc subprocess |
| Plain text / Markdown | passthrough |

### Classification

One cheap LLM call produces `{doc_type, language, sensitivity, hints}`. Stored in the source row and fed to the agent as priors.

### Agent Distillation

A bounded tool-using LLM loop (max 18 turns, 20 staged ops, 500 KB bundle). The agent reads the source, searches the existing wiki, then proposes new pages or patches to existing ones. Nothing touches disk until the validator approves the bundle.

### Validation

Deterministic checks before any commit:
- Frontmatter present and contains `title` + `kind` (system files exempt: `index.md`, `log.md`, `QPEDIA.md`, `_meta/*`)
- All `[[wikilinks]]` resolve to existing or newly-created pages (system files exempt — `index.md` is a catalog that references pages across all ingests)
- Bundle within size caps

### Commit

Git commit first (durable), then `wiki_pages` upsert in Postgres (derivable: row carries title/content/tags/source_slugs + the 384-dim embedding and a generated tsvector). If the upsert fails, the job retries from `Committed` — git is the source of truth and the index is rebuildable.

---

## Remove Pipeline

When a source is deleted, the remove job performs a complete cleanup in order:

1. **Plan** — walk every wiki page, find those whose `source_ids` frontmatter includes the deleted source
2. **Delete** — pages whose only source is the deleted one are staged for `DiffOp::Delete`
3. **Patch** — pages with other remaining sources have the deleted source stripped from their `source_ids` frontmatter; body is left intact
4. **index.md** — all wikilinks pointing to deleted pages are removed from `index.md`
5. **log.md** — a timestamped entry is appended recording what was deleted/patched
6. **Git commit** — all ops land as a single atomic commit
7. **`wiki_pages`** — deleted pages are removed from the Postgres search index; patched pages are re-embedded and upserted
8. **Blobs** — `/data/raw/<source_id>/` is deleted
9. **DB row** — the source row is deleted last

The pipeline is idempotent: if the source row is already gone on retry, the job skips straight to blob cleanup. If the git commit already happened (pages no longer exist in the wiki), the plan produces an empty bundle and the job skips to `wiki_pages`/blob/DB cleanup.

---

## Job System

All background work runs through a Postgres-backed job queue (`jobs` table). Workers claim via `UPDATE … FOR UPDATE SKIP LOCKED`. Jobs are visible and schedulable from the Admin panel.

| Kind | Trigger | Purpose |
|---|---|---|
| `ingest` | Source upload | Full pipeline for one source |
| `remove` | Source delete | Clean up wiki pages and vectors |
| `lint` | Manual / scheduled | Wiki health checks |
| `reembed` | Manual | Re-embed all pages (e.g. after model change) |
| `sync` | Connector schedule | Pull changed docs from Confluence etc. |

Job states: `queued → running → done` or `failed` (retried) → `dead` (exhausted).

---

## Wiki Structure

The wiki is a git repository at `/data/wiki`. Every page is a Markdown file with YAML frontmatter:

```yaml
---
id: 01KRSE8CS9Z18JPW21K122PQAW
title: "Enterprise AI Governance"
kind: concept
source_ids: ["01KRS9HGA7VTEBY10KXRKV8KQG"]
tags: ["ai", "governance", "enterprise"]
created_at: 2026-05-16T21:00:00Z
updated_at: 2026-05-16T21:00:00Z
---
```

Page kinds: `concept`, `entity`, `comparison`, `summary`, `meta`.

Directory layout:
```
wiki/
├── QPEDIA.md          # style guide loaded into every agent call
├── index.md           # LLM-maintained catalog
├── log.md             # append-only event history
├── concepts/
├── entities/
├── comparisons/
├── summaries/         # one page per ingested source
└── _meta/             # lint reports, embeddings lock
```

---

## API

All endpoints under `/api/v1`. Auth via session cookie (OIDC) or dev mode.

### Sources
```
POST   /api/v1/sources                  Upload a document (multipart)
GET    /api/v1/sources?folder=&limit=   List sources in a folder
GET    /api/v1/sources/:id              Get source metadata
DELETE /api/v1/sources/:id              Enqueue remove job
```

### Wiki
```
GET    /api/v1/wiki/list?prefix=        List page paths
GET    /api/v1/wiki/search?q=&limit=    Hybrid search
GET    /api/v1/wiki/pages/*path         Get page markdown
```

### Chat
```
POST   /api/v1/chat                     SSE stream: meta → token… → done
```

### Admin (requires admin group)
```
GET    /api/v1/admin/sources/stalled    Sources stuck mid-pipeline
POST   /api/v1/admin/sources/resume     Re-enqueue all stalled sources
POST   /api/v1/admin/lint               Trigger lint job
GET    /api/v1/admin/lint               Last lint report
POST   /api/v1/admin/reembed            Trigger reembed job (rebuild wiki_pages search index from git)
GET    /api/v1/admin/folder-acls        List folder ACL rules
PUT    /api/v1/admin/folder-acls        Set a folder ACL
DELETE /api/v1/admin/folder-acls        Remove a folder ACL
```

---

## Frontend

The SvelteKit 5 + TypeScript SPA lives in the deployment overlay (it is
published as the `@qern/qpedia-web` package). `qpedia-api` serves a static
build from `QPEDIA_WEB_DIR` when one is present, and runs API-only
otherwise. The routes the SPA exposes:

| Route | Purpose |
|---|---|
| `/` | Sources list, upload panel, status chips |
| `/wiki` | Wiki tree + page viewer |
| `/search` | Hybrid search |
| `/chat` | Agentic chat with citations |
| `/admin` | Stalled sources, folder ACLs (admin only) |

### Uploading & folders

The Sources tab gives you three ways to add documents, which differ only in
how files land in the folder tree:

| Action | Where files land | Folder lock |
|---|---|---|
| **File picker / drag a flat batch** | The folder selected in the tree (`/` by default) | — |
| **📁 Upload folder (mirror)** / drag a folder | Subfolders mirroring your OS structure, created under the selected folder | 🔒 **Locked** |
| **🤖 Upload folder (AI organize)** | Everything at `/`; classification then auto-moves each file into `/<doc_type>` | — |

A **🔒 locked** (pinned) folder is excluded from AI auto-organization: the
classifier and linter will not move files in or out of it, rename it, or delete
it — your structure is preserved exactly as uploaded. Mirror uploads lock the
folders they create so a hand-curated tree survives ingestion. You can toggle
the lock on any folder from the tree (🔒/🔓), and manually-created folders are
locked by default. Auto-organized files (root `/` uploads) flow into unlocked
`/<doc_type>` folders the AI manages.

While files ingest, each tree node shows a live progress bar (`done/total`,
rolled up over its subtree) that fills as sources reach the `done` state; the
list polls every 2 s until the queue drains.

---

## Connectors

External document sources that sync on a schedule. Configured via the `connectors` table.

| Kind | Status |
|---|---|
| `confluence` | Implemented — pulls pages from a Confluence Cloud space |
| `gdrive` | Stub — trait defined, implementation pending |
| `sharepoint` | Stub — trait defined, implementation pending |

---

## High-Fidelity PDF Sidecar (Marker)

For table-heavy, multi-column, or formula-heavy PDFs, the extractor can
delegate to an optional **Marker** sidecar (an HTTP service; packaged in
the deployment overlay). Set `QPEDIA_MARKER_URL` to its address.

By default the Rust extractor delegates to Marker only when the PDF text layer is sparse (< 20 chars/page average) — i.e. scanned / image-only PDFs. For a normal digital PDF, pdfium handles it directly.

To send **every** PDF through Marker first (high-fidelity markdown for table-heavy, multi-column, or formula-heavy PDFs), also set `QPEDIA_MARKER_PREFER=1`. The pdfium two-pass remains as the fallback on any sidecar error, so a Marker outage doesn't block ingestion.

If `QPEDIA_MARKER_URL` is unset, scanned/image-only PDFs return the sparse pdfium result with a warning rather than failing.

---

## Operational Principles

1. **Every ingestion is idempotent.** Each pipeline stage is keyed by `(source_id, status)`. Re-running a job from any point produces the same result. Duplicate uploads are detected by SHA-256.

2. **Every internal job is visible and schedulable.** All background work goes through the Postgres job queue. Jobs can be inspected, retried, and triggered from the Admin panel. Nothing runs silently.

3. **All anomalies are visible.** Sources that stop mid-pipeline are marked `Tainted` (not silently dropped). Failed jobs record their error. The lint job surfaces orphans, broken links, stale source references, near-duplicates, and contradictions. The Admin panel shows all stalled sources with a one-click resume.

---

## Crate Map

| Crate | Purpose |
|---|---|
| `qpedia-core` | Domain types: `Source`, `WikiPage`, `Job`, `Acl`, `Tenant`, IDs |
| `qpedia-pg-store` | All SQL: tenants, sources, sessions, jobs, audit, folder_acls, folders, connectors, wiki_pages, hybrid_search, near-duplicates, slug helpers. Tenant isolation via per-request `SET LOCAL qpedia.tenant` + RLS. |
| `qpedia-store` | Filesystem-only: `BlobStore` (raw uploads + extracted text) and `WikiRepoStore` (per-tenant git repos). |
| `qpedia-extract` | Text extraction: PDF, DOCX, HTML, plain text, OCR |
| `qpedia-llm` | LLM provider abstraction: Anthropic, OpenAI, OpenRouter, OpenAI-compatible |
| `qpedia-embed` | Local embeddings via fastembed-rs (bge-small / bge-base) |
| `qpedia-ingest` | Ingest pipeline: state machine, agent loop, validator, job runner |
| `qpedia-retriever` | Query-time RAG: gather phase (agent) + synthesize phase (streaming) |
| `qpedia-lint` | Wiki health: orphans, broken links, duplicates, contradictions |
| `qpedia-connectors` | External sync: Confluence Cloud (implemented), GDrive/SharePoint (stubs) |
| `qpedia-api` | axum HTTP server, SSE chat, static SPA serving |
| `qpedia-cli` | Admin CLI: status, lint, reembed (stubs, not yet wired) |
| `qpedia-bench` | Retrieval benchmark harness: ingests a labeled corpus (with planted near-duplicate distractors) into a throwaway tenant and scores the real `hybrid_search` ranking path — Recall@K, MRR, nDCG@10, Exact@1 — with `baseline.json` regression gating |

---

## Development

```bash
# Check all crates
cargo check

# Run the full test suite (needs a running Postgres; see Quick Start)
cargo test --workspace

# Run the API locally against a Postgres 17 + pgvector instance
cargo run -p qpedia-api

# Benchmark the hybrid-search ranking path (needs QPEDIA_DB_URL + an embedder).
# Add --update-baseline to record the current scores as the regression baseline.
QPEDIA_DB_URL=postgres://… cargo run -p qpedia-bench -- run
```

The frontend (in the deployment overlay) runs its own dev server that
proxies API calls to `:8080`.

---

## Backup

| Data | Method | Frequency |
|---|---|---|
| Postgres (jobs/sources/sessions/audit/folder_acls/folders/connectors/`wiki_pages`) | `pg_dump -Fc` | Hourly |
| `/data/wiki/<tenant>/.git` | `git bundle` per tenant | Daily |
| `/data/raw` | rsync | Continuous |

Restore order: raw → Postgres → wiki. The wiki repo is the source of truth for page content; `wiki_pages` is a derived search index — if it's lost, trigger the `reembed` admin job to rebuild it from git.

Scripted, in dependency order:

```bash
# Back up Postgres (pg_dump -Fc), one git bundle per tenant wiki, and a
# tar of /data/raw — into ./backups/<timestamp>/
bash scripts/backup.sh

# Restore from a backup directory (DROPS + recreates the target DB).
bash scripts/restore.sh ./backups/20260529T120000Z
```

Both default to a `postgres` service and the `./data` bind-mount; override with `QPEDIA_DATA_DIR`, `QPEDIA_BACKUP_DIR`, or `QPEDIA_PG_MODE=dsn QPEDIA_DB_URL=…` for a direct connection. See the header comments in each script for the full env surface.


