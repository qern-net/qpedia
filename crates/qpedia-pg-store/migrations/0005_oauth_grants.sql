-- OAuth grants — durable per-(tenant, provider, scope) authorizations
-- that back the SSO-aligned connectors (Google Drive, and later
-- Microsoft SharePoint/OneDrive, GitHub). See ROADMAP "Vision threads:
-- SSO-aligned connectors".
--
-- Firebase Auth establishes *identity* but does not expose OAuth
-- refresh tokens to the backend. Durable resource access (the auto-sync
-- scheduler reading a Drive in the background) needs a refresh token,
-- obtained via a separate OAuth 2.0 authorization-code flow with
-- access_type=offline. That refresh token lives here.
--
-- subject = the user_id this grant belongs to, or '' for an
-- org/tenant-level grant (the admin connected the corporate Drive).
-- The UNIQUE constraint treats '' as a normal value so re-authorizing
-- the same (tenant, provider, scope, subject) upserts in place.

CREATE TABLE oauth_grants (
    id             BIGSERIAL PRIMARY KEY,
    tenant_id      TEXT NOT NULL REFERENCES tenants(id),
    provider       TEXT NOT NULL,                       -- google | microsoft | github
    scope          TEXT NOT NULL,                       -- e.g. drive.readonly
    subject        TEXT NOT NULL DEFAULT '',            -- user_id, or '' = org-level
    access_token   TEXT,                                -- short-lived; refreshed on demand
    refresh_token  TEXT NOT NULL,                       -- the durable credential
    expires_at     TIMESTAMPTZ,                         -- access_token expiry
    granted_by     TEXT NOT NULL,                       -- user_id who authorized
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, provider, scope, subject)
);
CREATE INDEX oauth_grants_tenant ON oauth_grants(tenant_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON oauth_grants TO qpedia_app;
GRANT USAGE, SELECT ON SEQUENCE oauth_grants_id_seq TO qpedia_app;

ALTER TABLE oauth_grants ENABLE ROW LEVEL SECURITY;

DO $$ BEGIN
    CREATE POLICY tenant_isolation ON oauth_grants FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
