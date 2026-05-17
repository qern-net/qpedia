-- Tenant isolation enforced by Postgres RLS.
--
-- Every tenant-scoped table gets a single FOR ALL policy that compares
-- the row's tenant_id to the `qpedia.tenant` GUC. The application sets
-- the GUC per request before issuing any query — `current_setting(...,
-- true)` returns NULL when unset, which causes every policy to fail
-- closed (no rows match, no writes succeed). Cross-tenant data leaks
-- are not an application bug we can ship; they would be a Postgres bug.
--
-- Roles with BYPASSRLS (e.g. qpedia_admin during migrations or
-- qpedia-migrate cross-tenant operations) skip these policies entirely.

ALTER TABLE sources       ENABLE ROW LEVEL SECURITY;
ALTER TABLE wiki_pages    ENABLE ROW LEVEL SECURITY;
ALTER TABLE jobs          ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit         ENABLE ROW LEVEL SECURITY;
ALTER TABLE sessions      ENABLE ROW LEVEL SECURITY;
ALTER TABLE folder_acls   ENABLE ROW LEVEL SECURITY;
ALTER TABLE connectors    ENABLE ROW LEVEL SECURITY;

DO $$ BEGIN
    CREATE POLICY tenant_isolation ON sources       FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
    CREATE POLICY tenant_isolation ON wiki_pages    FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
    CREATE POLICY tenant_isolation ON jobs          FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
    CREATE POLICY tenant_isolation ON audit         FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
    CREATE POLICY tenant_isolation ON sessions      FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
    CREATE POLICY tenant_isolation ON folder_acls   FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;

DO $$ BEGIN
    CREATE POLICY tenant_isolation ON connectors    FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
