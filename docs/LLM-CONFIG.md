# LLM configuration — design & remaining wiring

> How the BYOL design (model selection + per-tenant BYO credentials) is wired
> through persistence → API → runtime resolution → UI. Status: **persistence +
> registry landed; API, resolution, and UI specified here for implementation.**
> Companions: [`APPROVED-MODELS.md`](../APPROVED-MODELS.md),
> [`TASK-managed-llm-billing.md`](TASK-managed-llm-billing.md).

## Placement: AI code in OSS, gated by pvt

Per the decision (2026-06-22), "all AI is pvt" follows the **DB-layer pattern**:
the engine code lives in OSS and the pvt binary drives/gates it — **no
relocation**. So the `llm_config` table, the store (`qpedia-pg-store`), the
model registry and provider plumbing (`qpedia-llm`) **stay in the OSS engine**,
exactly like every other table and the DB layer itself. The **pvt overlay owns
only** the admin routes, the settings UI, and the entitlement/managed-tier
gating. See [`TASK-llm-pvt-gating.md`](TASK-llm-pvt-gating.md). Decisions baked
in: hosted non-approved models are **hard-blocked** (422); `openai-compatible`
is the escape hatch.

## What's already in the tree

- **Registry** — `qpedia-llm::models` (`ApprovedModel`, `approved_models()`,
  `is_approved(provider, model)`, `default_model(provider)`), mirroring
  `APPROVED-MODELS.md`. The machine-readable source of truth.
- **Config-driven provider** — `qpedia_llm::provider_from_config(&LlmConfig)`,
  a sibling of `provider_from_env` that builds a provider from explicit
  provider/model/key/base_url (any field falls back to env).
- **Persistence** — migration `0008_llm_config.sql` (RLS-isolated, pgcrypto
  encrypted key) + `PgStore::{get,resolve,set,clear}_llm_config` in
  `qpedia-pg-store::llm_config`. `get_*` returns the display view (key hint
  only); `resolve_*` decrypts for provider construction.

## Granularity & fallback (decided)

- **Org-wide (per tenant).** One `llm_config` row per tenant.
- **Additive.** No row, or a row with a NULL key, ⇒ the deployment env provider
  (today's behavior). BYOL at deploy level is always available; per-tenant BYO
  is an override, never a requirement.
- **Resolution order per request:** tenant row (decrypted) → its model, else the
  approved per-provider default → env provider when the tenant set none.

## Environment

Add to `.env.example` and deployment config:

```
# Master key for encrypting per-tenant BYO LLM API keys at rest (pgcrypto).
# Required ONLY if tenants store their own keys; generate 32+ random bytes.
# QPEDIA_SECRET_KEY=change-me-32-bytes-min
```

`QPEDIA_SECRET_KEY` stays a **plain env var** by design — the value is injected
at deploy time from the platform secret store (**AWS Secrets Manager** / **Azure
Key Vault**), not committed. The app never needs to know the source. Rotation =
fetch the new value into the env and re-encrypt rows (one-off migration).

## 1. API (mounted by the pvt overlay)

These routes are mounted by `qpedia-pvt-api` via `AppBuilder::with_routes` (the
gating layer), calling the OSS store methods + registry. All under the
authenticated `/api/v1` surface; mutations require a workspace **admin/owner**
(reuse the existing role check used by `folder-acls`) plus the pvt entitlement
check.

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/v1/models` | The approved (✅) list from the registry — for the picker. |
| GET | `/api/v1/admin/llm` | The tenant's current config (provider, model, `api_key_hint`, base_url, `has_api_key`). **Never** the key. |
| PUT | `/api/v1/admin/llm` | Set provider/model/[key]/[base_url]. Validates `is_approved`; 422 on a non-approved hosted model. |
| DELETE | `/api/v1/admin/llm` | Clear → revert to deployment provider. |
| POST | `/api/v1/admin/llm/test` | Build the provider from the *pending* config and do a 1-token completion; 200/“ok” or 400 with the provider error. |

Reference handlers (mirror `set_folder_acl` for auth + `State<AppState>`):

```rust
// GET /api/v1/models
async fn list_models() -> Json<Vec<ApprovedModelDto>> {
    Json(qpedia_llm::approved_models().map(ApprovedModelDto::from).collect())
}

// GET /api/v1/admin/llm
async fn get_llm(State(s): State<AppState>, user: User) -> Result<Json<LlmConfigDto>, ApiError> {
    let row = s.ctx.db.get_llm_config(&user.tenant).await?;
    Ok(Json(LlmConfigDto::from(row))) // None ⇒ {"source":"deployment"}
}

// PUT /api/v1/admin/llm
async fn put_llm(State(s): State<AppState>, user: User, Json(b): Json<PutLlmReq>)
    -> Result<StatusCode, ApiError>
{
    require_admin(&user)?;
    let provider = Provider::parse(&b.provider).ok_or(ApiError::unprocessable("bad provider"))?;
    if let Some(m) = &b.model {
        if !qpedia_llm::is_approved(provider, m) {
            return Err(ApiError::unprocessable("model not on the approved list"));
        }
    }
    s.ctx.db.set_llm_config(&user.tenant, &b.provider, b.model.as_deref(),
        b.api_key.as_deref(), b.base_url.as_deref(), &user.id).await?;
    Ok(StatusCode::NO_CONTENT)
}
```

Register in `core_router` (`app.rs`) next to the other `/api/v1/admin/*`
routes. `PutLlmReq { provider, model?, api_key?, base_url? }`. DTOs live in the
api crate (it already depends on both `qpedia-llm` and `qpedia-pg-store`).

## 2. Runtime resolution (chat + ingest)

The chat handler and ingest jobs currently use the process-global `ctx.llm`.
Add a resolver that prefers the tenant config:

```rust
// in qpedia-api (or a small helper on AppState)
async fn provider_for(s: &AppState, tenant: &Tenant)
    -> Result<(Arc<dyn LlmProvider>, String)>   // (provider, effective_model)
{
    if let Some(row) = s.ctx.db.resolve_llm_config(tenant).await? {
        let cfg = qpedia_llm::LlmConfig {
            provider: row.provider.clone(),
            model: row.model.clone(),
            api_key: row.api_key.clone(),
            base_url: row.base_url.clone(),
        };
        if let Some(p) = qpedia_llm::provider_from_config(&cfg)? {
            let model = row.model.unwrap_or_else(|| default_model_for(&row.provider));
            return Ok((p, model));
        }
    }
    // fall back to the global env provider + current_model()
    Ok((s.ctx.llm.clone().ok_or_else(|| anyhow!("no LLM configured"))?,
        qpedia_llm::current_model()))
}
```

- **Set the model per request:** pass the returned `effective_model` into
  `CompleteReq.model` so it applies uniformly (Anthropic reads it from the
  request; OpenAI/OpenRouter already carry it).
- **Caching: deferred for v1.** Resolve + build the provider per request
  (providers wrap a reqwest client — cheap). Revisit only if it shows up hot:
  then memoize in an `Extensions`-held `DashMap<tenant, (hash, Arc<dyn
  LlmProvider>)>` keyed by a hash of the resolved config, invalidated on `PUT`
  (adds a `dashmap` dep).
- **Ingest jobs** are per-tenant: resolve once at job start (in the `JobRunner`
  before the agent loop) and thread the provider + model into `AgentCtx`.

## 3. UI (`qpedia-pvt/web`)

A settings page (`/admin` area) with:

- a **provider** select and a **model** select populated from `GET
  /api/v1/models`, grouped cloud vs open-weight, showing role + license + a
  “BYOL: needs your key” hint for `openai-compatible`;
- an **API key** field (write-only; shows `•••• <hint>` when one is stored,
  with “Replace”/“Remove”), and an optional **Base URL** for
  `openai-compatible`;
- a **Test** button hitting `POST /api/v1/admin/llm/test`;
- a banner when no per-tenant config exists: “Using the deployment default
  (BYOL at deploy level).”

Brand overrides via CSS variables only (per `OPEN-CORE.md`); the page consumes
`@qern/qpedia-web` components. Keep secrets out of any logged client state.

## 4. Security notes

- The key is encrypted at rest (pgcrypto) **and** RLS-isolated; `QPEDIA_SECRET_KEY`
  is the deployment master key (rotate ⇒ re-encrypt rows).
- `GET` endpoints and DTOs expose only `api_key_hint`, never ciphertext or
  plaintext. Never log the key or the master key.
- Validate `is_approved` server-side on write; the picker is convenience, not
  the gate. `openai-compatible` accepts any non-empty model id (operator BYOL).

## 5. Managed-LLM tier hook

When the metered tier opens (`TASK-managed-llm-billing.md`), a tenant on the
managed plan resolves to a Qern-supplied provider instead of its own key,
restricted to `approved_models()`. The resolver above is the single seam where
that entitlement check slots in.
