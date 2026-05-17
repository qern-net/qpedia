# Qpedia

An LLM-powered knowledge base that turns uploaded documents into a searchable, linked wiki. Inspired by Karpathy's LLMWiki proposal.

Upload a PDF, Word doc, or HTML page. Qpedia extracts the text, classifies it, runs an agentic loop to write or update wiki pages, and makes the knowledge available through hybrid search and a chat interface.

---

## Architecture

Two containers. That's it.

```
┌─────────────────────── app (Rust) ───────────────────────────┐
│  axum HTTP API · ingest workers · retriever · linter         │
│  SQLite  — jobs, sessions, audit, sources                    │
│  git     — wiki markdown at /data/wiki                       │
│  /data/raw — uploaded docs + extracted text                  │
│  fastembed-rs (bge-small-en-v1.5) — local embeddings         │
│  tesseract + pdfium + pandoc — extraction                    │
│  SvelteKit static assets served by axum                      │
└──────────────────────────────┬───────────────────────────────┘
                               │ REST
┌──────────────────────────────▼──── weaviate ────────────────┐
│  WikiPage class · hybrid search (BM25 + vector)             │
└─────────────────────────────────────────────────────────────┘
```

Volumes: `wiki-data`, `raw-data`, `sqlite-data`, `weaviate-data`, `models-cache`.

---

## Quick Start

```bash
cp .env.example .env
# Set OPENAI_API_KEY (or ANTHROPIC_API_KEY) in .env or pass from shell
docker compose up --build
```

Open http://localhost:8080. In dev mode every request is the `dev:admin` user — no login required.

To pass an API key from your shell without writing it to disk:

```yaml
# docker-compose.yml — add to the app service:
environment:
  - OPENAI_API_KEY   # no value = pass-through from host shell
```

---

## Configuration

All config is via environment variables, loaded from `.env` by Docker Compose.

### LLM Provider

Auto-detected from whichever API key is present. Set `QPEDIA_LLM_PROVIDER` to override.

| Variable | Purpose |
|---|---|
| `QPEDIA_LLM_PROVIDER` | `anthropic` \| `openai` \| `openrouter` \| `openai-compatible` |
| `QPEDIA_LLM_MODEL` | Override the per-provider default model |
| `ANTHROPIC_API_KEY` | Anthropic direct (default: `claude-haiku-4-5`) |
| `OPENAI_API_KEY` | OpenAI direct (default: `gpt-4.1-mini`) |
| `OPENROUTER_API_KEY` | OpenRouter (default: `anthropic/claude-haiku-4-5`) |
| `QPEDIA_LLM_BASE_URL` | Base URL for OpenAI-compatible endpoint (vLLM, Ollama, LM Studio) |
| `QPEDIA_LLM_API_KEY` | API key for OpenAI-compatible endpoint |

### Storage

| Variable | Default | Purpose |
|---|---|---|
| `QPEDIA_DATA_DIR` | `/data` | Root for all persistent data |
| `QPEDIA_WEAVIATE_URL` | `http://weaviate:8080` | Weaviate endpoint |
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

Git commit first (durable), then Weaviate upsert (derivable). If Weaviate fails, the job retries from `Committed` — git is the source of truth.

---

## Remove Pipeline

When a source is deleted, the remove job performs a complete cleanup in order:

1. **Plan** — walk every wiki page, find those whose `source_ids` frontmatter includes the deleted source
2. **Delete** — pages whose only source is the deleted one are staged for `DiffOp::Delete`
3. **Patch** — pages with other remaining sources have the deleted source stripped from their `source_ids` frontmatter; body is left intact
4. **index.md** — all wikilinks pointing to deleted pages are removed from `index.md`
5. **log.md** — a timestamped entry is appended recording what was deleted/patched
6. **Git commit** — all ops land as a single atomic commit
7. **Weaviate** — deleted pages are removed from the vector index; patched pages are re-embedded
8. **Blobs** — `/data/raw/<source_id>/` is deleted
9. **DB row** — the source row is deleted last

The pipeline is idempotent: if the source row is already gone on retry, the job skips straight to blob cleanup. If the git commit already happened (pages no longer exist in the wiki), the plan produces an empty bundle and the job skips to Weaviate/blob/DB cleanup.

---

## Job System

All background work runs through a SQLite-backed job queue. Jobs are visible and schedulable from the Admin panel.

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
GET    /api/v1/admin/folder-acls        List folder ACL rules
PUT    /api/v1/admin/folder-acls        Set a folder ACL
DELETE /api/v1/admin/folder-acls        Remove a folder ACL
```

---

## Frontend

SvelteKit 5 + TypeScript, built into the container and served by axum.

| Route | Purpose |
|---|---|
| `/` | Sources list, upload panel, status chips |
| `/wiki` | Wiki tree + page viewer |
| `/search` | Hybrid search |
| `/chat` | Agentic chat with citations |
| `/admin` | Stalled sources, folder ACLs (admin only) |

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

For table-heavy, multi-column, or formula-heavy PDFs, enable the optional Marker sidecar:

```bash
docker compose --profile marker up -d marker
```

Then set `QPEDIA_MARKER_URL=http://marker:8000` in `.env`. The Rust extractor delegates to Marker and falls back to pdfium on any failure.

Cost: ~5 GB image, 30–90 s cold start, seconds to minutes per PDF on CPU. GPU recommended for throughput.

---

## Operational Principles

1. **Every ingestion is idempotent.** Each pipeline stage is keyed by `(source_id, status)`. Re-running a job from any point produces the same result. Duplicate uploads are detected by SHA-256.

2. **Every internal job is visible and schedulable.** All background work goes through the SQLite job queue. Jobs can be inspected, retried, and triggered from the Admin panel. Nothing runs silently.

3. **All anomalies are visible.** Sources that stop mid-pipeline are marked `Tainted` (not silently dropped). Failed jobs record their error. The lint job surfaces orphans, broken links, stale source references, near-duplicates, and contradictions. The Admin panel shows all stalled sources with a one-click resume.

---

## Crate Map

| Crate | Purpose |
|---|---|
| `qpedia-core` | Domain types: `Source`, `WikiPage`, `Job`, `Acl`, `Tenant`, IDs |
| `qpedia-store` | Storage: SQLite (jobs/sources/audit), Weaviate, git wiki repo, blob FS |
| `qpedia-extract` | Text extraction: PDF, DOCX, HTML, plain text, OCR |
| `qpedia-llm` | LLM provider abstraction: Anthropic, OpenAI, OpenRouter, OpenAI-compatible |
| `qpedia-embed` | Local embeddings via fastembed-rs (bge-small / bge-base) |
| `qpedia-ingest` | Ingest pipeline: state machine, agent loop, validator, job runner |
| `qpedia-retriever` | Query-time RAG: gather phase (agent) + synthesize phase (streaming) |
| `qpedia-lint` | Wiki health: orphans, broken links, duplicates, contradictions |
| `qpedia-connectors` | External sync: Confluence Cloud (implemented), GDrive/SharePoint (stubs) |
| `qpedia-api` | axum HTTP server, SSE chat, static SPA serving |
| `qpedia-cli` | Admin CLI: status, lint, reembed (stubs, not yet wired) |

---

## Development

```bash
# Check all crates
cargo check

# Run the API locally (needs a running Weaviate)
docker compose up -d weaviate
cargo run --bin qpedia-api

# Frontend dev server (proxies to :8080)
cd web && npm install && npm run dev
```

The frontend dev server runs on `:5173` and proxies API calls to `:8080`.

---

## Backup

| Data | Method | Frequency |
|---|---|---|
| `/data/wiki/.git` | `git bundle` | Daily |
| `/data/raw` | rsync | Continuous |
| `/data/sqlite/qpedia.db` | `sqlite3 .backup` | Hourly |
| Weaviate | Built-in backup module | Daily |

Restore order: raw → sqlite → weaviate → wiki (wiki derives from the others).
