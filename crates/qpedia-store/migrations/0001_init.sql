-- Qpedia core schema. See DESIGN.md §2.5.

CREATE TABLE IF NOT EXISTS sources (
  id            TEXT PRIMARY KEY,
  folder_path   TEXT NOT NULL,
  filename      TEXT NOT NULL,
  mime          TEXT NOT NULL,
  sha256        TEXT NOT NULL,
  size_bytes    INTEGER NOT NULL,
  acl_json      TEXT NOT NULL DEFAULT '[]',
  status        TEXT NOT NULL,
  language      TEXT,
  created_at    INTEGER NOT NULL,
  ingested_at   INTEGER
);
CREATE INDEX IF NOT EXISTS sources_folder ON sources(folder_path);
CREATE INDEX IF NOT EXISTS sources_status ON sources(status);
CREATE INDEX IF NOT EXISTS sources_sha    ON sources(sha256);

CREATE TABLE IF NOT EXISTS jobs (
  id            TEXT PRIMARY KEY,
  kind          TEXT NOT NULL,
  payload_json  TEXT NOT NULL,
  state         TEXT NOT NULL,
  attempt       INTEGER NOT NULL DEFAULT 0,
  max_attempts  INTEGER NOT NULL DEFAULT 5,
  next_run_at   INTEGER NOT NULL,
  locked_by     TEXT,
  locked_until  INTEGER,
  last_error    TEXT,
  created_at    INTEGER NOT NULL,
  updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_dispatch ON jobs(state, next_run_at);

CREATE TABLE IF NOT EXISTS audit (
  id          INTEGER PRIMARY KEY AUTOINCREMENT,
  actor       TEXT NOT NULL,
  action      TEXT NOT NULL,
  target      TEXT,
  metadata    TEXT,
  at          INTEGER NOT NULL
);
