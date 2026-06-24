# TASK: pvt gating for per-tenant LLM management

**Raised:** 2026-06-22 · **Decision:** "all AI is pvt" via the **DB-layer
pattern** — the AI engine code lives in OSS and the pvt binary drives/gates it.
**No relocation** (supersedes the earlier relocation plan). Rationale: the DB
layer (`qpedia-pg-store`, migrations, RLS) is OSS yet all runtime DB access in
the SaaS goes through the pvt binary; AI is treated identically.

## Stays in OSS (`qpedia`) — already landed, ships in the next release
- `crates/qpedia-pg-store/migrations/0008_llm_config.sql` (RLS, pgcrypto)
- `crates/qpedia-pg-store/src/llm_config.rs` (`{get,resolve,set,clear}_llm_config`)
- `crates/qpedia-llm/src/models.rs` (approved-models registry) + `provider_from_config`
- These are engine capability, exactly like every other table and the DB layer.

## In pvt (`qpedia-pvt`) — to build (the gate)
1. **Admin routes** mounted via `AppBuilder::with_routes` in `qpedia-pvt-api`:
   `GET /api/v1/models`, `GET/PUT/DELETE /api/v1/admin/llm`,
   `POST /api/v1/admin/llm/test` — calling OSS `PgStore::*_llm_config` and
   `qpedia_llm::{is_approved, approved_models, provider_from_config}`.
2. **Runtime resolution** in the pvt chat/ingest path: prefer the tenant config
   (`resolve_llm_config`) → `provider_from_config`, else the env provider.
3. **Entitlement gating** — which tenants/plans may set BYO config; the
   managed-tier seam (`TASK-managed-llm-billing.md`) restricting the managed
   provider to `approved_models()`.
4. **Settings UI** in `qpedia-pvt/web`.
5. **`QPEDIA_SECRET_KEY`** injected from AWS Secrets Manager / Azure Key Vault.

## Dependency
pvt consumes qpedia via the pinned release tag, so the OSS pieces above must be
in a tagged release before pvt builds against `provider_from_config` /
`*_llm_config`. `PgStore::pool()` + `begin_for()` are already `pub`.

Reference design for routes/resolution/UI: [`LLM-CONFIG.md`](LLM-CONFIG.md).
