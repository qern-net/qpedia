# TASK: Steering-compliance audit — RLS multi-tenancy + key conventions

**Raised:** 2026-06-19. Global Qern steering rules:
1. **Multi-tenancy only on PostgreSQL, isolated with built-in RLS.**
2. **Internal PK = `BIGINT`; external-facing tables also expose an `external_id UUID`.**

## Audit result (migrations `0001_init.sql`, `0002_rls.sql`)

### 1. Multi-tenancy via RLS — ✅ COMPLIANT (no action)
Every tenant-scoped table (`sources`, `wiki_pages`, `jobs`, `audit`, `sessions`,
`folder_acls`, `connectors`) has `ENABLE ROW LEVEL SECURITY` + a `tenant_isolation`
policy on `current_setting('qpedia.tenant', true)`. App role `qpedia_app` has no
`BYPASSRLS`; unset GUC fails closed. This is exactly the steering pattern — exemplary,
nothing to change.

### 2. Internal BIGINT PKs — ✅ largely compliant
`sources`, `wiki_pages`, `jobs`, `audit`, `connectors` use `BIGSERIAL PRIMARY KEY`.
`folder_acls` (composite `(tenant_id, folder_path)`) and `sessions` (`token_hash`) are
natural keys — acceptable.

### 3. Residual gaps — DECISION NEEDED
- **(a) `tenants.id` is `TEXT` (slug)** — no BIGINT surrogate. Low cost to add a
  `BIGSERIAL` surrogate PK while keeping the slug as a `UNIQUE` business key.
- **(b) External identifiers are slugs, not UUIDs.** This *diverges* from the
  "external UUID" rule, but it is an **intentional, documented Wikipedia-style design**
  (human-readable URLs are a product feature). This is a genuine conflict between the
  steering rule and Qpedia's design.

## Proposed resolution (decide before implementing)

Recommended pragmatic reconciliation rather than ripping out slugs:
- Keep slugs as the human-facing identifiers.
- Where a stable opaque id benefits external consumers (e.g. cross-system references from
  `qproc`), **add `external_id UUID NOT NULL UNIQUE DEFAULT gen_random_uuid()` alongside
  the slug** on externally-referenced tables (`tenants`, `sources`, `wiki_pages`).
- Add a `BIGSERIAL` surrogate to `tenants`.
- Deliver as an **additive, non-breaking** migration `0008_external_ids.sql`. No RLS work.

**Alternative:** formally grant Qpedia an exception to rule (2) and keep slugs only — record
the exception in `DESIGN.md` so the divergence is intentional and tracked.

## Acceptance criteria
- [ ] Decision recorded (add UUIDs/surrogate **or** documented exception).
- [ ] If adopting: additive migration adds `tenants.id` surrogate + `external_id UUID` on the chosen tables; existing slug behaviour unchanged.
- [ ] No change to RLS (already compliant).
