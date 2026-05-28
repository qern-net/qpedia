-- Short-lived OIDC handshake state. Rows live only between /auth/login
-- (PKCE challenge issued, state cookie set) and /auth/callback (code
-- exchanged, session minted). 10-minute TTL is plenty; a sweep on
-- every take_pending() expires the rest.
--
-- No tenant column: at this stage the user isn't authenticated yet, so
-- there's nothing to scope to. The table sits outside RLS and is only
-- writable by qpedia_admin / the connecting role used by the API
-- process (which has BYPASSRLS in dev). Production deployments can
-- grant qpedia_app explicit write privileges (below) without enabling
-- RLS on this table — `state` carries enough entropy to be a capability.

CREATE TABLE oidc_pending (
    state            TEXT PRIMARY KEY,
    pkce_verifier    TEXT NOT NULL,
    nonce            TEXT NOT NULL,
    redirect_after   TEXT,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX oidc_pending_created ON oidc_pending(created_at);

GRANT SELECT, INSERT, DELETE ON oidc_pending TO qpedia_app;
