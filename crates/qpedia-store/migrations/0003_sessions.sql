-- Auth: session cookies + OIDC PKCE state.
-- See DESIGN.md §12.

CREATE TABLE IF NOT EXISTS sessions (
  token_hash    TEXT PRIMARY KEY,             -- sha256 of opaque cookie value
  user_id       TEXT NOT NULL,                -- OIDC `sub`, or "dev:<name>" in dev mode
  user_email    TEXT,
  user_name     TEXT,
  groups_json   TEXT NOT NULL DEFAULT '[]',   -- JSON array of group ids from claims
  expires_at    INTEGER NOT NULL,             -- unix ms
  created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_expires ON sessions(expires_at);

-- Pending OIDC authorize-redirect state, keyed by csrf token.
-- Rows are taken (read-and-deleted) in the callback.
CREATE TABLE IF NOT EXISTS auth_pending (
  state           TEXT PRIMARY KEY,
  pkce_verifier   TEXT NOT NULL,
  nonce           TEXT NOT NULL,
  redirect_after  TEXT,
  created_at      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS auth_pending_created ON auth_pending(created_at);
