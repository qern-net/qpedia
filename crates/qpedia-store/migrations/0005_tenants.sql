-- Multi-tenant scoping. Every user-visible resource gets a `tenant`
-- column. Existing rows are 'default' so single-tenant deployments
-- continue working unchanged. Tenant resolution lives in the auth
-- layer: User.tenant flows through into every store call.

ALTER TABLE sources       ADD COLUMN tenant TEXT NOT NULL DEFAULT 'default';
ALTER TABLE sessions      ADD COLUMN tenant TEXT NOT NULL DEFAULT 'default';
ALTER TABLE auth_pending  ADD COLUMN tenant TEXT NOT NULL DEFAULT 'default';

CREATE INDEX IF NOT EXISTS sources_tenant_folder
    ON sources(tenant, folder_path);
CREATE INDEX IF NOT EXISTS sources_tenant_status
    ON sources(tenant, status);

-- folder_acls needs a composite primary key so different tenants can
-- have overlapping folder paths. SQLite doesn't support altering a
-- primary key, so do the standard rename dance.
CREATE TABLE folder_acls_v2 (
  tenant       TEXT NOT NULL DEFAULT 'default',
  folder_path  TEXT NOT NULL,
  acl_json     TEXT NOT NULL,
  updated_at   INTEGER NOT NULL,
  updated_by   TEXT NOT NULL,
  PRIMARY KEY (tenant, folder_path)
);
INSERT INTO folder_acls_v2 (tenant, folder_path, acl_json, updated_at, updated_by)
  SELECT 'default', folder_path, acl_json, updated_at, updated_by FROM folder_acls;
DROP TABLE folder_acls;
ALTER TABLE folder_acls_v2 RENAME TO folder_acls;
