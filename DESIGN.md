# Qpedia — Detailed Design

_Rust-based LLMWiki knowledge base, inspired by Karpathy's LLM Wiki proposal._
_Status: **implemented** — see `CHANGELOG.md` for the release history and
the [project wiki](https://github.com/qern-net/qpedia/wiki) for guides._

> This document is the architectural deep-dive. The shipping stack is
> Postgres + pgvector. The narrower `AGENTS.md` covers each LLM agent in
> detail.

---

## 0. Goals & Non-Goals

**Goals**
- Enterprise document store (up to 1M docs, 100 concurrent users; multi-tenant via RLS).
- Cloud-hosted default; same image ships on-prem.
- LLM-authored wiki as the semantic layer (Karpathy three-layer model: raw sources + wiki + schema).
- File-explorer folder/upload UX and chat-over-wiki UX, equally weighted.
- Fits in **two containers**: `app` + `postgres` (pgvector). Marker is an optional third.
- All heavy lifting hidden from users.
- **Be a platform foundation** — a stable `/api/v1` that other AI applications build on, so they reuse Qpedia's ingestion / embeddings / wiki / RAG instead of reimplementing them.

**Non-Goals (still)**
- Real-time collaborative editing of wiki pages.
- Federated search across external corpora.
- Mobile app.
- Bespoke per-consumer endpoints — the external surface stays the small, versioned `/api/v1` in `qpedia-openapi.yaml`; application-specific logic lives in the consuming app, not the engine.

---

## 0.5 Qpedia as a platform foundation

Qpedia is consumed in two ways: directly by end users, and as the **foundational knowledge layer beneath other AI applications**. The second mode is a first-class design constraint, not an afterthought, and it has a deliberate shape:

- **One integration surface.** External apps reach Qpedia only through the versioned `/api/v1` HTTP contract ([`qpedia-openapi.yaml`](qpedia-openapi.yaml)) — ingest, hybrid search, RAG chat. The Rust handlers are the source of truth; the contract is kept in lockstep and a change to either is a coordinated PR.
- **Shared instance, isolated schemas.** A consumer may run its own Postgres schema in the same instance (e.g. an `app` schema) for its own structured data, but **never** reads Qpedia's tables via cross-schema SQL. The only path to Qpedia's knowledge is the API. This keeps the engine free to evolve its schema without breaking consumers.
- **Tenant = workspace, RLS end-to-end.** Each consumer tenant maps to a Qpedia workspace; both sides share the OIDC issuer, and every call sets `qpedia.tenant` so Postgres RLS enforces isolation regardless of which app originated the request.
- **Machine identity carries tenant.** External apps authenticate M2M via an `ExternalAuthProvider` the deployment overlay registers (`AppBuilder::with_auth_provider`) — a service token, an OAuth 2 client-credentials JWT, or whatever scheme that deployment needs. The engine itself ships no concrete scheme; it only requires that whatever the overlay returns carries tenant + groups, so an external call is scoped by RLS identically to a user session. This is why a bare API key was rejected: it carries no identity.
- **Additive, degradable dependency.** A consumer that can't reach Qpedia keeps serving its own data and re-syncs later. Qpedia is a layer apps lean on, never a hard runtime coupling.

The payoff: the expensive, reusable retrieval machinery is built and operated once, and each new application on top is thin.

---

## 1. Two-Container Architecture

```
┌───────────────────────────── app (Rust) ─────────────────────────────┐
│  axum HTTP/WS API · ingest workers · retriever · linter              │
│  gix-managed git repos: /data/wiki/<tenant>/   (one per tenant)      │
│  filesystem volume:    /data/raw  (uploaded docs + OCR text)         │
│  fastembed-rs (bge-small-en-v1.5) — embeddings in-process            │
│  tesseract + pdfium + pandoc  — extraction tooling                   │
│  SvelteKit static assets served by axum                              │
└────────────────────────────────┬─────────────────────────────────────┘
                                 │ SQL (sqlx, RLS-scoped pool)
┌────────────────────────────────▼─── postgres + pgvector ────────────┐
│  tenants · sources · jobs · sessions · audit · folder_acls · folders │
│  connectors · oidc_pending                                          │
│  wiki_pages — vector(384) + tsvector hybrid search in one query     │
└──────────────────────────────────────────────────────────────────────┘

Bind mounts: ./data/wiki, ./data/raw, ./data/models.  Named volume: postgres-data.
Optional third container: Marker high-fidelity PDF sidecar (--profile marker).
```

---

## 2. Data Model

### 2.1 Filesystem layout (inside `app` container)

```
/data/
├── raw/
│   ├── <source_id>/
│   │   ├── original.<ext>           # as uploaded
│   │   ├── extracted.txt            # OCR / text extraction output
│   │   └── manifest.json            # hashes, pages, metadata
├── wiki/                             # one git working tree per tenant
│   └── <tenant>/
│       ├── .git/
│       ├── QPEDIA.md                # schema / style guide (governs agent)
│       ├── index.md                 # LLM-maintained catalog
│       ├── log.md                   # append-only events
│       ├── concepts/
│       ├── entities/
│       ├── comparisons/
│       ├── summaries/
│       └── _meta/
│           └── lint.json            # latest lint report
└── models/                           # fastembed model cache
```

All structured state — sources, jobs, sessions, audit, folder_acls, folders,
connectors, oidc_pending, and the `wiki_pages` search index — lives in
Postgres (out of band, in the `postgres` container's named volume).

### 2.2 Git repo conventions

- One commit per ingest/lint bundle, signed by `qpedia-bot <bot@qpedia>`.
- Commit message: `ingest(source=<id>): <agent summary>` or `lint: <what>`.
- Branches: `main` only in v1. Future: `review/<ingest_id>` for human-in-the-loop.
- Every wiki page has YAML frontmatter:

```yaml
---
id: <ulid>                    # stable, never reused
title: "Quarterly Revenue Model"
kind: concept | entity | comparison | summary
source_ids: [ulid, ulid]      # raw docs this page derives from
tags: [finance, forecasting]
links_out: [page/path.md, ...]
created_at: 2026-04-24T10:00:00Z
updated_at: 2026-04-24T10:00:00Z
embedding_hash: sha256:...
confidence: 0.0..1.0
---
```

### 2.3 QPEDIA.md (schema / style guide)

Lives at `wiki/QPEDIA.md`. This is the single most important prompt-engineering artifact. It is loaded into **every** agent call. Template:

```markdown
# QPEDIA.md — Wiki Style & Operations Guide

## Page kinds
- **concept/**: ideas, processes, frameworks. Title = noun phrase.
- **entities/**: people, companies, products, systems. Title = proper noun.
- **comparisons/**: "X vs Y" syntheses. Created when 2+ entities/concepts overlap.
- **summaries/**: one page per raw source. Created on ingest.

## Writing rules
- Every page is standalone-readable. Never write "see above" — link instead.
- Every claim has a source citation: `[^src:<source_id>]`.
- Prefer bullet lists over paragraphs for fact-dense content.
- Page length target: 500–2000 words. Split if longer.
- New terms introduced → create concept page → link.

## Linking
- Use `[[path/to/page.md]]` for internal links.
- Every page must have ≥2 outbound links (unless it's a leaf summary).
- When adding a fact that contradicts an existing page, add a `> ⚠ contradicts [[...]]` callout.

## Ingest protocol
1. Read the source.
2. search_wiki for related pages.
3. Create one summary page at `summaries/<source_id>.md`.
4. Update or create concept/entity pages referenced by the source.
5. Update index.md (alpha-sorted catalog).
6. Append one line to log.md.

## Forbidden
- Never rewrite content you can't derive from the sources.
- Never delete pages with external inbound links — mark deprecated instead.
- Never modify QPEDIA.md itself.
```

### 2.4 Postgres schema

All structured state lives in Postgres+pgvector. RLS isolates tenants
(per-request `SET LOCAL ROLE qpedia_app` + `set_config('qpedia.tenant',
…, true)`), and a single table — `wiki_pages` — carries both the dense
embedding (`vector(384)`) and a generated `tsvector` so hybrid search
runs as one SQL statement (see §2.6).

| Table          | Purpose |
|---|---|
| `tenants`      | Top-level (not RLS-scoped); slug PK, optional `email_domain` for IdP routing |
| `sources`      | Uploaded docs; BIGSERIAL `id`, public slug per `(tenant_id, slug)`, ACL, classification |
| `wiki_pages`   | Denormalized search index for the per-tenant git wiki; vector(384) + tsvector + `source_slugs` |
| `jobs`         | Queue for ingest/remove/lint/reembed/sync; claimed via `FOR UPDATE SKIP LOCKED` |
| `sessions`     | Cookie sessions (OIDC + Firebase) — `token_hash` PK, BYPASSRLS read path for the pre-auth lookup |
| `audit`        | Append-only log; one row per `(actor, action, target)` event |
| `folder_acls`  | Per-folder ACL rules; ancestor-walk resolution |
| `folders`      | Explicit folder nodes for the File Explorer tree, with `pinned` flag that bars AI auto-organize |
| `connectors`   | External-source configs (Confluence Cloud etc.) + cursors |
| `oidc_pending` | Short-lived PKCE handshake state (10-min TTL) |

Hybrid search blends cosine similarity (pgvector `<=>`) with
`ts_rank_cd(tsv, websearch_to_tsquery(…))`. Default alpha 0.7 (vector
weight); tunable per call. Full DDL:
`crates/qpedia-pg-store/migrations/`.

---

## 3. Rust Workspace Layout

```
qpedia/
├── Cargo.toml                    # [workspace]
├── docker-compose.yml
├── Dockerfile                    # multi-stage, produces the `app` image
├── DESIGN.md                     # this document
├── crates/
│   ├── qpedia-core/              # domain types: Source, WikiPage, Edge, Job
│   ├── qpedia-pg-store/          # all SQL: tenants/sources/jobs/sessions/audit/
│   │                             #         folder_acls/folders/connectors/
│   │                             #         oidc_pending/wiki_pages + slug helpers
│   ├── qpedia-store/             # filesystem-only: BlobStore + WikiRepoStore
│   │   ├── src/wikirepo.rs       # one git repo per tenant
│   │   └── src/blob.rs           # /data/raw
│   ├── qpedia-extract/           # OCR, PDF, docx, xlsx, pptx, html
│   ├── qpedia-llm/               # LlmProvider trait; Anthropic/OpenRouter/OpenAI-compat
│   ├── qpedia-embed/             # fastembed-rs wrapper
│   ├── qpedia-ingest/            # pipeline driver + agent runner + validator
│   ├── qpedia-retriever/         # RAG + graph walk
│   ├── qpedia-lint/              # periodic wiki health jobs
│   ├── qpedia-api/               # axum routes, WS/SSE, static serving
│   └── qpedia-cli/               # admin: reindex, lint, dump, restore
└── web/                          # SvelteKit app, built into app container
```

Key external crates:
`axum`, `tower`, `sqlx` (postgres + chrono + json + tls-rustls), `pgvector`, `gix`, `fastembed`, `tesseract`, `pdfium-render`, `pulldown-cmark`, `serde_yaml`, `ulid`, `reqwest`, `openidconnect`, `jsonwebtoken` (Firebase JWKS verify), `tokio`, `tracing`.

---

## 4. LLM Adapter

```rust
// qpedia-llm
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn complete(&self, req: CompleteReq) -> Result<CompleteResp>;
    async fn stream(&self, req: CompleteReq) -> Result<BoxStream<Token>>;
    fn name(&self) -> &str;
}

pub struct CompleteReq {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub max_tokens: u32,
    pub temperature: f32,
}
```

Impls shipped: `AnthropicDirect`, `OpenAICompatible` (vLLM/Ollama/LM Studio), `OpenRouter`. Selected via env:

```
QPEDIA_LLM=anthropic://claude-opus-4-7
QPEDIA_LLM=openai-compat://http://vllm:8000/v1#Qwen2.5-72B
QPEDIA_LLM=openrouter://anthropic/claude-3.7-sonnet
```

Embeddings kept separate and always local via `fastembed-rs` (bge-m3, 1024-dim). In-process, no network hop.

---

## 5. Ingest Pipeline

### 5.1 State machine

```
Uploaded
  → Extracting        (OCR, text extraction, metadata)
  → Extracted
  → Classifying       (language, doc type, sensitivity — cheap LLM call)
  → Classified
  → AgentDistilling   (the agent loop — see §6)
  → AgentDistilled    (diff bundle produced)
  → Validating        (deterministic checks)
  → Validated
  → Committing        (git commit + wiki_pages upsert in a tx-ish sequence)
  → Committed
  → Embedding         (fastembed on touched pages)
  → Embedded
  → Done

Any step → Failed(retryable, last_error)
Failed → Dead after max_attempts
```

Each transition is idempotent, keyed by `(source_id, phase)`. A worker crash at any point is safe: on restart, re-enter from the recorded phase.

### 5.2 Extraction (qpedia-extract)

| Input | Tool | Notes |
|---|---|---|
| PDF (text) | `pdfium-render` | Fast path. |
| PDF (scanned) | `pdfium-render` → rasterize → `tesseract` | OCR fallback when text layer empty. |
| DOCX/XLSX/PPTX | `pandoc` subprocess → markdown | Pandoc is robust; bundled in image. |
| HTML | `readability` crate → `pulldown-cmark` round-trip | Strip chrome. |
| Images | `tesseract` | With language detection. |
| Plain text / md | passthrough |  |
| Code | detect + passthrough with language tag |  |
| Email (.eml/.msg) | `mail-parser` |  |

Output: `/data/raw/<source_id>/extracted.txt` + `manifest.json` with page breaks, confidence, detected language.

### 5.3 Classification

One cheap LLM call, structured output:

```json
{
  "doc_type": "contract|report|email|slide|...",
  "language": "en",
  "sensitivity": "low|med|high",
  "hints": ["finance", "2026", "draft"]
}
```

Fed to agent as priors.

---

## 6. The Ingest Agent (novel part)

### 6.1 Tool surface

```rust
pub enum AgentTool {
    SearchWiki   { query: String, limit: u32 },
    ListPages    { prefix: String },
    ReadPage     { path: String },
    ReadSource   { source_id: String, section: Option<Range> },
    ProposeNew   { path: String, content: String, rationale: String },
    ProposePatch { path: String, new_content: String, rationale: String },
    ProposeDelete{ path: String, rationale: String },
    LinkPages    { from: String, to: String, kind: String },
    Done         { summary: String },
}
```

All `Propose*` calls stage into an in-memory `DiffBundle`; nothing touches disk until validation passes.

### 6.2 Budget & guardrails

- Max 25 tool calls per ingest (configurable).
- Max 20 page mutations per bundle.
- Bundle size cap 500KB.
- Per-page size cap 50KB.
- Agent cannot modify `QPEDIA.md`.
- Agent cannot modify files outside `wiki/`.

### 6.3 System prompt (agent)

Loaded: `QPEDIA.md` + extracted doc summary + classification + dynamic context (recent log.md entries). Then:

```
You are qpedia-bot. A new source has been ingested. Follow QPEDIA.md
exactly. Your goal: integrate this source into the wiki so a future
reader (human or LLM) can find and use its knowledge via index.md
and cross-references.

Steps (you decide the order):
1. search_wiki for topics this source touches.
2. Decide: which existing pages need updates, which new pages to create.
3. propose_* each change with clear rationale.
4. Update index.md and append one line to log.md.
5. call done() with a one-sentence summary.

Budget: 25 tool calls. Stop early if the source is redundant.
```

### 6.4 Validator (deterministic, no LLM)

Rejects bundle if any of:
- Markdown doesn't parse (`pulldown-cmark` error).
- Frontmatter missing or schema-invalid.
- A `[[wikilink]]` target doesn't exist in repo ∪ bundle.
- New pages not referenced from any other touched page (orphan).
- `index.md` not updated when new pages added.
- `log.md` not appended.
- Any propose_delete on a page with inbound links.
- ACL coherence: page's `source_ids` all resolve to existing sources.

On rejection: one retry with validator errors fed back to the agent. If still bad → `Failed(retryable=false)` and quarantine bundle for admin review.

### 6.5 Commit

1. Write files to `/data/wiki` working tree.
2. `git add -A && git commit -m "ingest(source=<id>): <summary>" --author=qpedia-bot`.
3. Upsert touched rows into the `wiki_pages` table (path / title / content / tags / source_slugs / embedding / generated tsvector).
4. Queue embedding job for touched pages.
5. Write `audit` row.

Order matters: git first (durable), `wiki_pages` upsert second (derivable from git). If the upsert fails, we retry from "Committed" — git commit is the source of truth and the index is rebuildable via the `reembed` admin job.

---

## 7. Remove / Update Pipeline

When a user deletes a source:

```
MarkDirty → find wiki pages where source_ids contains <id>
         → for each dirty page:
             if other source_ids remain → AgentReSynthesize (re-runs agent for that page only)
             else → ProposeDelete (or mark deprecated if inbound links)
         → validate → commit → re-embed
```

Same rails as ingest: diff bundle, validator, single commit.

---

## 8. Query / Retrieval Path

### 8.1 State machine

```
QueryReceived
  → Embed (bge-m3)
  → HybridSearch(Postgres wiki_pages: cosine + ts_rank_cd, top 8)
  → LoadContext: fetch full content of top pages
  → AgentDecide:
       ├─ "answer now" → Answer
       ├─ "follow links [a,b,c]" → ExpandContext → AgentDecide (depth ≤ 3)
       └─ "refine search with query X" → HybridSearch (≤ 2 refines)
  → Answer (streamed via SSE)
```

### 8.2 Retriever agent tools

```rust
pub enum RetrieverTool {
    SearchWiki  { query: String },
    ReadPage    { path: String },
    FollowLinks { from: String, filter: Option<String> },
    Answer      { markdown: String, citations: Vec<Citation> },
}
```

Budget: 10 tool calls. Hard cap: 30KB of page content loaded into context (prevents runaway graph walks).

### 8.3 Citations

Every answer cites wiki page paths + source IDs. UI renders page links (opens side panel) and source links (opens original doc).

### 8.4 Write-back (admin-gated)

If the admin enables "learn from chat", good answers can be promoted:
- User thumbs-up → appears in admin review queue.
- Admin clicks "file to wiki" → runs a mini-ingest with the Q+A as the source.

---

## 9. Linting (periodic)

Scheduled every N hours. Produces a single diff bundle like ingest.

Checks:
- **Orphans**: pages with zero inbound links (excluding index).
- **Broken links**: `[[...]]` to non-existent pages.
- **Stale claims**: sources referenced but marked deleted.
- **Contradictions**: run a contradiction-detection pass on pages sharing tags (LLM call per tag-cluster, bounded by budget).
- **Duplicates**: cosine similarity > 0.93 between page vectors.
- **Index drift**: pages not listed in index.md, or listed but don't exist.

Each lint run commits as `lint: <summary>`.

---

## 10. API Surface

All under `/api/v1`. OIDC-authenticated.

### Documents / Folders

```
POST   /sources                    multipart upload, 1-N files
GET    /sources/:id                metadata + status
DELETE /sources/:id                triggers Remove pipeline
GET    /sources/:id/original       stream raw bytes
GET    /folders/:path              list children
POST   /folders                    create
PATCH  /folders/:path              rename/move
```

### Wiki

```
GET    /wiki/pages?q=              hybrid search
GET    /wiki/pages/:path           rendered markdown + metadata
GET    /wiki/pages/:path/links     inbound + outbound
GET    /wiki/graph?root=&depth=    graph neighborhood
GET    /wiki/index                 parsed index.md
```

### Chat

```
POST   /chat/sessions              create
POST   /chat/sessions/:id/messages { content } → SSE stream
GET    /chat/sessions/:id          history
```

### Admin

```
GET    /admin/jobs                 queue state
POST   /admin/jobs/:id/retry
GET    /admin/review-queue         quarantined diff bundles
POST   /admin/review-queue/:id/approve | /reject
POST   /admin/lint/run             manual trigger
GET    /admin/metrics              LLM spend, job stats, wiki size
```

### Realtime

```
WS /ws/ingest   subscribe to ingest status for a source
SSE /chat/...   as above
```

---

## 11. Frontend UX (SvelteKit)

What shipped — see `web/src/routes/` for the current surface:

- **Sources tab** (`/`) — file-explorer tree on the left with `+`
  folder / lock-against-AI / delete-empty controls and HTML5
  drag-and-drop file moves; per-folder file pane on the right with
  upload, status chip, download, delete.
- **Wiki tab** (`/wiki`) — sidebar grouped by directory (`system`,
  `concepts/`, `summaries/`, …) → page reader at `/wiki/<path>` with
  rendered markdown and clickable `[[wikilinks]]`.
- **Search tab** (`/search`) — hybrid (pgvector + tsvector) results
  with per-hit snippet.
- **Chat tab** (`/chat`) — streaming SSE answers with citations that
  link back into the wiki.
- **Admin tab** (`/admin`, admin-only) — stalled-sources resume,
  rebuild-search-index, folder-ACL editor that reuses the same tree,
  Wiki Lint report viewer, and the connector admin.
- **Login** (`/login`) — Firebase provider buttons + optional
  enterprise SSO.

Deferred from the original sketch: a graph view of wiki page links and
an LLM spend dashboard — neither has proven necessary at current scale.

---

## 12. Auth & ACL

- OIDC login (Auth0/Okta/Entra/Keycloak — configurable). Session cookie or JWT.
- Groups come from IdP claims.
- `Source.acl` = folder ACL at upload time.
- `WikiPage.acl` = **union of source ACLs** (liberal, per earlier decision — users who can see *any* source that informed the page can see the page). Configurable to intersection.
- Chat retrieval filters by user's effective ACL before loading pages.
- Citations are filtered too: if a source the agent wanted to cite is outside user's ACL, the citation is suppressed and the answer is regenerated without that source in context.

---

## 13. Deployment

### 13.1 docker-compose.yml (cloud + on-prem)

```yaml
services:
  app:
    image: qpedia/app:${VERSION}
    env_file: .env
    environment:
      # Compose owns the DB URL so it always points at the postgres service
      # and shares one password source with it. environment: overrides env_file:.
      QPEDIA_DB_URL: postgres://qpedia_admin:${QPEDIA_DB_PASSWORD:-qpedia-dev}@postgres:5432/qpedia?sslmode=disable
    volumes:
      - ./data/wiki:/data/wiki
      - ./data/raw:/data/raw
      - ./data/models:/data/models
    ports: ["8080:8080"]
    depends_on:
      postgres: { condition: service_healthy }
    restart: unless-stopped

  postgres:
    image: pgvector/pgvector:pg17
    environment:
      POSTGRES_USER: qpedia_admin
      POSTGRES_PASSWORD: ${QPEDIA_DB_PASSWORD:-qpedia-dev}
      POSTGRES_DB: qpedia
    volumes:
      - postgres-data:/var/lib/postgresql/data
    ports: ["5432:5432"]
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U qpedia_admin -d qpedia"]
    restart: unless-stopped

  # Optional: high-fidelity PDF sidecar. Enable with `--profile marker`.
  marker:
    build: ./sidecar/marker
    profiles: [marker]
    ports: ["18081:8000"]

volumes:
  postgres-data:
```

### 13.2 Environment

```
ANTHROPIC_API_KEY=...                          # or OPENAI_API_KEY / OPENROUTER_API_KEY
QPEDIA_DB_PASSWORD=qpedia-dev                  # single source of truth for Postgres
QPEDIA_DB_URL=postgres://qpedia_admin:qpedia-dev@127.0.0.1:5432/qpedia?sslmode=disable   # local cargo run
QPEDIA_DATA_DIR=/data                          # blobs, wiki repos, model cache
QPEDIA_FIREBASE_PROJECT_ID=qpedia-acme         # enable Firebase login (optional)
# OR legacy OIDC (kept for in-place deployments):
# QPEDIA_OIDC_ISSUER=https://...
# QPEDIA_OIDC_CLIENT_ID=...
QPEDIA_WIKI_AUTHOR_NAME=qpedia-bot
QPEDIA_WIKI_AUTHOR_EMAIL=bot@qpedia.cloud
```

### 13.3 On-prem variant

Swap `QPEDIA_LLM` to point at internal vLLM; drop `ANTHROPIC_API_KEY`. That's it. Same image.

### 13.4 Backup

- Postgres → `pg_dump -Fc` hourly. Holds jobs, sources, sessions, audit, folder_acls, folders, connectors, oidc_pending, plus the `wiki_pages` search index.
- `/data/wiki/<tenant>/.git` → `git bundle` per tenant, daily.
- `/data/raw` → rsync.
- Restore order: raw → Postgres → wiki. The `wiki_pages` index is derived; if it's lost (or model changes), run the `reembed` admin job to rebuild from git.

---

## 14. Observability

- `tracing` with an OTLP exporter (traces, metrics, logs). Every ingest has a root span; every LLM call a child span with model/tokens.
- The engine ships no bundled collector, dashboards, or UI — it only exports OTLP to a shared endpoint the deployment configures (`OTEL_EXPORTER_OTLP_ENDPOINT` / `OTEL_EXPORTER_OTLP_HEADERS`), e.g. Grafana Cloud's OTLP gateway or a self-run collector. Unset means console-only logging.
- Structured audit log (Postgres `audit` table) + human-readable `wiki/log.md` (LLM-facing).

---

## 15. External connectors

`qpedia-connectors` defines the `Connector` trait; the engine ships
**Confluence Cloud** and **Google Drive** as working concrete impls plus
extension points for SharePoint Online, Slack, etc. The sync scheduler
enqueues a Sync job every `QPEDIA_SYNC_INTERVAL_SECS` for each enabled
connector whose `last_run_at` is older than `QPEDIA_SYNC_STALE_SECS`.

## 16. High-fidelity extraction sidecar (Marker)

The Rust extractor in `crates/qpedia-extract/src/pdf.rs` delegates to an
optional **Marker** sidecar (`QPEDIA_MARKER_URL`) when the pdfium text
layer is sparse (< 20 chars/page average) — and, when
`QPEDIA_MARKER_PREFER=1`, sends every PDF there first with pdfium as the
fallback on any sidecar failure. The sidecar is a thin FastAPI wrapper
around marker-pdf, packaged in the deployment overlay.

Cost: ~5 GB image, 30–90s cold start on first request (model download),
seconds-to-minutes per PDF on CPU. GPU recommended for serious
throughput.

## 17. Where to look next

- [`CHANGELOG.md`](CHANGELOG.md) — release history.
- [`AGENTS.md`](AGENTS.md) — each LLM agent in detail.
- [Project wiki](https://github.com/qern-net/qpedia/wiki) — architecture,
  configuration, self-hosting, and operating guides.

---

_End of design._
