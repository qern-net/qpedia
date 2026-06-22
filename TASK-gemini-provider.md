# TASK: Native Google Gemini provider

**Raised:** 2026-06-22 · **Decision:** add a **native Gemini provider** rather
than routing Google models through OpenRouter. If no reliable Rust crate exists,
**build and publish a standalone one** as its own project.

## Why native
The approved list carries `google/gemini-3-pro` / `google/gemini-3.5-flash` only
via OpenRouter today. A first-class provider removes the aggregator hop, gives
direct control of auth/streaming/safety settings, and lets Gemini be a primary
BYOL option.

## Approach
1. **Evaluate crates** — survey current Rust SDKs for the Gemini API
   (`generativelanguage.googleapis.com`). Acceptance bar: actively maintained,
   supports streaming (SSE), system instructions, and the current Gemini 3.x
   models; permissive license.
2. **If a reliable crate exists:** wrap it behind the `LlmProvider` trait as a
   new `qpedia-llm` module `gemini.rs` (mirror `anthropic.rs` /
   `openai_compatible.rs`): `name()`, `complete()`, `stream()`, and `vision()`.
   Add `Provider::Gemini` to the registry; env: `GEMINI_API_KEY`,
   `QPEDIA_LLM_PROVIDER=gemini`, optional `GEMINI_BASE_URL`.
3. **If none is reliable:** create a standalone crate (new repo, e.g.
   `qern-net/gemini-rs`, Apache-2.0) implementing the minimal client we need
   (auth, generateContent, streamGenerateContent, function calling), publish to
   crates.io, then depend on it from `qpedia-llm`.
4. **Wire-through:** Gemini carries the model per request (set `CompleteReq.model`
   from the resolved/approved model), consistent with the other providers.
5. **Note (open-core):** a *provider* is neutral engine plumbing → OSS
   `qpedia-llm`. The Gemini *models in the approved list* are managed by the pvt
   feature (see `TASK-llm-pvt-relocation.md`).

## Acceptance
- `QPEDIA_LLM_PROVIDER=gemini` + `GEMINI_API_KEY` runs the full pipeline
  (distill + chat) against a Gemini 3.x model.
- Streaming works (chat SSE).
- Registry updated; APPROVED-MODELS.md changelog entry (per FEATURE-LANDING.md).
