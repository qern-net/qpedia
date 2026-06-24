# TASK: ~~Relocate per-tenant LLM management to pvt~~ — SUPERSEDED

**Superseded 2026-06-22.** Decision changed: "all AI is pvt" follows the
**DB-layer pattern** — the AI engine code stays in the OSS engine and the pvt
binary gates/drives it. There is **no relocation**.

➡️ See [`TASK-llm-pvt-gating.md`](TASK-llm-pvt-gating.md).
