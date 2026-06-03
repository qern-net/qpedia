-- Qpedia v2 — Postgres + pgvector schema with tenant-isolating RLS
-- and Wikipedia-style slug identifiers.
-- See SPEC-v2.md.
--
-- Identifier scheme:
--   - Internal PKs are BIGSERIAL (cheap joins, single writer).
--   - User-visible identifiers are slugs, unique per tenant:
--       sources.slug      e.g. "quarterly-revenue-model"
--       wiki_pages.path   e.g. "concepts/revenue-forecasting.md"
--       connectors.name   e.g. "engineering-confluence"
--   - On slug collision the application appends -2, -3, ... until
--     Postgres reports no conflict. See pg_store::slug::unique_slug.
--
-- Migrations run as a privileged role (qpedia_admin or the default
-- POSTGRES_USER, which both bypass RLS). The application connects as
-- qpedia_app (no BYPASSRLS) and sets the `qpedia.tenant` GUC per
-- request; RLS then auto-filters everything.

-- ---------- extensions ----------
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ---------- roles ----------
DO $$ BEGIN
    CREATE ROLE qpedia_app NOLOGIN;
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

GRANT USAGE ON SCHEMA public TO qpedia_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO qpedia_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
    GRANT USAGE, SELECT ON SEQUENCES TO qpedia_app;

-- ---------- tenants (top-level, no RLS) ----------
CREATE TABLE tenants (
    id            TEXT PRIMARY KEY,             -- slug
    display_name  TEXT NOT NULL,
    email_domain  TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    settings      JSONB NOT NULL DEFAULT '{}'::jsonb
);
CREATE UNIQUE INDEX tenants_email_domain ON tenants(email_domain)
    WHERE email_domain IS NOT NULL;

INSERT INTO tenants(id, display_name) VALUES ('default', 'Default')
    ON CONFLICT DO NOTHING;

GRANT SELECT ON tenants TO qpedia_app;

-- ---------- sources ----------
CREATE TABLE sources (
    id                   BIGSERIAL PRIMARY KEY,
    tenant_id            TEXT NOT NULL REFERENCES tenants(id),
    slug                 TEXT NOT NULL,                 -- public identifier
    folder_path          TEXT NOT NULL,                 -- slugified path
    filename             TEXT NOT NULL,                 -- original display name
    mime                 TEXT NOT NULL,
    sha256               TEXT NOT NULL,
    size_bytes           BIGINT NOT NULL,
    acl                  TEXT[] NOT NULL DEFAULT '{}',
    status               TEXT NOT NULL,
    language             TEXT,
    classification       JSONB,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    ingested_at          TIMESTAMPTZ,
    UNIQUE (tenant_id, slug)
);
CREATE INDEX sources_tenant_folder ON sources(tenant_id, folder_path);
CREATE INDEX sources_tenant_status ON sources(tenant_id, status);
CREATE INDEX sources_doctype ON sources((classification->>'doc_type'))
    WHERE classification IS NOT NULL;

GRANT SELECT, INSERT, UPDATE, DELETE ON sources TO qpedia_app;
GRANT USAGE, SELECT ON SEQUENCE sources_id_seq TO qpedia_app;

-- ---------- wiki_pages (denormalized search index; canonical is the git repo) ----------
-- `path` is itself a slug path: "concepts/revenue-forecasting.md".
-- The LLM agent generates the path; the application enforces uniqueness
-- by retrying with a -2, -3, ... suffix on the path's filename portion.
CREATE TABLE wiki_pages (
    id            BIGSERIAL PRIMARY KEY,
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    path          TEXT NOT NULL,
    kind          TEXT,
    title         TEXT,
    content       TEXT NOT NULL,
    tags          TEXT[] NOT NULL DEFAULT '{}',
    source_slugs  TEXT[] NOT NULL DEFAULT '{}',           -- references sources.slug
    embedding     vector(384),
    tsv           TSVECTOR
                  GENERATED ALWAYS AS (
                      setweight(to_tsvector('english', coalesce(title, '')), 'A')
                   || setweight(to_tsvector('english', content),               'B')
                  ) STORED,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, path)
);
CREATE INDEX wiki_pages_embedding ON wiki_pages USING hnsw (embedding vector_cosine_ops);
CREATE INDEX wiki_pages_tsv       ON wiki_pages USING GIN (tsv);
CREATE INDEX wiki_pages_tags      ON wiki_pages USING GIN (tags);
CREATE INDEX wiki_pages_tenant    ON wiki_pages(tenant_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON wiki_pages TO qpedia_app;
GRANT USAGE, SELECT ON SEQUENCE wiki_pages_id_seq TO qpedia_app;

-- ---------- jobs queue ----------
CREATE TABLE jobs (
    id            BIGSERIAL PRIMARY KEY,
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
CREATE INDEX jobs_dispatch ON jobs(tenant_id, state, next_run_at);

GRANT SELECT, INSERT, UPDATE, DELETE ON jobs TO qpedia_app;
GRANT USAGE, SELECT ON SEQUENCE jobs_id_seq TO qpedia_app;

-- ---------- audit ----------
CREATE TABLE audit (
    id          BIGSERIAL PRIMARY KEY,
    tenant_id   TEXT NOT NULL REFERENCES tenants(id),
    actor       TEXT NOT NULL,
    action      TEXT NOT NULL,
    target      TEXT,
    metadata    JSONB,
    at          TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX audit_tenant_at ON audit(tenant_id, at DESC);

GRANT SELECT, INSERT ON audit TO qpedia_app;
GRANT USAGE, SELECT ON SEQUENCE audit_id_seq TO qpedia_app;

-- ---------- sessions ----------
CREATE TABLE sessions (
    token_hash                    TEXT PRIMARY KEY,        -- sha256 of opaque cookie
    tenant_id                     TEXT NOT NULL REFERENCES tenants(id),
    user_id                       TEXT NOT NULL,           -- "firebase:<uid>" | "dev:admin"
    user_email                    TEXT,
    user_name                     TEXT,
    provider                      TEXT,                    -- google.com | github.com | ...
    groups                        TEXT[] NOT NULL DEFAULT '{}',
    firebase_id_token_expires_at  TIMESTAMPTZ,
    expires_at                    TIMESTAMPTZ NOT NULL,
    created_at                    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sessions_expires ON sessions(expires_at);
CREATE INDEX sessions_tenant  ON sessions(tenant_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON sessions TO qpedia_app;

-- ---------- folder_acls ----------
CREATE TABLE folder_acls (
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    folder_path   TEXT NOT NULL,                            -- slugified
    acl           TEXT[] NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by    TEXT NOT NULL,
    PRIMARY KEY (tenant_id, folder_path)
);

GRANT SELECT, INSERT, UPDATE, DELETE ON folder_acls TO qpedia_app;

-- ---------- connectors ----------
CREATE TABLE connectors (
    id            BIGSERIAL PRIMARY KEY,
    tenant_id     TEXT NOT NULL REFERENCES tenants(id),
    kind          TEXT NOT NULL,                            -- confluence | gdrive | sharepoint
    name          TEXT NOT NULL,                            -- slugified
    config_json   JSONB NOT NULL,
    cursor        TEXT,
    enabled       BOOLEAN NOT NULL DEFAULT TRUE,
    last_run_at   TIMESTAMPTZ,
    last_error    TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, name)
);
CREATE INDEX connectors_due ON connectors(enabled, last_run_at);

GRANT SELECT, INSERT, UPDATE, DELETE ON connectors TO qpedia_app;
GRANT USAGE, SELECT ON SEQUENCE connectors_id_seq TO qpedia_app;
