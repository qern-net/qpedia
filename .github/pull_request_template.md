<!-- Thanks for the contribution. Keep the record honest — see FEATURE-LANDING.md -->

## What & why

<!-- One paragraph: what this changes and why. -->

## Feature-landing checklist (Tier 1 — required when applicable)

See [`FEATURE-LANDING.md`](../FEATURE-LANDING.md). CI enforces the starred items.

- [ ] ⭐ **CHANGELOG.md** updated under `## [Unreleased]` (required for any `crates/**` change).
- [ ] ⭐ **Models touched?** Updated **both** `APPROVED-MODELS.md` and `crates/qpedia-llm/src/models.rs`.
- [ ] **Feature lists** updated (`README.md`, and `docs/INTEGRATION.md` if the external surface changed).
- [ ] **`/api/v1` changed?** Updated `contracts/qpedia-openapi.yaml`.
- [ ] **New env vars?** Added to `.env.example` + README config tables.
- [ ] **New tenant-scoped table?** Follows the steering rules (FORCE RLS, BIGINT PK + `external_id UUID`).
- [ ] Tests added/updated; `cargo test` green.

## Tier 2 (outward — at release)

- [ ] Flagged for release propagation (home page / wiki / ads / articles / pricing) per `FEATURE-LANDING.md`. _N/A for internal-only changes._
