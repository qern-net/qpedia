# Roadmap

Living, prioritized TODO. Top → bottom = strictly next-up → eventually.
Split into four bands; ship each band before moving to the next unless
otherwise noted.

The OSS / SaaS split groundwork sits **on top of everything** because it
shapes how every other item gets built. See [`OPEN-CORE.md`](OPEN-CORE.md)
for the full strategy and rationale.

Status legend: 🟢 in progress · ⚪ pending · ✅ done · 🔴 blocked

---

## Band 0 — OSS / SaaS split groundwork (priority 1)

These are the [`OPEN-CORE.md`](OPEN-CORE.md) migration steps. Don't ship
SaaS-only features until at least #0.1 lands; otherwise we paint into
the corner of forking `main.rs`.

| # | Item | Repo | Status |
|---|---|---|---|
| 0.1 | **Extract `AppBuilder` library** from `qpedia-api/src/main.rs`. Expose `from_env() -> AppBuilder`, `.with_routes()`, `.with_state_extension::<T>()`, `.with_event_sink()`, `.with_tenant_hook()`, `.serve()`. OSS users see no behavior change. | qpedia | ✅ |
| 0.2 | Add **`EventSink` integration** — surface defined in 0.1; wire registered sinks at every `db.write_audit(...)` call site so registered sinks fire alongside the existing tracing + audit-table writes. Default sink stays no-op. | qpedia | ✅ |
| 0.3 | Add **`TenantHook` integration** — surface defined in 0.1; fire `on_upsert` from `/api/v1/admin/bootstrap` and any future tenant CRUD endpoints. | qpedia | ✅ |
| 0.4 | Publish **`@qern/qpedia-web`** as an npm package (or local workspace) so `web-pvt` can override theme tokens and named slots without forking pages. | qpedia | ⚪ |
| 0.5 | **Tag `qpedia v1.0.0`** + write the first public CHANGELOG. | qpedia | ⚪ |
| 0.6 | **Spin up `qpedia-pvt` repo:** empty Cargo workspace, depends on `qpedia` via git tag, minimal `qpedia-pvt-api` binary that just delegates to OSS. CI green. | qpedia-pvt | ⚪ |
| 0.7 | One-paragraph note in `qpedia/README` about the split; full version in `qpedia-pvt/README`. | both | ⚪ |

**Done criteria for Band 0:** a `qpedia-pvt-api` binary builds, runs,
and serves the same routes as `qpedia-api` — purely by composition,
zero source duplication.

---

## Band 1 — Quick wins (in OSS, parallelizable with Band 0)

Small enough to slot in between Band 0 items.

| # | Item | Repo | Status |
|---|---|---|---|
| 1.1 | **`QPEDIA_MARKER_PREFER=1` flag** — when set, route every PDF to Marker first, fall back to pdfium on sidecar error. ~15 lines in `crates/qpedia-extract/src/pdf.rs`. | qpedia | ✅ |
| 1.2 | **Verbose claim_next_job cause chain** — `{:#}` instead of `%e` in the runner warn / fail logs. | qpedia | ✅ |
| 1.3 | **Admin: surface latest lint report** — render `_meta/lint.json` in the Admin tab (orphans, broken links, duplicates) with one-click "open page" links. | qpedia | ✅ |
| 1.4 | **First-run wizard endpoint** — create tenant + set Firebase project + seed initial admin folder ACL in one POST. | qpedia | ✅ |
| 1.5 | **Address the a11y `<label>` warnings** in `admin/+page.svelte` (the "Groups (comma):" label has no associated control). Pre-existing. | qpedia | ✅ |
| 1.6 | **Fix verify-embeddings.sh hybrid assertion** — the Python heredoc treats `mode != "hybrid"` as fatal, but on an empty `wiki_pages` table the API legitimately falls back to fs. Either pre-warm with `reembed` or relax the assertion. | qpedia | ✅ |

---

## Band 2 — Next feature candidates (pick one at a time)

Ranked by user-visible impact per unit of work. Each one is a multi-day
piece; don't start until the prior shipped.

| # | Item | Repo | Notes |
|---|---|---|---|
| 2.1 | **Source replace-in-place** — re-upload a file with the same slug, cascade through wiki updates. Common operator need; today you delete + re-upload and lose the slug. | qpedia | ⚪ |
| 2.2 | **Bulk ingest UX** — drag a folder from the OS into the tree; auto-creates pinned subfolders mirroring the structure, uploads everything. Big leap in onboarding feel. | qpedia | ⚪ |
| 2.3 | **GDrive connector** — the framework + extension points exist in `qpedia-connectors`. Validate Confluence's pattern on a second concrete connector. *Lives in OSS.* | qpedia | ⚪ |
| 2.4 | **SharePoint Online connector** | qpedia-pvt | ⚪ (premium) |
| 2.5 | **Slack connector** | qpedia-pvt | ⚪ (premium) |
| 2.6 | **Multi-language wiki** — tsvector is hardcoded to `'english'`, embedder is `bge-small-en-v1.5`. To support es / fr / de / etc. you need per-page language → per-page tsvector config + a multilingual embedder (e.g., bge-m3). Non-trivial; defer until a real customer demands it. | qpedia | ⚪ |
| 2.7 | **Collaborative human editing** of wiki pages with agent-assisted merge. Listed in `DESIGN §16`. Biggest leap — premature until a few real teams are using v2. | qpedia | ⚪ |

---

## Band 3 — Production readiness

Do these before / alongside the qpedia-pvt SaaS launch.

| # | Item | Repo | Notes |
|---|---|---|---|
| 3.1 | **Wire OpenTelemetry export** — env stub exists (`OTEL_EXPORTER_OTLP_ENDPOINT`); collector wiring missing. | qpedia | ⚪ |
| 3.2 | **Multi-worker job runner** — one worker per process today; add `QPEDIA_WORKERS=N` to spawn N concurrent claimers (SKIP LOCKED already supports it). | qpedia | ⚪ |
| 3.3 | **Backup runbook** — `pg_dump` cadence + per-tenant `git bundle`. README has the table; we need a `scripts/backup.sh` and a tested restore drill. | qpedia | ⚪ |
| 3.4 | **Rate limit on `/api/v1/chat`** — per-tenant (and per-session in pvt) token bucket. Otherwise one runaway client can drain the LLM budget. | qpedia | ⚪ |
| 3.5 | **CI for migrations** — spin up a fresh pgvector container, apply all migrations, run `cargo test`. Catches accidental schema regressions. | qpedia | ⚪ |
| 3.6 | **Premium-LLM ops** — vendor failover, per-tenant quotas, cost dashboards. | qpedia-pvt | ⚪ |
| 3.7 | **Compliance** — SOC2 / ISO27001 audit hooks, GDPR data export / erasure flows. | qpedia-pvt | ⚪ |

---

## Recently shipped (this session)

For context, in order:

1. **v2 Stage D cutover + File Explorer tree** — `a3d62e5`
   SqliteStore + WeaviateStore → PgStore everywhere, payloads carry
   tenant, retriever / lint / api rewired, dotenvy + `QPEDIA_DB_URL`,
   docker-compose DB URL ownership, OIDC pending in Postgres, new
   `folders` table + tree UI with auto-pin protection against AI
   auto-organize.
2. **Wiki sidebar empty-render fix** — `2d81c80`
   Drop `()` from the `$derived` reference in `wiki/+page.svelte`.
3. **Docs sweep: Weaviate → Postgres throughout** — `34717b4`
   README, DESIGN, AGENTS, admin UI strings, code doc-comments, verify
   script. SPEC-v2 and the v2-update banner keep historical refs
   intentionally.
4. **Chat tab `$state` crash fix** — `7338d50`
   Rename `stores.ts` → `stores.svelte.ts`, update import path.
5. **`claim_next_job` verbose-cause logging** — folded into #3.

User action: `docker compose up -d --build app` to pick up #4 in the
running container.

---

## Maintenance discipline

- **Update this file** whenever a Band 0 item ships, when a Band 2/3
  item is started, or when a new item lands on the backlog. The
  top-of-file priority order is the source of truth.
- **Don't add Band 2/3 items to qpedia-pvt before Band 0 ships.** Even
  one shortcut pollutes the split.
- **When a feature could go either way:** read the decision rule in
  [`OPEN-CORE.md §2`](OPEN-CORE.md#2-where-things-live).
