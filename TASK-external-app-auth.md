# TASK: Authenticate external application calls to `/api/v1` (OAuth 2)

**Raised:** 2026-06-19 · **Driver:** `qproc` (RFP Assembly Platform) is the **first external
application** on the Qpedia platform and needs a sanctioned way to authenticate its
service-to-service calls to Qpedia's `/api/v1`.

## Current state (verified 2026-06-19)

The engine authenticates requests **only via the `qpedia_session` cookie** (opaque,
sha256-hashed in Postgres; `User` extractor in `auth.rs`). OIDC and Firebase differ only
in how the session is *minted*, not read. **There is no bearer / API-key / PAT path** in
either the OSS engine or the `qpedia-pvt` overlay. So external apps can only authenticate
today by carrying a session cookie obtained from a browser login — unsuitable for clean
service-to-service. This task adds the missing machine path.

## Implemented — first cut (branch `feat/external-app-auth`)

A minimal, opt-in **service-token (M2M)** path is now in `crates/qpedia-api/src/m2m.rs`
and wired into the `User` extractor (`auth.rs`):

- `QPEDIA_SERVICE_TOKENS` = JSON array of `{name, token, tenant, groups[]}`.
- Presented as `Authorization: Bearer <token>`; matched by sha256 hash (never stored/logged
  in plaintext). The token carries **tenant + groups**, so RLS scoping and ACLs behave
  exactly as for a user session — no cross-tenant leak. (This is why a bare API key was
  rejected: it carries no identity.)
- `None` unless configured, so existing deployments are unaffected.
- The RFP app's existing `Authorization: Bearer` wiring (`QPEDIA_TOKEN`) works against this
  as-is.

**OAuth 2 path — implemented (branch `feat/external-app-auth-oauth`).** `crates/qpedia-api/src/oauth.rs`
adds `OAuthVerifier`: validates a client-credentials **access-token JWT** against the OIDC
issuer's JWKS (RS256, mirroring `firebase.rs`), enforces a client (`azp`) allowlist and an
optional required scope, and reads the tenant claim → drives RLS. Wired into the `User`
extractor after the service-token path. Opt-in via `QPEDIA_M2M_AUDIENCE` (+ issuer); `None`
otherwise. Env: `QPEDIA_M2M_OIDC_ISSUER`, `QPEDIA_M2M_AUDIENCE`, `QPEDIA_M2M_ALLOWED_CLIENTS`,
`QPEDIA_M2M_REQUIRED_SCOPE`, `QPEDIA_M2M_TENANT_CLAIM`, `QPEDIA_M2M_JWKS_URL`.

**Still to do:** per-route scope enforcement (distinct scopes for sources.write / search.read /
chat) rather than a single required scope; rotate-friendly client registry.

## Decision (target): OAuth 2 (not API-key+IP, not HMAC secret)

Three options were considered:

| Option | Verdict | Reason |
|---|---|---|
| API key + source IP allowlist | ✗ rejected as primary | IP allowlists are brittle in cloud/k8s/NAT/egress; a static key carries **no identity and no tenant**, so it can't drive RLS. Coarse, easy to leak. |
| Shared secret + signed API key (HMAC) | ✗ rejected | Better request integrity, but still no identity/tenant context, and it **duplicates** auth Qpedia already has via OIDC. |
| **OAuth 2** | ✅ chosen | Qpedia already validates OIDC JWTs (issuer, groups claim). Tokens carry identity → can set the `qpedia.tenant` GUC that RLS depends on. Standard, rot>atable, short-lived. |

The deciding factor is **tenant isolation**: Qpedia's RLS needs a trustworthy per-request
tenant identity. A bearer token supplies it; an API key does not (you'd have to trust the
caller to assert the tenant in a header — unacceptable).

## Design

1. **Service identity — OAuth 2 client-credentials grant.** `qproc` is a confidential
   client in the shared IdP (same issuer both apps already use). Qpedia validates the JWT
   (`iss`, `exp`, `aud`), checks the client (`azp`) against an allowlist, and enforces
   **scopes** per route, e.g. `qpedia.sources.write`, `qpedia.search.read`, `qpedia.chat`.
2. **User/tenant context — forward the end-user token.** For calls made on b