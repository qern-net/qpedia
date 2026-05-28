-- Explicit folder nodes for the File Explorer tree.
--
-- Folders are otherwise implicit (a folder "exists" because some source
-- has that folder_path). This table lets a folder exist with no files
-- (the `+ new folder` action) and carries the `pinned` attribute:
--
--   pinned = TRUE  → user-created/locked. The AI auto-organizer
--                    (classify + lint) must never auto-create, auto-delete,
--                    or auto-drop files into this folder. Files a user
--                    drags into any non-root folder are already exempt
--                    from auto-organization (which only ever acts on "/"),
--                    so the pin specifically protects the folder lifecycle
--                    and bars AI from dumping new files here.
--
-- `path` is a slugified absolute path ("/finance/q4-reports"), matching
-- the convention used by sources.folder_path and folder_acls.folder_path.

CREATE TABLE folders (
    tenant_id   TEXT NOT NULL REFERENCES tenants(id),
    path        TEXT NOT NULL,
    pinned      BOOLEAN NOT NULL DEFAULT TRUE,
    created_by  TEXT NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, path)
);
CREATE INDEX folders_tenant ON folders(tenant_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON folders TO qpedia_app;

ALTER TABLE folders ENABLE ROW LEVEL SECURITY;

DO $$ BEGIN
    CREATE POLICY tenant_isolation ON folders FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
