# Qpedia — platform integration & SDK guide

> How an application builds **on top of** Qpedia. Companion to the
> foundational-layer section in [`README.md`](README.md#qpedia-as-a-foundational-layer),
> the architecture in [`DESIGN.md`](DESIGN.md), and the canonical
> contract in [`qpedia-openapi.yaml`](qpedia-openapi.yaml).

## 1. The foundational role

Qpedia owns the hard, amortizable parts of a retrieval system — ingestion,
extraction, classification, the LLM-authored wiki, embeddings, hybrid search,
and RAG chat — behind a small, stable surface. An application built on Qpedia
does **not** reimplement any of that; it adds its own domain logic and treats
Qpedia as the knowledge layer underneath.

Three properties make it a platform rather than just an app:

- **A versioned contract, not schema coupling.** Consumers integrate over the
  `/api/v1` HTTP boundary, defined once in `qpedia-openapi.yaml`
  (OpenAPI 3.1). They never read Qpedia's Postgres tables directly.
- **Tenancy that lines up end-to-end.** Each consumer tenant maps to a Qpedia
  workspace; both sides share the OIDC issuer, and Postgres RLS enforces
  isolation on every request — a request can only ever see its own tenant's
  rows.
- **Additive, never a hard dependency.** A consumer that loses its Qpedia
  connection keeps operating on its own data and re-syncs later.
- **BYOL — bring your own LLM.** Qpedia runs against the *deployer's* LLM
  (provider key or OpenAI-compatible / on-prem endpoint); it does not ship or
  resell inference. A consumer never supplies an LLM to Qpedia — the engine
  owner does. A metered, Qern-managed LLM option is planned for the hosted tier,
  but BYOL stays first-class. The
  validated/supported models, refreshed quarterly, are in
  [`APPROVED-MODELS.md`](APPROVED-MODELS.md).

## 2. Two ways to build on Qpedia

There are exactly two sanctioned integration modes. Pick by language and
coupling, not by preference.

| | **A — In-process overlay** | **B — External application** |
|---|---|---|
| Shape | A Rust binary that composes `qpedia_api::AppBuilder` and mounts extra routes/state into the *same* process. | A separate service (any language) that calls `/api/v1` over HTTP. |
| Coupling | Tight: shares `AppState`, `PgStore`, the wiki/blob stores, embedder, RLS pool. | Loose: only the HTTP contract. |
| Use when | You are extending the engine itself with first-party features (billing, SSO, admin) **or** you want to reuse the wiki/graph/embeddings primitives directly. | You are a distinct product with its own lifecycle, deploy cadence, and (often) language. |
| Auth | In-process; no token needed between overlay and core. | Service-token or OAuth 2 client-credentials (§4). |
| Reference consumer | **qpedia-pvt** (the SaaS overlay: billing, SAML/SCIM, premium connectors). | **qproc / qpedia-rfp** (RFP assembly), **qcodia** (code knowledge layer). |

### 2.1 Mode A — the `AppBuilder` overlay

The engine exposes its whole application as a composable builder
(`crates/qpedia-api/src/app.rs`). An overlay binary stays *flat* — additive
composition, never a `main.rs` fork:

```rust
qpedia_api::AppBuilder::from_env().await?
    .with_state_extension(my_service)     // typed value in AppState.extensions
    .with_routes(my_router())             // merge a Router<AppState>
    .with_event_sink(my_sink)             // fires after every db.write_audit
    .with_tenant_hook(my_hook)            // fires after every db.upsert_tenant
    .serve().await
```

Overlay handlers take `State<AppState>` and pull their own service from
`state.extensions.get::<T>()`; they read `state.ctx` (DB, blob, wiki, embedder)
and `state.auth` directly. Because the overlay shares the RLS-aware pool, a
new overlay-owned table inherits the tenant-isolation guarantees as long as it
follows the steering rules (PostgreSQL, `FORCE ROW LEVEL SECURITY`, `BIGINT`
PK + `external_id UUID` where externally referenced). This is the only mode that can reuse the
git-backed **wiki** (`WikiRepoStore`) and **summary embeddings** without going
over the wire.

### 2.2 Mode B — the external application

A separate service speaks only the contract: `/api/v1` for ingest → search →
chat, authenticated per §4. It runs its own database (its own Postgres
*schema*, even in the same instance) and never reaches into Qpedia's tables.
`qproc` was the first external application and authored the original contract;
`qcodia`'s resident adapters are external callers of the hub in the same way.

## 3. The contract is the source of truth for the SDK

`qpedia-openapi.yaml` is the canonical, versioned description of the
external surface. **Generate clients from it; do not hand-write HTTP calls.**

- **TypeScript:** `openapi-typescript` for types, or `openapi-generator` /
  `orval` for a full client. Publish as `@qern/qpedia-client`.
- **Java:** `openapi-generator` (`java`/`spring` generators).
- **Rust:** `progenitor` or `openapi-generator`; in-process Mode-A overlays
  skip this and call the crates directly.

Regenerate the SDK whenever the contract's `info.version` bumps. The contract
is *derived from the engine source* (handlers in
`crates/qpedia-api/src/routes.rs`); when handlers change, the PR updates the
contract in the same change, and downstream consumers re-pull and regenerate.

## 4. Authentication & identity (the ceremony)

Qpedia reads three credential types via the `User` extractor
(`crates/qpedia-api/src/auth.rs`). All three resolve to the **same** identity
shape — a tenant + groups — which is what drives RLS. There is deliberately no
bare API-key path: a credential that carries no tenant cannot set the
`qpedia.tenant` GUC that isolation depends on.

| Credential | For | How |
|---|---|---|
| Session cookie (`qpedia_session`) | Browser users | Minted by OIDC/Firebase login; opaque, sha256-hashed at rest. |
| **External auth (M2M)** | Service-to-service | Any non-interactive credential (service token, OAuth 2 client-credentials JWT, etc.). The concrete scheme is supplied by the deployment overlay via the `ExternalAuthProvider` trait; the OSS engine itself ships no implementation. The provider must return a `User` with resolved tenant + groups so RLS scoping is identical to a session. |

**On-behalf-of a user.** For calls a consumer makes for a specific end user,
forward the user's OIDC access token (both apps share the issuer) or use
**RFC 8693 token exchange** for delegation. Qpedia resolves user + tenant from
the token and `set_config('qpedia.tenant', …, true)`; RLS does the rest.
**Never** accept a tenant asserted through an unauthenticated header.

**Scopes** map to authorization on `/api/v1`; a token missing the required
scope is rejected with `403`:

| Scope | Grants |
|---|---|
| `qpedia.sources.write` | Upload / replace / delete sources (ingestion). |
| `qpedia.search.read` | Hybrid search + wiki read. |
| `qpedia.chat` | RAG chat (SSE). |

OAuth 2 env knobs are now overlay-specific (e.g. `qpedia-pvt` defines
`QPEDIA_M2M_OIDC_ISSUER`, `QPEDIA_M2M_AUDIENCE`, etc.). The OSS engine has no
M2M-specific env vars — overlays register their provider via
`AppBuilder::with_auth_provider()`.
Defense in depth: short-lived tokens; optional mTLS / network policy between
services; IP allowlists only as a *secondary* control, never primary.

## 5. API ceremony

**Versioning.** The surface is `/api/v1`. Additive changes only within a major
(new optional fields, new endpoints); any breaking change bumps to `/api/v2`
and the contract's major version. Consumers pin the contract version they
generated against.

**Errors.** Standard HTTP status codes: `401` no/invalid credential, `403`
insufficient scope or cross-tenant denial (RLS), `404` not found *within the
caller's tenant*, `409` conflict, `429` rate-limited, `5xx` engine fault. Treat
`401/403` as terminal (fix auth), `429/5xx` as retryable with backoff.

**Chat is streaming.** `POST /api/v1/chat` returns Server-Sent Events; read it
as a stream, not a single JSON body.

**Rate limits.** Chat is governed by a per-tenant token-bucket limiter
(`ChatRateLimiter`, overridable in Mode A via `with_chat_rate_limiter` — e.g. a
Redis-backed limiter across replicas). Honor `429` + backoff.

**Idempotency & sizes.** Re-ingesting unchanged content is deduplicated
downstream; uploads are bounded by the multipart limit (default 256 MiB,
overlay-tunable via `upload_limit_bytes`).

## 6. Onboarding a new consumer

1. **Register the client** in the shared IdP as a confidential OAuth 2 client;
   add its `azp` to `QPEDIA_M2M_ALLOWED_CLIENTS` and grant the scopes it needs.
2. **Map tenancy:** each consumer tenant ↔ a Qpedia workspace; confirm both
   sides resolve the same tenant claim.
3. **Generate the SDK** from `qpedia-openapi.yaml` at a pinned version.
4. **Wire graceful degradation:** the consumer must keep working (degraded)
   when Qpedia is unreachable, and re-sync on recovery.
5. **Add yourself** to the consumers table in §8 below.

## 7. Worked example (Mode B, curl)

```bash
# 0. Obtain a client-credentials token from the shared IdP (out of band):
TOKEN=$(curl -s "$IDP/oauth2/token" \
  -d grant_type=client_credentials -d scope="qpedia.sources.write qpedia.search.read qpedia.chat" \
  -u "$CLIENT_ID:$CLIENT_SECRET" | jq -r .access_token)

# 1. Ingest a document
curl -X POST https://qpedia.cloud/api/v1/sources \
  -H "Authorization: Bearer $TOKEN" -F file=@./report.pdf

# 2. Hybrid search the resulting wiki
curl -G https://qpedia.cloud/api/v1/wiki/search \
  -H "Authorization: Bearer $TOKEN" --data-urlencode q="revenue recognition"

# 3. RAG chat (SSE stream)
curl -N -X POST https://qpedia.cloud/api/v1/chat \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"message":"Summarize the revenue policy"}'
```

## 8. Consumers today

| App | Mode | What it adds |
|---|---|---|
| **qpedia-pvt** | A (in-process) | SaaS overlay: billing, SAML/SCIM, premium connectors, compliance, observability sinks. |
| **qproc / qpedia-rfp** | B (external) | RFP/tender aggregation & response assembly — the first external app; authored the contract. |
| **qcodia** | B (external adapters) + A (Rust hub) | The same distilled-knowledge-layer idea for source code: symbol graph + code wiki for PR review and spec/codegen governance. Its hub is a Qpedia overlay; its resident forge adapters are external callers. |
