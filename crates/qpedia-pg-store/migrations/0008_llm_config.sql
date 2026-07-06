-- Per-tenant LLM configuration (BYO model + BYO credentials).
--
-- Realizes the BYOL design (TASK-managed-llm-billing.md, APPROVED-MODELS.md):
-- a tenant may pick an approved model and, optionally, bring its own provider
-- credentials. When no row exists (or api_key_ciphertext is NULL) the engine
-- falls back to the deployment-level env provider — BYOL at deploy level stays
-- the default and nothing here is required to run.
--
-- Org-wide granularity: at most ONE active config per tenant (PK = tenant_id).
--
-- Secrets: the API key is encrypted AT REST with pgcrypto symmetric encryption
-- (pgp_sym_encrypt) under a deployment master key (QPEDIA_SECRET_KEY), passed
-- as a bind parameter by the app — it is NEVER stored in plaintext and never
-- returned to clients (only api_key_hint, the last 4 chars, is shown in UI).
-- RLS still isolates rows per tenant; encryption is defense-in-depth against
-- at-rest / DBA exposure. pgcrypto is already enabled (see 0001_init.sql).
--
-- Steering compliance: BIGINT identity PK + external_id UUID on this
-- externally-referenced table; tenant-scoped with FORCE ROW LEVEL SECURITY.

CREATE TABLE llm_config (
    id                 BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    external_id        UUID NOT NULL DEFAULT gen_random_uuid(),
    tenant_id          TEXT NOT NULL UNIQUE REFERENCES tenants(id) ON DELETE CASCADE,
    provider           TEXT NOT NULL,            -- anthropic | openai | openrouter | openai-compatible
    model              TEXT,                     -- approved model id; NULL ⇒ per-provider default
    api_key_ciphertext BYTEA,                    -- pgp_sym_encrypt(key, $master); NULL ⇒ use deploy env key
    api_key_hint       TEXT,                     -- last 4 chars, for display only
    base_url           TEXT,                     -- for openai-compatible / proxy overrides
    updated_by         TEXT,                     -- user_id of the last editor
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX llm_config_external_id ON llm_config(external_id);

GRANT SELECT, INSERT, UPDATE, DELETE ON llm_config TO qpedia_app;

-- Tenant isolation (mirror migrations/0002_rls.sql). FORCE so even the table
-- owner is subject to the policy; the app connects as qpedia_app (no BYPASSRLS).
ALTER TABLE llm_config ENABLE ROW LEVEL SECURITY;
ALTER TABLE llm_config FORCE ROW LEVEL SECURITY;
DO $$ BEGIN
    CREATE POLICY tenant_isolation ON llm_config FOR ALL TO qpedia_app
        USING      (tenant_id = current_setting('qpedia.tenant', true))
        WITH CHECK (tenant_id = current_setting('qpedia.tenant', true));
EXCEPTION WHEN duplicate_object THEN NULL; END $$;
