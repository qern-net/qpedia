-- Domain ownership for org workspaces — Band 4.2 foundation.
-- See AUTH-DESIGN.md §0 (verification methods) and §3 (matrix).
--
-- A row records a *claim* by a workspace on a domain. `verified` flips
-- true once ownership is proven by one of:
--   verified_via = 'microsoft_entra' | 'google_workspace'  (IdP-admin)
--                | 'sso'                                    (SSO test login)
--                | 'dns'                                    (DNS TXT fallback)
--
-- A domain may be *claimed* (unverified) by several workspaces, but can
-- be *verified* by only one — enforced by a partial unique index that
-- fires at the storage layer regardless of RLS visibility, so a second
-- workspace's verify fails closed even though it can't see the first
-- workspace's row (matrix row 6).

CREATE TABLE workspace_domains (
    id                 BIGSERIAL PRIMARY KEY,
    tenant_id          TEXT NOT NULL REFERENCES tenants(id),
    domain             TEXT NOT NULL,                 -- lowercased
    verified           BOOLEAN NOT NULL DEFAULT FALSE,
    verified_via       TEXT,                          -- dns | microsoft_entra | google_workspace | sso
    verification_token TEXT,                          -- nonce for the DNS method
    verified_at        TIMESTAMPTZ,
    created_by         TEXT NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One claim per (workspace, domain).
CREATE UNIQUE INDEX workspace_domains_tenant_domain
    ON workspace_domains (tenant_id, domain);
-- A *verified* domain belongs to exactly one workspace, globally.
CREATE UNIQUE INDEX workspace_domains_verified_unique
    ON workspace_domains (domain) WHERE verified;
CREATE INDEX workspace_domains_tenant ON workspace_domains (tenant_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON workspace_domains TO qpedia_app;
GRANT USAGE, SELECT ON SEQUENCE workspace_domains_id_seq TO qpedia_app;

ALTER TABLE workspace_domains ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
    CREATE POLICY tenant_isolation ON workspace_domains FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
