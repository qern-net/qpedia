# Qpedia — approved LLM list

> The curated set of LLMs Qpedia is **validated and supported against**, for both
> cloud-provided and open-weight / self-hosted deployments. Reviewed **every
> quarter**; every change is recorded in the [changelog](#changelog) and stamped
> to the qpedia release that carried it. Companion to the LLM
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
| Anthropic | `claude-sonnet-5` | Production sweet spot (distill + chat) | ✅ |
| Anthropic | `claude-haiku-4-5` | Fast / cheap; **engine default** | ✅ |
| OpenAI | `gpt-5.5` | Heavy reasoning / coding-grade distillation | ✅ |
| OpenAI | `gpt-5.4-mini` | Cost-efficient default-class | ✅ |
| OpenAI | `gpt-5.4-nano` | High-volume, low-latency | ✅ |
| OpenAI | `gpt-5.5-pro` | Premium reasoning | ✅ |
| Gemini (native) | `gemini-3.1-pro` | Heavy reasoning, long context | ✅ |
| Gemini (native) | `gemini-3.5-flash` | Fast default-class | ✅ |
| Gemini (native) | `gemini-3.1-flash-lite` | High-volume, low-cost | 🧪 |

> Qpedia has native `anthropic` / `openai` / `openrouter` / `gemini` providers.
> **Gemini is native** — set `QPEDIA_LLM_PROVIDER=gemini` + `GEMINI_API_KEY`; it
> uses Gemini's OpenAI-compatible endpoint (no OpenRouter hop). OpenRouter
> remains approved as an **aggregator** — the approval applies to the underlying
> model, not every model it can route to.

## Approved — open-weight / self-hosted

Run via the `openai-compatible` provider (vLLM / Ollama / LM Studio /
on-prem). License matters here because you are redistributing/operating weights.

| Model | Approx. size / context | License | Tier / role | Status |
|---|---|---|---|---|
| Qwen 3.5 (e.g. 397B-A17B) | MoE / long ctx | Apache-2.0 | General distill + chat, multilingual | ✅ |
| Qwen 3.6-27B | dense 27B / long ctx | Apache-2.0 | Efficient dense, coding-strong | 🧪 |
| DeepSeek V4 | large / long ctx | MIT | Heavy reasoning distillation | ✅ |
| Llama 4 Maverick | MoE / long ctx | Llama Community* | General-purpose, high MMLU | ✅ |
| Llama 4 Scout | up to ~10M ctx | Llama Community* | Very-long-context sources | ✅ |
| Mistral Small 4 | ~24B / 256K | Apache-2.0 | Single-GPU self-host default | ✅ |
| Mistral Large 3 | large / long ctx | Apache-2.0 | Multilingual, heavier tier | ✅ |
| GLM-5.2 (Z.ai) | 753B / 1M ctx | MIT | Agentic / structured-output, frontier-class | ✅ |
| Gemma 4 | E2B–31B | Apache-2.0 | Lightweight on-prem | ✅ |
| MiniMax M3 | MoE / 1M ctx | MiniMax Community† | Long-context multimodal coding | 🧪 |
| Kimi K2.6 (Moonshot) | 1T-A32B / 256K ctx | Modified MIT‡ | Agentic / multi-agent orchestration | 🧪 |

> \* Llama Community license is permissive for most commercial use but **not**
> OSI-standard Apache/MIT — review terms before redistribution. Gemma 4 moved
> to Apache-2.0 this review (previously the restrictive Gemma terms), so it no
> longer needs this caveat.
> † MiniMax Community license: unrestricted personal/research use, but
> commercial use requires MiniMax's authorization — **not** approved-tier
> permissive; trial only, do not ship in the managed tier.
> ‡ Modified MIT: functions as standard MIT below 100M MAU / $20M monthly
> revenue; above that, must display "Kimi K2" in the product UI. Apache-2.0 /
> MIT (unmodified) entries remain the safe default for resale or air-gapped
> shipping.

## Defaults (engine constants)

The engine auto-detects a provider and picks a per-provider default. As of the
Q3 2026 review these **remain unchanged**: `anthropic → claude-haiku-4-5`,
`openai → gpt-5.4-mini`, `openrouter → anthropic/claude-haiku-4-5`,
`gemini → gemini-3.5-flash`. (Claude Sonnet moved 4-6 → 5 and Gemini's heavy
tier moved 3-pro → 3.1-pro this review, but neither is a *default*, so no
engine constant changed.) Defaults are re-pinned to this document at each
quarterly review (see TODO).

## Claude partnership

A partnership application has been submitted to **Anthropic** (applied
2026-Q2). If accepted, it may enable preferred Claude pricing and/or a
sanctioned managed-Claude path for the metered LLM tier, and would make the
Anthropic models the reference target for the managed offering. **Status:
applied — pending** (unchanged this review; no confirmation of our specific
application found). Note for whoever owns this: Anthropic opened the **Claude
Partner Network** as free/open enrollment in 2026 (distinct from the paid
startup-credits tiers) — worth checking whether our application should be
re-routed through that program. Update this section and the changelog when it
resolves.

## Changelog

Keep-a-Changelog style; each entry is stamped to the qpedia release that shipped
the list change. `Added` / `Dropped` / `Changed` are relative to the approved
set. Newest on top.

### [Unreleased] — Q3 2026 review (Jul 2026)
- **Added (engine):** native **Gemini** provider (`QPEDIA_LLM_PROVIDER=gemini`,
  via Gemini's OpenAI-compatible endpoint). Gemini models moved off OpenRouter
  routing (`google/gemini-*`) to the native provider ids (`gemini-*`).
- **Changed (engine):** per-provider default re-pinned `openai → gpt-5.4-mini`
  (was `gpt-4.1-mini`) to match the Defaults section; `.env.example`, README,
  and the engine constant updated. `anthropic` / `openrouter` defaults unchanged.
- **Added — cloud:** `claude-sonnet-5` (Anthropic, released 2026-06-30, new
  Free/Pro default upstream, edges out Opus 4.8 on knowledge work per
  Anthropic); `gemini-3.1-pro` (Google, supersedes `gemini-3-pro` at the same
  price — doubled ARC-AGI-2 score, fixed the ~21K-token output truncation).
  Promoted 🧪→✅: `gpt-5.5-pro` (Premium reasoning; stable in eval since its
  Apr 2026 GA).
- **Dropped:** `claude-sonnet-4-6` (⛔ superseded by `claude-sonnet-5`);
  `gemini-3-pro` (⛔ superseded by `gemini-3.1-pro`).
- **Added — open-weight:** `GLM-5.2` (Z.ai, MIT, 753B/1M ctx) added ✅ directly,
  superseding and dropping the `GLM-5.1` 🧪 trial (5.2 shipped stronger and
  cheaper before 5.1 cleared evaluation). `Gemma 4` promoted 🧪→✅ **and** its
  license corrected `Gemma terms*` → **Apache-2.0** (Google moved Gemma 4 to a
  fully permissive license — no more MAU cap / AUP enforcement). New 🧪 trials:
  `Qwen 3.6-27B` (Apache-2.0, dense, beats the Qwen 3.5 flagship on coding),
  `MiniMax M3` (MiniMax Community license — restricted, trial-only, not
  approval-eligible until license changes), `Kimi K2.6` (Modified MIT —
  effectively permissive below 100M MAU / $20M mo. revenue).
- **Held back (not added):** Claude **Fable 5** — GA and benchmarks are
  strong, but Anthropic suspended access under a US export-control directive
  as of 2026-06-12 ("temporary," per Anthropic); revisit once access is
  restored. Claude **Mythos 5** — limited availability only (Project
  Glasswing), not broadly reachable. OpenAI **GPT-5.6 (Sol/Terra/Luna)** —
  announced 2026-06-26 but limited-preview/partner-only access; no general
  API yet. All three are **watch items for Q4 2026**, not table entries.

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
- [x] Add a native Google/Gemini provider, or keep routing via OpenRouter.
      _(Done: native `gemini` provider, see engine change above.)_
- [ ] Resolve the Anthropic partnership; reflect any managed-Claude terms here.
      Check whether the Claude Partner Network's open enrollment (2026)
      supersedes our pending application.
- [ ] Q4 2026: revisit Claude Fable 5 (export-control hold), GPT-5.6
      Sol/Terra/Luna (limited preview → GA?), and whether Qwen 3.6-27B,
      MiniMax M3, or Kimi K2.6 clear the bar for ✅ (MiniMax M3 needs a
      license change first).
- [ ] Each quarter: move 🧪 → ✅ or ⛔, append the net change under the next
      qpedia version, bump defaults.

> Model availability/names verified against vendor docs as of July 2026; treat
> each quarterly review as the point of re-verification.
