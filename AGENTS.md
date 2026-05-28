# AGENTS.md — Qpedia Agent Reference

This document describes every LLM agent in the system: its purpose, tool surface, budget constraints, system prompt strategy, and failure modes.

---

## Overview

Qpedia uses three distinct LLM agents, each with a different scope and tool set:

| Agent | Crate | Phase | Purpose |
|---|---|---|---|
| **Ingest Agent** | `qpedia-ingest` | `AgentDistilling` | Integrates one source into the wiki |
| **Retrieval Agent** | `qpedia-retriever` | Gather phase | Collects wiki context to answer a question |
| **Lint Agent** | `qpedia-lint` | Contradiction detection | Finds contradictions within tag-clustered pages |

All agents share the same `LlmProvider` abstraction and are configured by the same `QPEDIA_LLM_*` environment variables.

---

## 1. Ingest Agent

**File:** `crates/qpedia-ingest/src/agent.rs`

### Purpose

Given one newly-extracted source document, the ingest agent reads the source, searches the existing wiki, and proposes a set of page creates and patches that integrate the source's knowledge. It produces a `DiffBundle` — a list of staged operations — which is then validated and committed as a single git commit.

### Invocation

Called by `handlers/distill.rs` after the classify phase. The source must be in `Classified` status. The agent runs synchronously within the ingest job.

### System Prompt Strategy

Two-part system prompt:
1. **`QPEDIA.md`** — the wiki's own style and operations guide, loaded from the git repo. This is the primary prompt-engineering artifact. It governs page kinds, writing rules, linking conventions, and the ingest protocol.
2. **`SYSTEM_INSTRUCTIONS`** — hardcoded protocol instructions covering the tool sequence, page format requirements, and budget limits.

The user message contains: source ID, filename, MIME type, doc type, language, and hints from the classifier.

### Tools

| Tool | Purpose |
|---|---|
| `search_wiki` | Hybrid (BM25 + vector) search over wiki pages |
| `list_pages` | List page paths under a prefix |
| `read_page` | Load full markdown of a wiki page |
| `read_source` | Load first ~12,000 chars of the source's extracted text |
| `propose_new` | Stage a new wiki page (path must not exist) |
| `propose_patch` | Stage a replacement for an existing page |
| `done` | Finalize — triggers validation and commit |

All `propose_*` calls stage into an in-memory `StagedOps` struct. Nothing touches disk until `done` is called and the validator approves.

### Budget Constraints

| Limit | Value | Configurable |
|---|---|---|
| Max turns | 18 | No (hardcoded) |
| Max staged operations | 20 | No (hardcoded) |
| Max bundle size | 500 KB | No (hardcoded) |
| Max page size | 50 KB | No (hardcoded) |
| Source snippet | 12,000 chars | No (hardcoded) |
| Tool result truncation | 6,000 chars | No (hardcoded) |

### Expected Protocol

The agent is expected to follow this sequence (enforced by the system prompt, not the code):

1. `read_source(SOURCE_ID)` — ground itself in the document
2. `search_wiki` for topics the source touches
3. `read_page` on relevant candidates
4. `propose_new` for a summary at `summaries/<SOURCE_ID>.md`
5. `propose_new` or `propose_patch` for concept/entity pages (1–3 max)
6. `propose_patch` on `index.md` to catalog new pages
7. `propose_patch` on `log.md` to append a timestamped line
8. `done(summary)` with one sentence

### Idempotency

The ingest agent is idempotent at the job level. If the job fails after the agent produces a bundle but before the git commit, the job retries from `AgentDistilling` status and the agent runs again. The validator's path-existence checks prevent duplicate page creation. The `ensure_frontmatter_id_and_timestamps` function injects stable IDs if the agent omits them.

### Failure Modes

| Failure | Handling |
|---|---|
| Agent exhausts turns without calling `done` | Job fails with error; retried up to `max_attempts` |
| Agent exceeds op budget or bundle byte cap | Job fails immediately |
| Validator rejects bundle | Job fails; error logged with details |
| LLM provider unavailable | Job fails; retried with backoff |
| No LLM configured | Source marked `Tainted`; visible in Admin panel; resumable |

---

## 2. Retrieval Agent (Gather Phase)

**File:** `crates/qpedia-retriever/src/lib.rs`

### Purpose

The retrieval agent is the first phase of answering a chat question. It uses a bounded tool loop to collect the wiki pages most relevant to the user's question. The gathered pages are then passed to a non-agentic streaming synthesis call.

### Two-Phase Design

**Phase 1 — Gather (agentic):** The agent uses tools to search and read pages. It decides when it has enough context and calls `done()`. Hard caps prevent runaway graph walks.

**Phase 2 — Synthesize (streaming):** A plain streaming completion with no tools. The gathered pages are injected into the user message. Tokens stream out via SSE.

This separation keeps latency predictable: the gather phase is fast (small completions, no streaming), and the synthesis phase streams immediately once context is ready.

### Tools

| Tool | Purpose |
|---|---|
| `search_wiki` | Hybrid search; results ACL-filtered before returning |
| `read_page` | Load a page; ACL-checked; added to gathered context |
| `follow_links` | Return outbound `[[wikilinks]]` from a page without loading body |
| `done` | Signal that enough context has been gathered |

### Budget Constraints

| Limit | Value |
|---|---|
| Max gather turns | 6 |
| Max pages loaded | 8 |
| Max context bytes | 30,000 |
| Tool result truncation | 6,000 chars |
| Synthesis max tokens | 2,048 |

### ACL Filtering

Every tool call filters results by the caller's group membership. Pages whose `source_ids` resolve to sources the user cannot read are silently excluded. System pages (`index.md`, `log.md`, `QPEDIA.md`) are readable by all authenticated users.

### Fallback

If the gather agent loads zero pages (e.g. the agent calls `done` immediately, or the LLM returns no tool calls), the retriever falls back to a single hybrid search and loads the top results directly. The synthesis phase always has at least a search result to work with.

### Failure Modes

| Failure | Handling |
|---|---|
| Gather LLM call fails | `ChatEvent::Error` streamed to client |
| Synthesis stream fails mid-response | `ChatEvent::Error` appended to stream |
| No LLM configured | API returns 400 with a clear message |
| Postgres hybrid search fails | Falls back to filesystem text search |

---

## 3. Lint Agent (Contradiction Detection)

**File:** `crates/qpedia-lint/src/lib.rs`

### Purpose

The lint agent is an optional sub-step within the lint job. It clusters wiki pages by shared tags, then asks the LLM to identify contradictions within each cluster. It does not produce a `DiffBundle` — it only reports findings.

### Invocation

Called by `Linter::run()` when an LLM provider is configured. Skipped silently if no LLM is available (the rest of the lint checks still run).

### Protocol

For each tag cluster with ≥ 2 pages:
1. Build a prompt with page excerpts (up to `PAGE_EXCERPT_CHARS = 1,500` chars each)
2. Ask the LLM to identify direct contradictions between pages
3. Parse the JSON array response
4. Add findings to the `LintReport`

### Budget Constraints

| Limit | Default | Override |
|---|---|---|
| Max clusters per lint run | 8 | `QPEDIA_LINT_MAX_CLUSTERS` |
| Max pages per cluster | 6 | `QPEDIA_LINT_MAX_PAGES_PER_CLUSTER` |
| Page excerpt | 1,500 chars | No |

### Output Format

The LLM is asked to return a JSON array:

```json
[
  {
    "pages": ["concepts/foo.md", "concepts/bar.md"],
    "summary": "foo.md states X while bar.md states the opposite"
  }
]
```

Empty array `[]` means no contradictions found.

### Failure Modes

Failures in contradiction detection are logged as warnings and do not fail the lint job. The rest of the lint report (orphans, broken links, duplicates, index drift) is always produced regardless of LLM availability.

---

## Shared Concerns

### Provider Selection

All agents use `provider_from_env()` from `qpedia-llm`. Detection order:

1. `QPEDIA_LLM_PROVIDER` if set explicitly
2. `ANTHROPIC_API_KEY` → anthropic
3. `OPENAI_API_KEY` → openai
4. `OPENROUTER_API_KEY` → openrouter
5. `QPEDIA_LLM_BASE_URL` → openai-compatible

Model is selected by `QPEDIA_LLM_MODEL` or the per-provider default.

### Tool Call Format

All agents use provider-native tool use:
- Anthropic: `tool_use` / `tool_result` content blocks
- OpenAI / OpenAI-compatible: `tool_calls` arrays

The `LlmProvider` trait normalizes these into a common `ToolCall` struct. The agent code is provider-agnostic.

### Message History

All agents maintain a full message history across turns (user → assistant → tool → assistant → …). This is passed in full on every LLM call. The history is in-memory only; it is not persisted between job retries.

### Operational Principles

- **Every agent call is bounded.** Hard turn and token limits prevent runaway costs.
- **Agents stage, they don't commit.** The ingest agent only proposes changes; the validator and commit logic are outside the agent loop.
- **Failures are visible.** Agent errors propagate to job failures, which are recorded in the job queue with the full error message. Tainted sources appear in the Admin panel.
- **No agent modifies `QPEDIA.md`.** The ingest agent's `check_path` function rejects any attempt to write to `QPEDIA.md`.

---

## Adding a New Agent

1. Define the tool set as a `Vec<ToolDef>` (see `agent.rs` for examples).
2. Write the system prompt. Load `QPEDIA.md` if the agent needs wiki context.
3. Implement the tool dispatch function (`execute_tool`).
4. Add a budget struct with hard limits.
5. Return a typed result (e.g. `DiffBundle`, `Vec<Contradiction>`) — not raw LLM text.
6. Wire into a job handler in `qpedia-ingest/src/handlers/` or a new crate.
7. Ensure failures mark the source/job as failed (not silently swallowed).
