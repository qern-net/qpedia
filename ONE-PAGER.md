# Qpedia — LLM-maintained enterprise knowledge base

*Drop documents in. The model writes the wiki. Ask anything.*

## What it is

A Rust-built enterprise document store that implements Karpathy's
"LLM wiki" idea: an LLM ingests each uploaded document **once** and
writes a structured wiki of markdown pages. The wiki **is** the
semantic layer — queries hit the already-distilled wiki, not raw
chunks.

## Why it's different

- **The wiki compounds.** Each new source can touch 10+ existing pages —
  cross-references, syntheses, corrections — maintained in place by
  the model. Knowledge accumulates instead of being repeatedly
  rediscovered at query time.
- **It's human-inspectable.** Pages are markdown files in a real git
  repo. Browse them as a normal wiki, diff them, blame them,
  `git clone` them, push them to GitHub. No vendor lock-in on the
  artifact.
- **Synthesis is paid once, not per query.** Cost scales with
  ingestion, not usage. Predictable LLM bills.

## How it works

```
Upload → Extract → Classify → Distill (agent) → Validate → Commit → Embed → Done
  ↑          ↑          ↑           ↑              ↑          ↑       ↑
SharePoint  PDF/DOCX  doc_type    multi-page    markdown   single   bge-small
folder UI   /HTML/OCR  language   tool loop     parses    git      → Postgres
                       hints      writes new    + ACL     commit    (pgvector
                                  + patches     check                + tsvector)
                                  existing
```

Queries run through an **agentic chat retriever**: hybrid search
(BM25 + vector in a single SQL statement) → graph-walk over wiki
page `[[wikilinks]]` → streaming SSE answer with inline citations.

## Identifiers, Wikipedia-style

User-facing identifiers are **slugs**, not opaque UUIDs:

```
/sources/quarterly-revenue-model         ← uploaded "Quarterly Revenue Model.pdf"
/wiki/concepts/revenue-forecasting       ← LLM-authored concept page
/finance/q4-reports/                     ← folder, also slugified
```

Internal primary keys are `BIGSERIAL` (cheap joins, single writer);
the slug is the public handle, unique per tenant. Collisions resolved
by appending `-2`, `-3`, ... probed against Postgres before insert.
URLs never carry a numeric id.

## Deployment

| | |
|---|---|
| **Footprint** | 2 containers (Rust app + Postgres with pgvector). Git, OCR, pandoc, pdfium, fastembed, and the SvelteKit SPA all live inside the app image. |
| **Optional 3rd container** | Marker sidecar for high-fidelity PDFs (tables, multi-column, equations). `docker compose --profile marker up`. |
| **Database** | PostgreSQL 17 + pgvector (HNSW) + tsvector (BM25-ish). Row-Level Security policies enforce tenant isolation at the database, not the application — bugs can't leak across tenants. |
| **Auth** | Firebase Auth federation. Google / Apple / Microsoft / GitHub / X (Twitter) / Facebook + generic OIDC SSO for enterprise — all from one integration. Backend verifies Firebase ID tokens; never holds client secrets. |
| **LLM** | Pluggable: Anthropic / OpenAI / OpenRouter / OpenAI-compatible (vLLM / Ollama / LM Studio). On-prem air-gapped supported. |
| **Scale target** | 1M docs, 100 concurrent users per tenant. Multi-tenant via Postgres RLS — single shared DB, all queries auto-scoped by `qpedia.tenant` GUC. |

## Capabilities

- **Folder + upload UI** with live status (pending → extracting → classified → distilling → committed → embedded → done).
- **Wiki browser** — rendered markdown, clickable wikilinks, frontmatter inspectable.
- **Hybrid search** — `embedding <=> vector` + `ts_rank_cd(tsv, q)` weighted in a single SQL statement; falls back to a degraded fs-grep mode only when explicitly run without Postgres.
- **Agentic chat** — graph-walk retrieval, streaming answers via SSE, citations link back to source docs.
- **Auth + ACL** — Firebase ID-token exchange, dev-mode bypass for local development, per-folder ACLs (admin UI), source-union ACLs on wiki pages, RLS enforcement on every read/write.
- **Lint pass** — orphans / broken links / index drift / stale source refs / near-duplicates (single self-join on cosine distance) / LLM-driven contradictions.
- **Remove pipeline** — DELETE cascades to wiki re-synthesis + Postgres cleanup + blob deletion.
- **External connectors** — Confluence Cloud shipped; auto-sync scheduler enqueues poll jobs every N minutes; GDrive / SharePoint extension points wired.
- **Retrieval-quality eval harness** — Q/A batch + JSON report, CI-friendly exit codes.

## Tenancy in one paragraph

A single Postgres instance hosts every tenant's data, but never
returns cross-tenant rows. Every tenant-scoped table has
`tenant_id TEXT NOT NULL` and a `tenant_isolation` RLS policy
comparing it to `current_setting('qpedia.tenant', true)`. On each
request the application opens a transaction and runs
`SET LOCAL qpedia.tenant = $tenant` before any query — RLS then
auto-filters. The application connects as a role *without*
`BYPASSRLS`, so a misuse (forgotten `SET LOCAL`) fails closed: every
policy returns NULL, every row is hidden, the bug surfaces loudly
instead of leaking.

## Status

Architecture v2 specced; foundation (Stages A/B/C) committed. Stage D
(cutover from the legacy stores to Postgres) is the remaining heavy
commit — mechanical edits across ~15 files, no design risk. Greenfield
deployment; no migration tooling needed.

**Repo layout:** 12 Rust crates, SvelteKit frontend, optional
Python sidecar. Build clean across the workspace; 3 unit tests +
6 end-to-end verify scripts; `cargo build` produces a single static
binary.
