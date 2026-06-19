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

**Still to do** (the OAuth 2 target below): replace static tokens with OAuth 2
client-credentials JWTs validated against the OIDC issuer, with scope-based authorization
and per-route scope checks. The extractor seam stays the same.

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
2. **User/tenant context — forward the end-user token.** For calls made on behalf of an end
   user, forward the user's OIDC access token (both apps share the issuer), or use **RFC 8693
   token exchange** for delegation. Qpedia resolves user + tenant from the token and sets
   `set_config('qpedia.tenant', …, true)`; RLS enforces isolation. Never accept a tenant
   asserted via an unauthenticated header.
3. **Scope → authorization** mapping on `/api/v1`; reject tokens missing the required scope (403).
4. **Defense in depth:** short-lived tokens; optionally mTLS or a network policy between the
   two services; IP allowlist only as a *secondary* contro