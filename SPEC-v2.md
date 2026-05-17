# Qpedia v2 — Postgres + Firebase Auth (greenfield)

Consolidation rework. Two storage subsystems collapse into one. Bespoke
OIDC plumbing collapses into Firebase Auth federation. Tenant isolation
moves from application-level WHERE clauses to Postgres RLS. Identifiers
go Wikipedia-style: BIGSERIAL internal PKs + tenant-unique slugs as the
public-facing handles.

**Greenfield.** No SQLite + Weaviate data exists in production; no
migration tooling, no compatibility shim. The codebase converges on the
new stack directly.

Status: design approved 2026-05-17, building in stages on `main`.

---

## 0. Goals

- **One database.** Postgres replaces both SQLite (jobs/sources/sessions/audit/folder_acls/connectors) and Weaviate (vectors + hybrid search + WikiPage objects).
- **Tenant isolation enforced by the database.** Postgres RLS policies, not application WHERE clauses. Application bugs can no longer leak across tenants.
- **One auth integration covers every relevant provider.** Firebase Auth handles Google / Apple / Microsoft / GitHub / X (Twitter) / Facebook + generic OIDC SSO for enterprise. Backend verifies Firebase ID tokens and mints its own session cookie.
- **Identifiers are slugs, not opaque UUIDs.** Internal PKs are `BIGSERIAL`; the user-facing handle is a Wikipedia-style slug (`quarterly-revenue-model`) unique per tenant. Slug collisions resolved by appending `-2`, `-3`, ... probed against Postgres.
- **Two containers.** `app` + `postgres` (with pgvector). Marker remains an optional third profile.

Non-goals (deferred):
- Postgres read replicas / HA pairing — the schema permits but ops is the deployer's job.
- Per-tenant database (vs. shared DB + RLS) — RLS is enough at our scale.

---

## 1. Container layout

```
┌──────────────── app (Rust) ───────────────┐
│  axum API · workers · scheduler           │
│  fastembed (in-process, bge-small)        │
│  gix-managed wiki repos in /data/wiki     │
│  raw blobs in /data/raw                   │
│  SvelteKit SPA → Firebase Auth SDK        │
│       │                                   │
│       ▼ sqlx (rls-aware, SET LOCAL tenant)│
└───────┬───────────────────────────────────┘
        │
┌───────▼─────────── postgres ──────────────┐
│  PostgreSQL 17 + pgvector                 │
│  RLS policies on every tenant-scoped table│
│  Roles: qpedia_app (RLS), qpedia_admin    │
│         (BYPASSRLS, for migrations + ops) │
└───────────────────────────────────────────┘

optional:
┌─────────────── marker ────────────────────┐
│  high-fidelity PDF sidecar                │
│  docker compose --profile marker          │
└───────────────────────────────────────────┘
```

External: Firebase Auth (Google-managed) for the IdP. The app makes JWKS
fetches but holds no client secrets — Firebase configuration lives in the
Firebase console, not in our env.

---

## 2. Postgres schema

All tables `OWNER qpedia_admin`. All tenant-scoped tables `ENABLE ROW LEVEL
SECURITY` plus a single `tenant_id` policy below.

```sql
-- Extensions
CREATE EXTENSION IF NOT EXISTS pgvector;       -- HNSW + ivfflat indexes
CREATE EXTENSION IF NOT EXISTS pgcrypto;       -- gen_random_uuid()
-- Optional: pg_search (ParadeDB) for BM25 — fall back to ts_rank_cd if absent.

-- Roles
CREATE ROLE qpedia_app NOLOGIN;
GRANT USAGE ON SCHEMA public TO qpedia_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO qpedia_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO qpedia_app;
-- Production DSN connects as qpedia_app + impersonates via SET ROLE if needed.

CREATE ROLE qpedia_admin BYPASSRLS LOGIN;       -- migrations, backups, cross-tenant ops

-- Tenants are top-level (no RLS on this table).
CREATE TABLE tenants (
    id            TEXT PRIMARY KEY,             -- slug; sanitized like in v1
    display_name  TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    settings      JSONB NOT NULL DEFAULT '{}'
);

-- Sources: the canonical user-visible record.
CREATE TABLE sources (
    id                   TEXT PRIMARY KEY,
    tenant_id            TEXT NOT NULL REFERENCES tenants(id),
    folder_path          TEXT NOT NULL,
    filename             TEXT NOT NULL,
    mime                 TEXT NOT NULL,
    sha256               TEXT NOT NULL,
    size_bytes           BIGINT NOT NULL,
    acl                  TEXT[] NOT NULL DEFAULT '{}',
    status               TEXT NOT NULL,                  -- pipeline state
    language             TEXT,
    classification       JSONB,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    ingested_at          TIMESTAMPTZ
);
CREATE INDEX sources_tenant_folder ON sources(tenant_id, folder_path);
CREATE INDEX sources_tenant_status ON sources(tenant_id, status);
CREATE INDEX sources_doctype       ON sources((classification->>'doc_type'))
    WHERE classification IS NOT NULL;

-- Wiki pages: denormalized for search. Canonical is still the git repo.
CREATE TABLE wiki_pages (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    path          TEXT NOT NULL,
    page_id       TEXT NOT NULL,                  -- ulid from frontmatter
    kind          TEXT,
    title         TEXT,
    content       TEXT NOT NULL,
    tags          TEXT[] NOT NULL DEFAULT '{}',
    source_ids    TEXT[] NOT NULL DEFAULT '{}',
    embedding     VECTOR(384),                    -- bge-small-en-v1.5
    tsv           TSVECTOR
                  GENERATED ALWAYS AS (
                      setweight(to_tsvector('english', coalesce(title, '')), 'A')
                   || setweight(to_tsvector('english', content),               'B')
                  ) STORED,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, path)
);
CREATE INDEX wiki_pages_embedding ON wiki_pages
    USING hnsw (embedding vector_cosine_ops);    -- ANN
CREATE INDEX wiki_pages_tsv       ON wiki_pages USING GIN (tsv);
CREATE INDEX wiki_pages_tags      ON wiki_pages USING GIN (tags);

-- Jobs queue, sessions, audit, folder_acls, connectors, auth_pending —
-- all tenant_id-scoped where appropriate. Same shape as today; column
-- names and types preserved so the Rust types are unchanged.

-- Sessions: opaque token, sha256-hashed at rest, tied to (tenant, user_id).
CREATE TABLE sessions (
    token_hash    TEXT PRIMARY KEY,
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    user_id       TEXT NOT NULL,                  -- "firebase:<uid>"
    user_email    TEXT,
    user_name     TEXT,
    provider      TEXT,                           -- google.com | github.com | ...
    groups        TEXT[] NOT NULL DEFAULT '{}',
    firebase_id_token_expires_at TIMESTAMPTZ,     -- track ID token lifetime
    expires_at    TIMESTAMPTZ NOT NULL,           -- session cookie lifetime
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sessions_expires ON sessions(expires_at);
```

### 2.1 RLS policy template

One policy per tenant-scoped table, all driven off the
`qpedia.tenant` GUC the app sets per request.

```sql
ALTER TABLE sources       ENABLE ROW LEVEL SECURITY;
ALTER TABLE wiki_pages    ENABLE ROW LEVEL SECURITY;
ALTER TABLE sessions      ENABLE ROW LEVEL SECURITY;
ALTER TABLE folder_acls   ENABLE ROW LEVEL SECURITY;
ALTER TABLE connectors    ENABLE ROW LEVEL SECURITY;
ALTER TABLE auth_pending  ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit         ENABLE ROW LEVEL SECURITY;

CREATE POLICY tenant_isolation ON sources FOR ALL TO qpedia_app
    USING (tenant_id = current_setting('qpedia.tenant', true))
    WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
-- identical policy on wiki_pages, folder_acls, connectors, audit
-- sessions / auth_pending: tenant_id match too
```

Application contract: every request opens a transaction (or borrows a
pooled connection) and immediately runs
`SELECT set_config('qpedia.tenant', $1, true)` with the user's tenant.
RLS rejects every cross-tenant read/write. Admin operations
(`POST /api/v1/admin/lint` etc.) still happen as `qpedia_app` and only
touch their tenant; cross-tenant maintenance uses `qpedia_admin`.

`current_setting('qpedia.tenant', true)` returns `NULL` if the GUC isn't
set — every policy then fails closed, surfacing the bug rather than
leaking.

---

## 3. Hybrid search

Postgres replaces Weaviate's `hybrid: { query, vector, alpha }` with one
parameterized SQL statement. Default `ALPHA = 0.7` (vector weight),
matching DESIGN.md §2.4.

```sql
WITH q AS (
    SELECT
        to_tsquery('english', $1) AS ts_q,           -- $1 = sanitized query
        $2::vector                 AS vec            -- $2 = embedding
)
SELECT
    p.path,
    p.title,
    left(p.content, 200) AS snippet,
    -- combined score: vector weight ALPHA, BM25 weight (1 - ALPHA)
    ($3::float * (1 - (p.embedding <=> q.vec))            -- $3 = ALPHA
        + (1 - $3::float) * ts_rank_cd(p.tsv, q.ts_q)) AS score
FROM wiki_pages p, q
WHERE p.embedding IS NOT NULL
ORDER BY score DESC
LIMIT $4;                                            -- $4 = K
```

RLS auto-filters by tenant. No `WHERE tenant_id = ...` in the query.

For "real BM25" (better than `ts_rank_cd`), swap `ts_rank_cd` for
`pg_search`'s `paradedb.score`. Same shape, different operator. Env-gated.

Near-duplicate detection (lint): a single self-join on `embedding <=>`:

```sql
SELECT a.path, b.path, 1 - (a.embedding <=> b.embedding) AS similarity
FROM wiki_pages a JOIN wiki_pages b
  ON a.id < b.id
WHERE 1 - (a.embedding <=> b.embedding) > 0.93;
```

---

## 4. Auth: Firebase federation

Firebase Auth is the only identity provider the backend integrates with.
Firebase already brokers Google / Apple / Microsoft / GitHub / X / Facebook
+ generic OIDC and SAML for enterprise SSO; we get the variety for free.

### 4.1 Flow

```
1. Browser opens /login.
2. SvelteKit page loads Firebase JS SDK + project config.
3. User clicks "Sign in with <Google|GitHub|Microsoft|X|SSO|...>".
4. Firebase handles the provider OAuth dance (popup or redirect).
5. Firebase returns an ID token (JWT) to the SPA.
6. SPA POSTs the token to:
       POST /api/v1/auth/firebase/login   { idToken }
7. Backend verifies the JWT:
   - signature: against Google's published JWKS (cached)
   - audience: equal to QPEDIA_FIREBASE_PROJECT_ID
   - issuer:   https://securetoken.google.com/<project_id>
   - exp / iat: still valid
8. Backend extracts: uid (sub), email, name, picture,
   firebase.sign_in_provider, and our custom claims (tenant_id, groups).
9. Backend resolves the user's tenant:
     a. custom claim `tenant_id` if set,
     b. else lookup by email_domain in the `email_domain_to_tenant` map,
     c. else fall back to "default" or 401 in strict mode.
10. Backend creates a row in `sessions`, sets a Secure HttpOnly cookie,
    returns 200 with { tenant, user, groups }.
11. Subsequent calls use the cookie; the existing User extractor reads
    the session and runs `set_config('qpedia.tenant', ...)` before any
    query.
```

### 4.2 Supported providers (via Firebase)

| Provider | Firebase identifier | Notes |
|---|---|---|
| Google | `google.com` | OAuth, default |
| Apple | `apple.com` | OAuth |
| Microsoft | `microsoft.com` | OAuth, Entra ID |
| GitHub | `github.com` | OAuth |
| X (Twitter) | `twitter.com` | OAuth |
| Facebook | `facebook.com` | OAuth |
| Enterprise SSO | OIDC provider | Customer configures their IdP in Firebase Console; users sign in with their own corporate creds |
| SAML SSO | SAML provider | Same — admin-time Firebase config |

### 4.3 Custom claims for tenancy + groups

Tenants and group memberships are properties of the *Firebase* user, set
via the Firebase Admin SDK (server-side) at user-provisioning time:

```ts
// admin-side script (outside qpedia)
await admin.auth().setCustomUserClaims(uid, {
    tenant_id: "acme",
    groups: ["finance-team", "admin"],
});
```

The next ID token the user obtains carries these claims. Our backend
trusts the claims because Firebase signed the token.

Fallback: if `tenant_id` claim is absent, derive from email domain
(`alice@acme.com` → `acme`) via a `tenants.email_domain` lookup.

### 4.4 Backend verification

Pure Rust, no Firebase Admin SDK:
- Fetch JWKS from `https://www.googleapis.com/service_accounts/v1/jwk/securetoken@system.gserviceaccount.com`, cache for 1h.
- Verify JWT with `jsonwebtoken` crate using RS256 + the matching JWKS key.
- Validate iss/aud/exp/iat.
- Extract claims.

### 4.5 Dev mode unchanged

`QPEDIA_FIREBASE_PROJECT_ID` unset ⇒ dev mode: every request is the
synthetic `dev:admin` user with the `admin` group, tenant from
`QPEDIA_DEV_TENANT` (default `"default"`). Smoke tests + verify scripts
keep working with zero auth setup.

---

## 5. Identifiers

Internal primary keys are `BIGSERIAL` everywhere — sources, wiki_pages,
jobs, audit, connectors. Cheap joins, sequential, single writer, no
distributed minting needed.

User-facing identifiers are Wikipedia-style slugs:
- lowercase, ASCII alphanumeric, dashes between word runs,
- capped at 80 chars,
- non-alpha runs collapsed (`"Q4 2026 / Forecast"` → `"q4-2026-forecast"`),
- empty/garbage input falls back to `"untitled"`.

Each table that exposes a slug enforces a per-tenant `UNIQUE` constraint:
- `sources       UNIQUE (tenant_id, slug)`
- `wiki_pages    UNIQUE (tenant_id, path)`
- `connectors    UNIQUE (tenant_id, name)`

On upload / page creation / connector setup the app calls a small
probing helper (`pg_store::slug::unique_*`) that issues `SELECT 1 …`
and appends `-2`, `-3`, … until Postgres reports no collision. Caller
then inserts; the `UNIQUE` constraint catches the rare race where two
concurrent uploads picked the same suffix, and the caller retries.

URLs use the slug directly: `GET /api/v1/sources/quarterly-revenue-model`.
Wiki citations stay readable: `[^src:quarterly-revenue-model]`.
Numeric ids never appear in the API surface.

## 6. Bootstrap

`docker compose up` on a fresh host:
1. `postgres` starts (pgvector/pgvector:pg17), runs initdb as `qpedia_admin`.
2. `app` runs `sqlx::migrate!` against Postgres on first boot —
   extensions + roles + tables + RLS policies install in one transaction.
3. The seeded `default` tenant row lets dev mode work without any
   tenant provisioning.

Two containers. Same `up` command. No legacy stores anywhere.

---

## 7. Operational notes

- **Backups.** `pg_dump --format=custom`. Wiki git repo backed up
  separately (it's the canonical record; Postgres `wiki_pages` is a
  cache that the Reembed job rebuilds from git).
- **TLS.** Postgres connection uses `sslmode=require` in prod;
  `sslmode=disable` in compose. SQLx handles both.
- **Connection pool.** Default 16 connections per app instance,
  configurable via `QPEDIA_DB_MAX_CONN`.
- **Migrations.** `sqlx::migrate!` runs as `qpedia_admin` (env DSN);
  app runtime queries as `qpedia_app`.
- **Search recall.** HNSW `ef_search` tunable via
  `QPEDIA_HNSW_EF_SEARCH` (default 64). Raise for better recall at
  query cost.
- **Pgvector limits.** 16 KiB per vector — our 384-dim bge-small needs
  ~1.5 KiB. Well within limits.
- **Storage.** 1M wiki pages × (avg 4KB content + 384-dim vector ≈ 1.5KB
  + index overhead) ≈ ~15 GB. Comfortable single-instance.

---

## 8. Crate-level changes

| Crate | Change |
|---|---|
| `qpedia-pg-store` (new) | All SQL: tenants, sources, sessions, jobs, audit, folder_acls, connectors, wiki_pages, hybrid_search, near-duplicate scan, tenant context setter, slug helpers. Replaces `qpedia-store::sqlite` and `qpedia-store::weaviate`. |
| `qpedia-store` | Deleted. Crates that depend on it now depend on `qpedia-pg-store` directly. |
| `qpedia-api::auth` | `FirebaseVerifier` (JWKS-cached JWT validator), `/api/v1/auth/firebase/login` route, dev-mode preserved. OIDC code dropped — Firebase covers every provider we care about. |
| `qpedia-retriever` | `Retriever::hybrid_search` calls the Postgres hybrid query. No Weaviate import. |
| `web/` | `/login` route uses Firebase JS SDK. Other routes unchanged. |
| `Cargo.toml` (workspace) | Add `sqlx-postgres`, `pgvector`, `jsonwebtoken`. Remove SQLite + Weaviate REST glue. |
| `docker-compose.yml` | `weaviate` service replaced with `postgres` (image `pgvector/pgvector:pg17`). Marker stays optional. |

---

## 9. Stages (commit boundaries)

Stage A — Foundation [done]
1. `qpedia-pg-store`: schema, migrations, role + RLS, sources/sessions/
   tenants/wiki CRUD, slug helpers.
2. `docker-compose.yml` swaps to pgvector/pgvector:pg17.

Stage B — Wiki search [done with A]
3. `wiki_pages` + `pg_hybrid_search` (BM25 + vector, single SQL) +
   `pg_near_duplicates`.

Stage C — Auth [done]
4. `FirebaseVerifier` + `/api/v1/auth/firebase/login` + frontend
   Firebase SDK on `/login` with Google / Apple / Microsoft / GitHub /
   X / Facebook + opt-in enterprise SSO.

Stage D — Cutover [pending]
5. `qpedia-api/src/main.rs` rewires every handler from `SqliteStore` /
   `WeaviateStore` to `PgStore`. `IngestContext.db` becomes `PgStore`.
   Per-request `SET LOCAL qpedia.tenant` (via `begin_for(&tenant)`).
6. Upload handler calls `slug::unique_source_slug(&store, &tenant, filename)`
   before insert; subsequent updates / reads use the slug as the public id.
7. Delete `qpedia-store` (the old crate) and the SQLite + Weaviate code.

No Stage E — greenfield, no migration tooling needed.

This document is the contract for what gets built.
