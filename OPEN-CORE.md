# Open-Core Strategy — `qpedia` and `qpedia-pvt`

> How the public OSS engine (`qpedia`) and the private SaaS overlay
> (`qpedia-pvt`) compose, what lives where, and how to split the existing
> codebase without forking. Mirrors the `uptime` / `uptime-pvt` pattern.

## 0. Why open-core

- **`qpedia`** is the engine: ingest pipeline, agentic wiki authoring,
  hybrid search, multi-tenant runtime. Apache-2.0. Self-hostable on a
  laptop or a single VM. The thing that wins developers.
- **`qpedia-pvt`** is the SaaS overlay: tenant lifecycle, billing,
  premium connectors, enterprise auth, branded UX, compliance. Private.
  The thing that runs our hosted service at qern.net.

The split must satisfy two non-negotiables:

1. **No fork.** `qpedia-pvt` consumes `qpedia` as a versioned dependency,
   never modifies its source. Drift kills open-core.
2. **No framework creep.** `qpedia` exposes only extension points the
   overlay actually uses; we don't speculatively turn the OSS engine into
   a plugin host.

## 1. Composition: Cargo-workspace overlay via git dependency

```
qpedia/                              (public, Apache-2.0)
└─ Cargo.toml [workspace]
   └─ all current crates incl. qpedia-api binary

qpedia-pvt/                          (private)
└─ Cargo.toml [workspace]
   ├─ depends on qpedia crates as git deps (pinned to a tag)
   └─ adds pvt-only crates + a qpedia-pvt-api binary that
      reuses qpedia-api as a library
```

This requires one prerequisite refactor in OSS:

> **Extract `qpedia-api::main` into an `AppBuilder` library.** Today it
> is `#[tokio::main] fn main()` with everything hardcoded. Turn it into
> `qpedia_api::AppBuilder::from_env().await? -> Router` so the overlay
> can do `builder.with_route(...).with_state_extension(...).build()`.

That's the single largest piece of OSS-side work this split requires.
Without it, the overlay forks `main.rs` and the whole arrangement decays
into a hard fork.

## 2. Where things live

| Belongs in `qpedia` (OSS) | Belongs in `qpedia-pvt` (SaaS) |
|---|---|
| All current Rust crates (PgStore, ingest, retriever, lint, …) | **Tenant lifecycle**: provisioning, dunning, suspension, deletion |
| Single + multi-tenant runtime (RLS is already generic) | **Billing**: Stripe meters, usage tracking, plan enforcement |
| OSS frontend (current SvelteKit, minus brand) | **Branded SaaS web**: marketing pages, billing UI, support widget, custom theme |
| Firebase Auth verifier *and* OIDC (both generic) | **Enterprise auth extensions**: SAML, SCIM provisioning, custom IdP hooks |
| Open connectors: Confluence | **Premium connectors**: GDrive, SharePoint Online, Slack, Salesforce, Jira |
| LLM provider abstraction + Anthropic / OpenAI / OpenRouter | **Hosted-LLM ops**: vendor failover, per-tenant quotas, cost dashboards |
| Folder ACLs, lint, audit, OTel stubs | **Compliance**: SOC2 / ISO27001 audit exports, GDPR DSR flows, eDiscovery, retention policy engine |
| Docs (README, SPEC-v2, DESIGN, AGENTS, OPEN-CORE) | **Runbooks** (private): incident playbooks, on-call rotations, infra docs |
| `docker-compose.yml` for self-hosters | **Production deployment**: Terraform / Helm / K8s for qern.net |

**Decision rule when a new feature lands ambiguously:** *"Does a
single-tenant self-hoster want this?"* — yes → OSS, no → pvt. Easier
to move OSS → pvt later than the reverse (Apache-2.0 contributions
can't be retracted).

## 3. Extension points OSS must expose

Most already exist (the codebase was built with this in mind):

- ✅ `LlmProvider`, `Embedder`, `Extractor`, `Connector` traits — done.
- ✅ `Acl`, `Tenant` types — already generic, no SaaS assumptions baked in.

The gaps the overlay needs filled:

- ⚙️ **`AppBuilder`** (the refactor above) — must expose `.with_route()`,
  `.with_state_extension::<T>()`, `.with_auth_provider()`, `.with_event_sink()`.
- ⚙️ **`EventSink` trait** — audit + observability hook so pvt can route
  events to its compliance store / SIEM.
- ⚙️ **`TenantHook` trait** — fires on tenant create / update / delete
  so pvt can provision billing rows and send onboarding emails.
- ⚙️ **SvelteKit web theme tokens + named slots** — `web-pvt` overrides
  brand without forking pages. Either publish `qpedia-web` as an npm
  package or have `web-pvt` symlink it during dev and copy on build.

Add each extension point **only when the overlay actually needs it.**
Speculative hooks rot.

## 4. Repo layout for `qpedia-pvt`

```
qpedia-pvt/
├─ Cargo.toml                       # workspace; depends on qpedia via git tag
├─ crates/
│  ├─ qpedia-pvt-saas/              # tenant lifecycle + billing + onboarding
│  ├─ qpedia-pvt-connectors/        # gdrive, sharepoint, slack, salesforce, jira
│  ├─ qpedia-pvt-auth/              # saml, scim, custom IdP plugins
│  ├─ qpedia-pvt-observability/     # otel collectors, dashboards, SLI/SLO defs
│  ├─ qpedia-pvt-compliance/        # audit exports, GDPR DSR, retention
│  └─ qpedia-pvt-api/               # binary: composes qpedia + pvt
├─ web-pvt/                         # branded SvelteKit (consumes @qern/qpedia-web)
├─ deploy/                          # terraform / helm / k8s manifests
├─ docker/                          # Dockerfile + compose.prod.yml
└─ docs/                            # runbooks, SaaS ops, internal SLAs
```

Sample `qpedia-pvt-api/src/main.rs`:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    qpedia_api::AppBuilder::from_env().await?
        .with_state_extension(qpedia_pvt_saas::Billing::from_env()?)
        .with_state_extension(qpedia_pvt_compliance::AuditExporter::new()?)
        .with_auth_provider(qpedia_pvt_auth::saml::Provider::from_env()?)
        .with_route("/api/v1/billing/*",       qpedia_pvt_saas::billing_router())
        .with_route("/api/v1/admin/tenants",   qpedia_pvt_saas::tenant_router())
        .with_event_sink(qpedia_pvt_compliance::SiemSink::new()?)
        .serve()
        .await
}
```

## 5. Distribution + CI

- **qpedia** → public GitHub Actions builds `ghcr.io/qern-net/qpedia:vX.Y.Z`.
  Tags published; SemVer.
- **qpedia-pvt** → private CI builds `gcr.io/qern-prod/qpedia-pvt:vX.Y.Z`,
  deploys to qern.net SaaS infra. Pins `qpedia = { git =
  "https://github.com/qern-net/qpedia", tag = "vX.Y.Z" }`. Bumped on a
  schedule (weekly or per-release).

## 6. License

**Apache-2.0** for `qpedia`. Enterprise-friendly (no copyleft, no
contribution-revocation surprises) and the standard choice for
open-core. Avoid AGPL (would force pvt open). BSL is possible but
Apache-2.0 signals "no future re-licensing surprises" to enterprise
self-hosters.

## 7. Migration plan (~3-4 weeks)

| Step | Repo | Effort | Output |
|---|---|---|---|
| 1. Extract `AppBuilder` library from `qpedia-api`'s `main.rs` | qpedia | 1-2 wk | OSS users see no change; overlay has a clean composition surface |
| 2. Add `EventSink` + `TenantHook` traits, integrate at the right call sites | qpedia | 2-3 d | OSS still works; pvt has hook points |
| 3. Publish `@qern/qpedia-web` as an npm package (or workspace) so themes can be overridden | qpedia | 2-3 d | Frontend re-use enabled for `web-pvt` |
| 4. Tag `qpedia v1.0.0`; write the first proper public CHANGELOG | qpedia | 1 d | First open-source release |
| 5. Spin up `qpedia-pvt` repo: empty workspace, minimal qpedia-pvt-api that just delegates to OSS | qpedia-pvt | 1 d | Repo skeleton; CI green |
| 6. Move SaaS-specific code (when written) into qpedia-pvt; build infra; deploy | qpedia-pvt | 1-2 wk | qern.net runs on qpedia-pvt image |
| 7. One-paragraph note in `qpedia/README` about the split; full version in `qpedia-pvt/README` | both | half day | Future contributors understand the boundary |

## 8. Risks

- **The `AppBuilder` refactor is real work.** Skipping it forces pvt to
  fork main.rs — that's the maintenance trap. Don't shortcut.
- **Trait-object soup risk.** Don't add extension traits speculatively.
  Each hook only when pvt needs it; OSS shouldn't become a framework.
- **Web-pvt drift.** SvelteKit is harder to "extend" than backend Rust.
  Decide early: theme-token override or full route override. Start with
  theme tokens + named slots; only do full route override for
  billing / tenant management pages.
- **Pvt depending on a `main` snapshot vs a tag.** Always pin to a tag —
  `branch = "main"` looks innocent until OSS makes a breaking change and
  SaaS suddenly fails to build at 3am.

## 9. Day-to-day discipline once split

- Bugs found in qpedia code: fix in `qpedia`, cut a patch release, bump
  `qpedia-pvt`'s pin. **Never** patch the pinned source in `qpedia-pvt`.
- Features that start in `qpedia-pvt` and turn out to be generally
  useful: promote to `qpedia` (rewrite under Apache-2.0; don't copy a
  proprietary file across a license boundary).
- Security advisories: dual-disclose — fix in `qpedia` first, then
  publish the patched OSS image, then update SaaS.

See [`ROADMAP.md`](ROADMAP.md) for the prioritized work plan that
sequences this split alongside the rest of the active backlog.
