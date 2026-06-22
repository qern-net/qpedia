# Qpedia — approved LLM list

> The curated set of LLMs Qpedia is **validated and supported against**, for both
> cloud-provided and open-weight / self-hosted deployments. Reviewed **every
> quarter**; every change is recorded in the [changelog](#changelog) and stamped
> to the qpedia release that carried it. Companion to BYOL policy
> ([`TASK-managed-llm-billing.md`](TASK-managed-llm-billing.md)) and the LLM
> config in [`README.md`](README.md#llm-provider).

> **Code mirror:** this list has a machine-readable twin in
> `crates/qpedia-llm/src/models.rs` (`APPROVED_MODELS`), which the API and
> tenant model-validation read. Update **both** in the same PR — the
> [feature-landing checklist](FEATURE-LANDING.md) enforces it.

## What "approved" means

Qpedia is **BYOL** — any provider/model reachable through a configured provider
or an OpenAI-compatible endpoint will *technically run*. The approved list is
narrower and stronger: these are the models we **test the pipeline against**
(extraction → distillation → wiki authoring → embeddings → RAG chat) and will
support in production. Two consequences:

- **Self-host / BYOL:** you may point Qpedia at anything, but only approved
  models are supported; off-list models are "at your own risk."
- **Managed-LLM tier (when it opens):** the metered Qern-supplied option will
  serve **only** approved models. So the list is the contract for that SKU.

## Cadence & governance

- **Quarterly review** (Q1 Jan · Q2 Apr · Q3 Jul · Q4 Oct). Off-cycle additions
  are allowed for a major model launch; off-cycle **drops** for a safety,
  licensing, or deprecation event.
- A model is **Added** when it passes the inclusion bar below; **Dropped** when a
  provider deprecates it, its license changes unacceptably, or a strictly-better
  sibling supersedes it. Dropped cloud models that vendors retire are removed;
  dropped open-weight models may be kept runnable but unsupported.
- Each review's net change is appended to the [changelog](#changelog) under the
  **next qpedia version**, so a reader can see exactly what the approved set was
  at any release. The engine's per-provider **default** model constants are
  bumped to track this list (see the TODO at the bottom).

## Inclusion criteria

1. **Task quality** — faithful long-document distillation and grounded RAG
   answers; no degenerate summaries.
2. **Structured-output reliability** — stable JSON / tool calling (the agentic
   wiki loop depends on it).
3. **Context window** — ≥ 128K usable; longer preferred for big sources.
4. **License (open-weight only)** — a genuinely permissive license
   (Apache-2.0 / MIT or equivalent). Source-available-but-restricted is noted,
   not auto-approved.
5. **Provider stability & safety** — a real deprecation policy and acceptable
   safety posture; for cloud, a documented, versioned API.
6. **Cost sanity** — predictable, competitive token pricing for its tier.

Legend: ✅ approved · 🧪 trial (under evaluation, not yet supported) · ⛔ dropped.

## Approved — cloud-provided

| Provider | Model (API id) | Tier / role | Status |
|---|---|---|---|
| Anthropic | `claude-opus-4-8` | Heavy distillation / hardest sources | ✅ |
| Anthropic | `claude-sonnet-4-6` | Production sweet spot (distill + chat) | ✅ |
| Anthropic | `claude-haiku-4-5` | Fast / cheap; **engine default** | ✅ |
| OpenAI | `gpt-5.5` | Heavy reasoning / coding-grade distillation | ✅ |
| OpenAI | `gpt-5.4-mini` | Cost-efficient default-class | ✅ |
| OpenAI | `gpt-5.4-nano` | High-volume, low-latency | ✅ |
| OpenAI | `gpt-5.5-pro` | Premium reasoning | 🧪 |
| Google (via OpenRouter) | `google/gemini-3-pro` | Heavy reasoning, long context | ✅ |
| Google (via OpenRouter) | `google/gemini-3.5-flash` | Fast default-class | ✅ |
| Google (via OpenRouter) | `google/gemini-3.1-flash-lite` | High-volume, low-cost | 🧪 |

> Qpedia has native `anthropic` / `openai` / `openrouter` providers; **Gemini is
> reached via OpenRouter** (or any OpenAI-compatible proxy) until a native Google
> provider lands. OpenRouter itself is approved as an **aggregator** — the
> approval applies to the underlying model, not every model it can route to.

## Approved — open-weight / self-hosted

Run via the `openai-compatible` provider (vLLM / Ollama / LM Studio /
on-prem). License matters here because you are redistributing/operating weights.

| Model | Approx. size / context | License | Tier / role | Status |
|---|---|---|---|---|
| Qwen 3.5 (e.g. 397B-A17B) | MoE / long ctx | Apache-2.0 | General distill + chat, multilingual | ✅ |
| DeepSeek V4 | large / long ctx | MIT | Heavy reasoning distillation | ✅ |
| Llama 4 Maverick | MoE / long ctx | Llama Community* | General-purpose, high MMLU | ✅ |
| Llama 4 Scout | up to ~10M ctx | Llama Community* | Very-long-context sources | ✅ |
| Mistral Small 4 | ~24B / 256K | Apache-2.0 | Single-GPU self-host default | ✅ |
| Mistral Large 3 | large / long ctx | Apache-2.0 | Multilingual, heavier tier | ✅ |
| GLM-5.1 (Z.ai) | large / long ctx | MIT | Agentic / structured-output | 🧪 |
| Gemma 4 | small–mid | Gemma terms* | Lightweight on-prem | 🧪 |

> \* Llama Community and Gemma licenses are permissive for most commercial use
> but **not** OSI-standard Apache/MIT — review their terms before
> redistribution. Apache-2.0 / MIT entries are the safe default for resale or
> air-gapped shipping.

## Defaults (engine constants)

The engine auto-detects a provider and picks a per-provider default. As of the
Q2 2026 list these **should** be: `anthropic → claude-haiku-4-5`,
`openai → gpt-5.4-mini`, `openrouter → anthropic/claude-haiku-4-5`. Defaults are
re-pinned to this document at each quarterly review (see TODO).

## Claude partnership

A partnership application has been submitted to **Anthropic** (applied
2026-Q2). If accepted, it may enable preferred Claude pricing and/or a
sanctioned managed-Claude path for the metered LLM tier, and would make the
Anthropic models the reference target for the managed offering. **Status:
applied — pending.** Update this section and the changelog when it resolves.

## Changelog

Keep-a-Changelog style; each entry is stamped to the qpedia release that shipped
the list change. `Added` / `Dropped` / `Changed` are relative to the approved
set. Newest on top.

### [Unreleased] — Q3 2026 review (target: Jul 2026)
- **Changed (engine):** per-provider default re-pinned `openai → gpt-5.4-mini`
  (was `gpt-4.1-mini`) to match the Defaults section; `.env.example`, README,
  and the engine constant updated. `anthropic` / `openrouter` defaults unchanged.
- _Pending the Q3 review. Candidates to promote from 🧪: `gpt-5.5-pro`, `GLM-5.1`.
  Watch: MiniMax M3, Kimi K2.6 (frontier open-weight, June 2026)._

### [1.2.0] — 2026-06-02 — Q2 2026 (initial approved list)
Initial curated set established.

**Added — cloud:** `claude-opus-4-8`, `claude-sonnet-4-6`, `claude-haiku-4-5`,
`gpt-5.5`, `gpt-5.4-mini`, `gpt-5.4-nano`, `google/gemini-3-pro`,
`google/gemini-3.5-flash`; OpenRouter approved as an aggregator.

**Added — open-weight:** Qwen 3.5 (Apache-2.0), DeepSeek V4 (MIT),
Llama 4 Maverick, Llama 4 Scout, Mistral Small 4 (Apache-2.0),
Mistral Large 3 (Apache-2.0).

**Trial (🧪):** `gpt-5.5-pro`, `google/gemini-3.1-flash-lite`, GLM-5.1, Gemma 4.

**Dropped:** legacy `gpt-4.1-mini` / `gpt-4o`-class defaults retired in favor of
the GPT-5.4/5.5 line.

---

### TODO
- [x] Re-pin engine per-provider default model constants to the "Defaults"
      section above. _(Done: `openai → gpt-5.4-mini`; `DEFAULT_OPENAI_MODEL` in
      `qpedia-llm/src/lib.rs`, `.env.example`, README. CHANGELOG [Unreleased].)_
- [ ] Add a native Google/Gemini provider, or keep routing via OpenRouter.
- [ ] Resolve the Anthropic partnership; reflect any managed-Claude terms here.
- [ ] Each quarter: move 🧪 → ✅ or ⛔, append the net change under the next
      qpedia version, bump defaults.

> Model availability/names verified against vendor docs as of June 2026; treat
> each quarterly review as the point of re-verification.
