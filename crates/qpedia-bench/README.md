# qpedia-bench

Retrieval benchmark for Qpedia's ranking. Ingests a fixed corpus (with
deliberately planted near-duplicate distractors) into a throwaway tenant,
runs a labeled query set through the real `hybrid_search` path, and reports
**Recall@10 / MRR / nDCG@10 / Exact@1** overall and per query category
(semantic / exact / hybrid).

Use it to **measure** every ranking change — RRF constant, reranker,
tsvector config — instead of guessing. Full rationale and the corpus
design are in the project wiki: *Retrieval* and *Retrieval Benchmark*.

## Run

Needs a Postgres 17 + pgvector instance and the embedder (downloads
`bge-small-en-v1.5` on first use):

```bash
docker compose up -d                       # local Postgres
export QPEDIA_DB_URL=postgres://qpedia_admin:qpedia-dev@127.0.0.1:5432/qpedia?sslmode=disable

cargo run -p qpedia-bench -- run                     # measure (fails on regression vs baseline.json)
cargo run -p qpedia-bench -- run --update-baseline   # accept current scores as the new baseline
```

Tune and compare:

```bash
QPEDIA_RRF_K=30 cargo run -p qpedia-bench -- run     # sharper precision
QPEDIA_RRF_K=90 cargo run -p qpedia-bench -- run     # more recall/consensus
```

## Layout

```
bench/
├─ corpus/         wiki pages (frontmatter + body), ingested as-is
├─ queries.jsonl   one labeled query per line: {id, category, query, qrels}
├─ baseline.json   last accepted overall scores (commit when improved)
└─ last-report.json  written each run (gitignored)
```

`qrels` grades: `2` = the exact answer, `1` = acceptable supporting
context (used by nDCG). The acid test is the distractor set — `enable` vs
`disable`, the three `ERR_PAYMENT_GATEWAY_*` codes, `rollback` vs
`rollout`, `v3.2` vs `v3.1` — where vector-only ranking returns the wrong
sibling.

## Interpreting results

- A change that lifts `hybrid`/`exact` `exact@1` without dropping
  `semantic` `ndcg@10` is a win — commit a new `baseline.json`.
- Hybrid retrieval is a distribution-level improvement, not a per-query
  guarantee; that's why the suite reports per-category numbers.
