-- Per-folder ACL overrides. When a source uploads under folder X, the
-- system resolves an effective ACL by walking from the exact folder
-- toward the root, returning the closest match. If nothing matches the
-- source falls back to the uploader's group set.

CREATE TABLE IF NOT EXISTS folder_acls (
  folder_path  TEXT PRIMARY KEY,
  acl_json     TEXT NOT NULL,
  updated_at   INTEGER NOT NULL,
  updated_by   TEXT NOT NULL
);
