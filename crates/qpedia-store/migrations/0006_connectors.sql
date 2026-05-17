-- External connectors: poll a source-of-truth system (Confluence,
-- GDrive, SharePoint) on a schedule and ingest changed docs.

CREATE TABLE IF NOT EXISTS connectors (
  id            TEXT PRIMARY KEY,                  -- ulid
  tenant        TEXT NOT NULL DEFAULT 'default',
  kind          TEXT NOT NULL,                     -- confluence | gdrive | sharepoint
  name          TEXT NOT NULL,                     -- human label
  config_json   TEXT NOT NULL,                     -- connector-specific (incl. creds)
  cursor        TEXT,                              -- opaque per connector
  enabled       INTEGER NOT NULL DEFAULT 1,
  last_run_at   INTEGER,                           -- unix ms
  last_error    TEXT,
  created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS connectors_tenant   ON connectors(tenant);
CREATE INDEX IF NOT EXISTS connectors_due      ON connectors(enabled, last_run_at);
