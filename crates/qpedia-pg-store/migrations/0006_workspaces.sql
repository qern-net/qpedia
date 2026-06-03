-- Team/org workspaces — Band 4.1. See AUTH-DESIGN.md.
--
-- A workspace IS a tenant (the `tenants` table). These two tables add
-- membership and invites so a user can belong to several workspaces and
-- be invited into one by email. Org creation + invites are the only ways
-- to join a workspace other than your own individual `u-<uid>` — no
-- domain magic (that's the domain-verified flow in 4.2/4.3).
--
-- RLS note: both tables are tenant-isolated for the *scoped* operations
-- (list this workspace's members/invites). Two operations are inherently
-- cross-tenant and run on the admin pool (BYPASSRLS), keyed on a
-- capability, exactly like session lookup:
--   * "list my workspaces" — query workspace_members by user_id.
--   * "accept invite" — look up the invite by its secret token.

CREATE TABLE workspace_members (
    tenant_id   TEXT NOT NULL REFERENCES tenants(id),
    user_id     TEXT NOT NULL,                      -- "firebase:<uid>" | "dev:admin"
    email       TEXT,
    role        TEXT NOT NULL DEFAULT 'member',     -- owner | admin | member
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, user_id)
);
CREATE INDEX workspace_members_user ON workspace_members(user_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON workspace_members TO qpedia_app;

ALTER TABLE workspace_members ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
    CREATE POLICY tenant_isolation ON workspace_members FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

CREATE TABLE workspace_invites (
    id          BIGSERIAL PRIMARY KEY,
    tenant_id   TEXT NOT NULL REFERENCES tenants(id),
    email       TEXT NOT NULL,                      -- invitee (lowercased)
    role        TEXT NOT NULL DEFAULT 'member',
    token       TEXT NOT NULL UNIQUE,               -- the capability
    invited_by  TEXT NOT NULL,                      -- user_id of the inviter
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ NOT NULL,
    accepted_at TIMESTAMPTZ                         -- NULL = pending
);
CREATE INDEX workspace_invites_token  ON workspace_invites(token);
CREATE INDEX workspace_invites_tenant ON workspace_invites(tenant_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON workspace_invites TO qpedia_app;
GRANT USAGE, SELECT ON SEQUENCE workspace_invites_id_seq TO qpedia_app;

ALTER TABLE workspace_invites ENABLE ROW LEVEL SECURITY;
DO $$ BEGIN
    CREATE POLICY tenant_isolation ON workspace_invites FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
