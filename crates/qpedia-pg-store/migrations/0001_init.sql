-- Qpedia v2 — Postgres + pgvector schema with tenant-isolating RLS.
-- See SPEC-v2.md.
--
-- Migrations run as a privileged role (qpedia_admin or postgres). At
-- runtime the app connects as qpedia_app (no BYPASSRLS) and sets the
-- `qpedia.tenant` GUC per request — RLS policies then auto-filter.

-- ---------- extensions ----------
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ---------- roles ----------
-- Runtime role (RLS-bound). Idempotent — if it already exists we just
-- ensure the grants are in place.
DO $$ BEGIN
    CREATE ROLE qpedia_app NOLOGIN;
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

GRANT USAGE ON SCHEMA public TO qpedia_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO qpedia_app;

-- ---------- tenants (top-level, no RLS) ----------
CREATE TABLE IF NOT EXISTS tenants (
    id            TEXT PRIMARY KEY,
    display_name  TEXT NOT NULL,
    email_domain  TEXT,                              -- optional auto-route by email
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    settings      JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE UNIQUE INDEX IF NOT EXISTS tenants_email_domain ON tenants(email_domain)
    WHERE email_domain IS NOT NULL;

INSERT INTO tenants(id, display_name) VALUES ('default', 'Default')
    ON CONFLICT DO NOTHING;

GRANT SELECT ON tenants TO qpedia_app;

-- ---------- sources ----------
CREATE TABLE IF NOT EXISTS sources (
    id                   TEXT PRIMARY KEY,
    tenant_id            TEXT NOT NULL REFERENCES tenants(id),
    folder_path          TEXT NOT NULL,
    filename             TEXT NOT NULL,
    mime                 TEXT NOT NULL,
    sha256               TEXT NOT NULL,
    size_bytes           BIGINT NOT NULL,
    acl                  TEXT[] NOT NULL DEFAULT '{}',
    status               TEXT NOT NULL,
    language             TEXT,
    classification       JSONB,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    ingested_at          TIMESTAMPTZ
);
CREATE INDEX IF NOT EXISTS sources_tenant_folder ON sources(tenant_id, folder_path);
CREATE INDEX IF NOT EXISTS sources_tenant_status ON sources(tenant_id, status);
CREATE INDEX IF NOT EXISTS sources_doctype ON sources((classification->>'doc_type'))
    WHERE classification IS NOT NULL;

GRANT SELECT, INSERT, UPDATE, DELETE ON sources TO qpedia_app;

-- ---------- wiki_pages (denormalized search index; canonical is the git repo) ----------
CREATE TABLE IF NOT EXISTS wiki_pages (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    path          TEXT NOT NULL,
    page_id       TEXT NOT NULL,
    kind          TEXT,
    title         TEXT,
    content       TEXT NOT NULL,
    tags          TEXT[] NOT NULL DEFAULT '{}',
    source_ids    TEXT[] NOT NULL DEFAULT '{}',
    embedding     vector(384),
    tsv           TSVECTOR
                  GENERATED ALWAYS AS (
                      setweight(to_tsvector('english', coalesce(title, '')), 'A')
                   || setweight(to_tsvector('english', content),               'B')
                  ) STORED,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, path)
);
CREATE INDEX IF NOT EXISTS wiki_pages_embedding ON wiki_pages
    USING hnsw (embedding vector_cosine_ops);
CREATE INDEX IF NOT EXISTS wiki_pages_tsv ON wiki_pages USING GIN (tsv);
CREATE INDEX IF NOT EXISTS wiki_pages_tags ON wiki_pages USING GIN (tags);
CREATE INDEX IF NOT EXISTS wiki_pages_tenant ON wiki_pages(tenant_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON wiki_pages TO qpedia_app;

-- ---------- jobs queue ----------
CREATE TABLE IF NOT EXISTS jobs (
    id            TEXT PRIMARY KEY,
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    kind          TEXT NOT NULL,
    payload       JSONB NOT NULL,
    state         TEXT NOT NULL,
    attempt       INTEGER NOT NULL DEFAULT 0,
    max_attempts  INTEGER NOT NULL DEFAULT 5,
    next_run_at   TIMESTAMPTZ NOT NULL,
    locked_by     TEXT,
    locked_until  TIMESTAMPTZ,
    last_error    TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS jobs_dispatch ON jobs(tenant_id, state, next_run_at);

GRANT SELECT, INSERT, UPDATE, DELETE ON jobs TO qpedia_app;

-- ---------- audit ----------
CREATE TABLE IF NOT EXISTS audit (
    id          BIGSERIAL PRIMARY KEY,
    tenant_id   TEXT NOT NULL REFERENCES tenants(id),
    actor       TEXT NOT NULL,
    action      TEXT NOT NULL,
    target      TEXT,
    metadata    JSONB,
    at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS audit_tenant_at ON audit(tenant_id, at DESC);

GRANT SELECT, INSERT ON audit TO qpedia_app;
GRANT USAGE ON SEQUENCE audit_id_seq TO qpedia_app;

-- ---------- sessions ----------
CREATE TABLE IF NOT EXISTS sessions (
    token_hash                    TEXT PRIMARY KEY,
    tenant_id                     TEXT NOT NULL REFERENCES tenants(id),
    user_id                       TEXT NOT NULL,
    user_email                    TEXT,
    user_name                     TEXT,
    provider                      TEXT,
    groups                        TEXT[] NOT NULL DEFAULT '{}',
    firebase_id_token_expires_at  TIMESTAMPTZ,
    expires_at                    TIMESTAMPTZ NOT NULL,
    created_at                    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS sessions_expires ON sessions(expires_at);
CREATE INDEX IF NOT EXISTS sessions_tenant ON sessions(tenant_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON sessions TO qpedia_app;

-- ---------- folder_acls ----------
CREATE TABLE IF NOT EXISTS folder_acls (
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    folder_path   TEXT NOT NULL,
    acl           TEXT[] NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by    TEXT NOT NULL,
    PRIMARY KEY (tenant_id, folder_path)
);

GRANT SELECT, INSERT, UPDATE, DELETE ON folder_acls TO qpedia_app;

-- ---------- connectors ----------
CREATE TABLE IF NOT EXISTS connectors (
    id            TEXT PRIMARY KEY,
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    kind          TEXT NOT NULL,
    name          TEXT NOT NULL,
    config_json   JSONB NOT NULL,
    cursor        TEXT,
    enabled       BOOLEAN NOT NULL DEFAULT TRUE,
    last_run_at   TIMESTAMPTZ,
    last_error    TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE INDEX IF NOT EXISTS connectors_due ON connectors(enabled, last_run_at);

GRANT SELECT, INSERT, UPDATE, DELETE ON connectors TO qpedia_app;

-- ---------- auth_pending (OIDC PKCE flight state — kept for legacy OIDC path) ----------
CREATE TABLE IF NOT EXISTS auth_pending (
    state           TEXT PRIMARY KEY,
    tenant_id       TEXT NOT NULL REFERENCES tenants(id),
    pkce_verifier   TEXT NOT NULL,
    nonce           TEXT NOT NULL,
    redirect_after  TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

GRANT SELECT, INSERT, UPDATE, DELETE ON auth_pending TO qpedia_app;
