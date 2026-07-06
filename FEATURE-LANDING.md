# Feature-landing checklist — keep the record honest

> When a feature lands, several sources of truth must move **with it**, in the
> same PR. This is the definition of done. Tier 1 (internal records) is
> **enforced** at PR/CI time; Tier 2 (outward content) is tracked and largely
> automatable. Owner: whoever merges the feature.

## Why

A feature is not "done" when the code compiles — it's done when the changelog,
the model list, the feature lists, and the public surfaces (home, wiki, ads,
articles) all reflect it. Letting these drift is how a product's story rots.
This doc + the PR template + the CI check are the "hooks" that prevent it.

## Tier 1 — internal sources of truth (same PR, CI-enforced)

- [ ] **CHANGELOG.md** — add a bullet under `## [Unreleased]`
      (`Added`/`Changed`/`Fixed`/`Removed`). *CI fails a `crates/**` change with
      no `[Unreleased]` edit.*
- [ ] **APPROVED-MODELS.md + `qpedia-llm/src/models.rs`** — if the change
      touches models/providers/defaults, update **both** (doc changelog entry +
      the `APPROVED_MODELS` table) together. *CI fails a `models.rs` change with
      no `APPROVED-MODELS.md` edit.*
- [ ] **Feature lists** — update the capability list in `README.md` (and the
      foundational-layer section if it's platform-facing) and the consumer/
      integration notes in `INTEGRATION.md` when the external surface changes.
- [ ] **Contract** — if `/api/v1` changed, update
      `qpedia-openapi.yaml` in the same PR (it is the SDK source).
- [ ] **Config docs** — new env vars land in `.env.example` and the README
      config tables.
- [ ] **Migrations & steering** — new tenant-scoped tables follow the steering
      rules (Postgres + FORCE RLS, BIGINT PK + `external_id UUID`).

## Tier 2 — outward propagation (tracked; automatable)

Triggered on **release** (when `[Unreleased]` is cut to a version), not every PR.
For each shipped feature, fan out the message:

- [ ] **Home page** — update the marketing site feature grid / "what's new".
- [ ] **Wiki** — add/refresh the relevant how-to or concept page (the product's
      own Qpedia wiki and the public GitHub wiki).
- [ ] **Ads** — refresh ad copy / creatives where the feature is a selling point
      (e.g. the LinkedIn campaign assets in `qpedia-pvt`).
- [ ] **Articles** — draft a changelog blog post / article for material
      features; cross-post as planned.
- [ ] **Pricing** — if the feature is plan-gated or a new SKU, update
      `qpedia-pvt/docs/PRICING.md` and the pricing page.

### Automating Tier 2

The release author can hand the cut `[Unreleased]` section to a drafting step
that produces first drafts of the home-page blurb, a wiki page, ad copy, and an
article, for human review. Two ways to run it:

- **On demand:** ask the assistant, "Draft Tier-2 propagation for the features
  in CHANGELOG [Unreleased]," pasting or pointing at the section.
- **Scheduled:** a recurring task (e.g. weekly, or on a release tag) that reads
  the latest changelog delta and opens draft content for review. Ask to set one
  up — cadence and target surfaces are the only inputs.

> Drafts are starting points, not auto-published. A human edits and ships them.

## Enforcement

- **PR template** (`.github/pull_request_template.md`) puts Tier 1 in front of
  every author.
- **CI** (`.github/workflows/feature-landing-check.yml`) blocks the two highest-
  value drifts: code-without-changelog, and `models.rs`-without-`APPROVED-MODELS`.
- **Release step** runs Tier 2 from the cut changelog.

The CI check is intentionally narrow (two rules) to stay low-friction; widen it
only if a particular drift keeps happening.
