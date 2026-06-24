# TASK: BYOL today; metered managed-LLM (paid) later

**Raised:** 2026-06-22 · **Status:** principle in effect (BYOL); managed offering = TODO.

## The principle — Qpedia is BYOL (bring your own LLM)

Qpedia does **not** ship or resell LLM inference. Every deployment **brings its
own LLM**: you set a provider + key (or an OpenAI-compatible / on-prem endpoint)
and Qpedia calls *your* account. This is already how the engine works —
`provider_from_env()` (`qpedia-llm`) auto-detects the provider from whichever
key is present (`ANTHROPIC_API_KEY` / `OPENAI_API_KEY` / `OPENROUTER_API_KEY`),
or a `QPEDIA_LLM_BASE_URL` for vLLM / Ollama / LM Studio. With no provider
configured, ingestion stops at `Extracted` (no wiki distillation).

Why BYOL is the default and stays available:

- **Cost & sovereignty.** The customer owns the inference spend and the data
  path to the model — essential for the enterprise / on-prem / air-gapped tier
  (`docs/PRICING.md`), and it keeps the OSS engine free of a metering
  dependency.
- **Open-core clean line.** The engine never needs billing wired in to be fully
  functional. BYOL is the OSS posture; metered inference is a SaaS concern that
  lives in the `qpedia-pvt` overlay, not here.

**Invariant:** BYOL must remain a first-class, fully-supported mode on every
plan — even after the managed option opens. We never *require* customers to buy
inference from us.

## TODO — metered managed-LLM offering ("when we open that")

A future paid option where Qern supplies the LLM and meters usage, for customers
who don't want to bring a key. **Not built yet.** It belongs in `qpedia-pvt`
(billing/metering), composed over the engine via `AppBuilder` — the engine stays
BYOL.

- [ ] **Metering hook** — count tokens/requests per tenant at the `qpedia-llm`
      call boundary; emit usage events (reuse the `EventSink` path).
- [ ] **Managed provider mode** — a pvt-side provider that proxies to Qern's
      account, gated by plan/entitlement; falls back to BYOL when not entitled.
- [ ] **Billing + plan enforcement** — Stripe usage meters, quotas, overage
      handling; per-tenant budgets and rate-limit/fallback (mirror the cost
      guardrails pattern). Lands as pvt-owned tables under the steering rules
      (PostgreSQL + RLS, `BIGINT` PK + `external_id UUID`) — see
      `TASK-steering-compliance.md`.
- [ ] **Pricing** — define the metered LLM SKU in `qpedia-pvt/docs/PRICING.md`;
      surface BYOL-vs-managed choice in the UI.
- [ ] **Serve only approved models** — the managed tier offers exactly the
      models in [`APPROVED-MODELS.md`](../APPROVED-MODELS.md) (quarterly list); a
      model dropped from that list is withdrawn from the managed SKU.
- [ ] **Docs** — flip this section to "shipped"; update README + `INTEGRATION.md`.

## Pointers

- Engine LLM wiring: `crates/qpedia-llm` (`provider_from_env`), config in
  `.env.example` (`--- LLM provider ---`) and README §"LLM Provider".
- Open-core decision rule + where SaaS billing lives:
  `qpedia-pvt/docs/OPEN-CORE.md`, `qpedia-pvt/docs/ROADMAP.md` (Band 2).
