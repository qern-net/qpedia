# Qpedia — Detailed Design

_Rust-based LLMWiki knowledge base, inspired by Karpathy's LLM Wiki proposal._
_Status: design approved, ready to build._

---

## 0. Goals & Non-Goals

**Goals**
- Enterprise document store (up to 1M docs, 100 concurrent users, single-tenant).
- Cloud-hosted default; same image ships on-prem.
- LLM-authored wiki as the semantic layer (Karpathy three-layer model: raw sources + wiki + schema).
- SharePoint-style folder/upload UX and chat-over-wiki UX, equally weighted.
- Fits in **two containers**: `app` + `weaviate`.
- All heavy lifting hidden from users.

**Non-Goals (v1)**
- Multi-tenant SaaS (design keeps the door open; not optimized for it).
- Real-time collaborative editing of wiki pages.
- Federated search across external corpora.
- Mobile app.

---

## 1. Two-Container Architecture

```
┌───────────────────────────── app (Rust) ─────────────────────────────┐
│  axum HTTP/WS API · ingest workers · retriever · linter              │
│  SQLite (jobs, sessions, audit)                                      │
│  gix-managed git repo: /data/wiki                                    │
│  filesystem volume:    /data/raw  (uploaded docs + OCR text)         │
│  fastembed-rs (bge-m3)  — embeddings in-process                      │
│  tesseract + pdfium + pandoc  — extraction tooling                   │
│  SvelteKit static assets served by axum                              │
└────────────────────────────────┬─────────────────────────────────────┘
                                 │ gRPC / REST
┌────────────────────────────────▼────── weaviate container ──────────┐
│  WikiPage · Chunk · Source · Folder classes                         │
│  Hybrid search (BM25 + vector), cross-ref graph traversal           │
└──────────────────────────────────────────────────────────────────────┘

Volumes: wiki-data, raw-data, sqlite-data, weaviate-data
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
├── wiki/                             # git working tree
│   ├── .git/
│   ├── QPEDIA.md                    # schema / style guide (governs agent)
│   ├── index.md                     # LLM-maintained catalog
│   ├── log.md                       # append-only events
│   ├── concepts/
│   ├── entities/
│   ├── comparisons/
│   ├── summaries/
│   └── _meta/
│       ├── embeddings.lock          # hash → vector version mapping
│       └── orphans.json             # linter output
└── sqlite/
    └── qpedia.db
```

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

### 2.4 Weaviate schema

```
Source {
  source_id:    text (uuid, indexFilterable)
  path:         text         # folder path for UI
  filename:     text
  mime:         text
  sha256:       text
  acl:          text[]       # group IDs allowed to read
  size_bytes:   int
  created_at:   date
  ingested_at:  date
  status:       text         # pending | extracted | ingested | failed
}

WikiPage {
  page_id:      text (ulid)
  path:         text         # wiki/concepts/foo.md
  kind:         text
  title:        text
  content:      text         # full markdown
  tags:         text[]
  source_ids:   text[]
  acl:          text[]       # derived: union of source ACLs
  links_out:    WikiPage[]   # cross-reference → graph traversal
  confidence:   number
  updated_at:   date
  vector:       [768]        # bge-m3
}

Chunk {
  chunk_id:     text
  page:        WikiPage      # back-ref
  text:         text
  position:     int
  vector:       [768]
}
# Chunks only created when a page > 2KB; most pages searched whole.

Folder {
  folder_id:    text
  path:         text
  parent:       Folder
  acl:          text[]
}
```

Hybrid search config: BM25 weight 0.3, vector 0.7. Tunable.

### 2.5 SQLite schema (app-local)

```sql
-- Jobs: durable queue, single writer (the app).
CREATE TABLE jobs (
  id            TEXT PRIMARY KEY,           -- ulid
  kind          TEXT NOT NULL,              -- ingest | remove | lint | reembed
  payload       TEXT NOT NULL,              -- json
  state         TEXT NOT NULL,              -- queued|running|done|failed|dead
  attempt       INTEGER NOT NULL DEFAULT 0,
  max_attempts  INTEGER NOT NULL DEFAULT 5,
  next_run_at   INTEGER NOT NULL,           -- unix ms
  locked_by     TEXT,                       -- worker id
  locked_until  INTEGER,
  last_error    TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX jobs_dispatch ON jobs(state, next_run_at);

-- Ingest state machine: per-source progress.
CREATE TABLE ingest_state (
  source_id     TEXT PRIMARY KEY,
  phase         TEXT NOT NULL,              -- see §5
  diff_bundle   TEXT,                       -- json: staged patches
  commit_sha    TEXT,
  started_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);

-- Audit log (separate from wiki/log.md; that one is LLM-facing).
CREATE TABLE audit (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  actor       TEXT NOT NULL,          -- user id | 'qpedia-bot'
  action      TEXT NOT NULL,
  target      TEXT,
  metadata    TEXT,
  at          INTEGER NOT NULL
);

-- Sessions (if we don't use stateless JWT).
CREATE TABLE sessions (
  token_hash  TEXT PRIMARY KEY,
  user_id     TEXT NOT NULL,
  expires_at  INTEGER NOT NULL
);

-- LLM call log (cost tracking + debugging).
CREATE TABLE llm_calls (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  job_id        TEXT,
  provider      TEXT,
  model         TEXT,
  input_tokens  INTEGER,
  output_tokens INTEGER,
  latency_ms    INTEGER,
  at            INTEGER NOT NULL
);
```

Users, groups, ACL membership → external IdP via OIDC; we cache claims per session. No user table needed in v1.

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
│   ├── qpedia-store/             # traits + impls: SQLite, Weaviate, git, fs
│   │   ├── src/sqlite.rs
│   │   ├── src/weaviate.rs       # generated client + wrapper
│   │   ├── src/wikirepo.rs       # gix-based
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
`axum`, `tower`, `sqlx` (sqlite), `gix`, `weaviate-community` (or hand-rolled gRPC), `fastembed`, `tesseract`, `pdfium-render`, `pulldown-cmark`, `serde_yaml`, `ulid`, `reqwest`, `async-openai` (for OpenAI-compat), `anthropic-sdk` or hand-rolled, `tokio`, `tracing`.

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
  → Committing        (git commit + Weaviate upsert in a tx-ish sequence)
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
3. Upsert touched `WikiPage` objects in Weaviate (including `links_out` refs).
4. Queue embedding job for touched pages.
5. Write `audit` row.

Order matters: git first (durable), Weaviate second (derivable from git). If Weaviate upsert fails, we retry from "Committed" — git commit is the source of truth.

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
  → HybridSearch(Weaviate, top 8 WikiPages + BM25)
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

## 11. Frontend UX Map (SvelteKit)

Two main surfaces:

### 11.1 Folder / upload surface (SharePoint-like)

- Tree view left, grid/list right.
- Drag-drop upload → progress bar per file → live status chip (`Extracting → Distilling → Done`).
- Right click → "Show wiki pages derived from this".
- Folder ACL editor for admins.

### 11.2 Wiki / chat surface

- **Left pane**: wiki tree (concepts/, entities/, ...) with search.
- **Center pane**: rendered page, wikilinks clickable.
- **Right pane**: chat. Answers cite pages (click → center pane jumps). Citations also link raw sources (click → opens original doc viewer).
- **Graph view** (tab): force-directed visualization of wiki page links, filtered by tag. Nice-to-have, build last.

### 11.3 Admin surface

- Job queue.
- Review queue (quarantined bundles → rendered as a git-style diff).
- Linter output.
- QPEDIA.md editor (with change history and "test on a recent ingest" dry-run).
- LLM spend dashboard.

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
    volumes:
      - wiki-data:/data/wiki
      - raw-data:/data/raw
      - sqlite-data:/data/sqlite
    ports: ["8080:8080"]
    depends_on: [weaviate]
    restart: unless-stopped

  weaviate:
    image: semitechnologies/weaviate:1.27
    volumes:
      - weaviate-data:/var/lib/weaviate
    environment:
      PERSISTENCE_DATA_PATH: /var/lib/weaviate
      DEFAULT_VECTORIZER_MODULE: none
      ENABLE_MODULES: ""
      AUTHENTICATION_ANONYMOUS_ACCESS_ENABLED: "true"
    restart: unless-stopped

volumes:
  wiki-data:
  raw-data:
  sqlite-data:
  weaviate-data:
```

### 13.2 Environment

```
QPEDIA_LLM=anthropic://claude-opus-4-7
ANTHROPIC_API_KEY=...
QPEDIA_OIDC_ISSUER=https://...
QPEDIA_OIDC_CLIENT_ID=...
QPEDIA_WEAVIATE_URL=http://weaviate:8080
QPEDIA_DATA_DIR=/data
QPEDIA_WIKI_AUTHOR=qpedia-bot <bot@qpedia.local>
```

### 13.3 On-prem variant

Swap `QPEDIA_LLM` to point at internal vLLM; drop `ANTHROPIC_API_KEY`. That's it. Same image.

### 13.4 Backup

- `/data/wiki/.git` → `git bundle` daily.
- `/data/raw` → rsync.
- `/data/sqlite/qpedia.db` → `sqlite3 .backup` hourly.
- Weaviate: built-in backup module → same target bucket/volume.
- Restore order: raw → sqlite → weaviate → wiki (last, because wiki derives nothing).

---

## 14. Observability

- `tracing` with OTLP exporter. Every ingest has a root span; every LLM call a child span with model/tokens.
- `/admin/metrics` Prometheus endpoint.
- Structured audit log (SQLite) + human-readable `wiki/log.md` (LLM-facing).

---

## 15. Build Plan (12 weeks, 2–3 engineers)

| Week | Focus | Exit criteria |
|---|---|---|
| 1 | Scaffolding, workspace, CI, Dockerfile | `cargo check` green, empty app container runs |
| 2 | Extract pipeline (PDF/docx/html/OCR), raw storage, folder API | Upload a file → extracted.txt appears |
| 3 | Weaviate schema, SQLite schema, fastembed integration | `POST /sources` ends in Embedded state for a plain-text file |
| 4 | LLM adapter, classifier step, simple RAG (no agent, no graph walk) | Chat returns grounded answer citing wiki pages |
| 5 | gix-based wiki repo, QPEDIA.md loader, commit pipeline | Manual wiki edits commit cleanly |
| 6 | Ingest agent: tool harness, validator, diff bundle, commit | End-to-end ingest of a PDF produces real wiki pages |
| 7 | Remove/update pipeline, Weaviate graph refs | Deleting a source cleans up or re-synthesizes pages |
| 8 | Retriever agent: graph walk, citations, ACL filtering | Chat follows links across ≥2 pages to answer |
| 9 | Frontend: folder view, upload, chat — usable end-to-end | Non-engineer can demo it |
| 10 | Linting job, admin review queue, audit UI | Lint produces a meaningful commit on a seeded corpus |
| 11 | Auth (OIDC), ACL wiring, rate limits, backup scripts | Multi-user demo with groups |
| 12 | Hardening: load test (1M docs / 100 users sim), on-prem image, docs | Ship MVP |

Parallelization notes: weeks 9–10 (frontend) can start at week 5 once the API shape is frozen. Linting (w10) is independent and can be done by a second engineer during w6–w8.

---

## 16. Open Items / Future Work

- Multi-tenant mode (per-tenant DB + wiki repo, namespaced Weaviate classes).
- Collaborative human editing of wiki pages with agent-assisted merge.
- External connectors (Google Drive, SharePoint, Confluence) → auto-ingest.
- Cross-language support in wiki (currently one working language).
- Fine-grained per-page ACLs beyond source-union.
- Eval harness: held-out Q&A set to track retrieval quality regressions.
- **High-fidelity extraction sidecar (Marker).** Shipped behind a docker
  compose `marker` profile (off by default). Lives in `sidecar/marker/`
  as a thin FastAPI wrapper around marker-pdf. The Rust extractor
  (`MarkerExtractor`) registers ahead of `PdfExtractor` when
  `QPEDIA_MARKER_URL` is set and falls back to pdfium on any sidecar
  failure. Activate when wiki quality on table-heavy, multi-column, or
  formula-heavy PDFs becomes the bottleneck. Cost: ~5 GB image,
  30-90 s cold start on first request (model download), seconds to
  minutes per PDF on CPU. GPU recommended for serious throughput.

---

_End of design._
