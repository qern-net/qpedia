#!/usr/bin/env bash
# verify-eval.sh — retrieval-quality eval harness.
#
# Streams a batch of questions through /api/v1/chat and reports timing,
# retrieved-page counts, and per-question pass/fail. Use it to track
# regressions as the agent + retrieval evolve.
#
# Input JSON:
#   {"questions": [
#     {"id": "q1", "q": "What is X?",
#      "expect_paths": ["concepts/x.md"],   // any prefix-match counts
#      "expect_citations_min": 1,
#      "expect_says_unknown": false }
#   ]}
#
# Requirements:
#   - qpedia-api running with an LLM key.
#   - Wiki populated with whatever the eval expects.
#   - python3 + curl on PATH.
#
# Usage:
#   bash scripts/verify-eval.sh                                 # uses scripts/eval-questions.example.json
#   bash scripts/verify-eval.sh path/to/questions.json
#   QPEDIA_URL=http://localhost:8080 \
#     bash scripts/verify-eval.sh path/to/questions.json
#   QPEDIA_EVAL_OUT=eval-report.json bash scripts/verify-eval.sh

set -u
export MSYS_NO_PATHCONV=1

INPUT="${1:-scripts/eval-questions.example.json}"
URL="${QPEDIA_URL:-http://127.0.0.1:18080}"
OUT="${QPEDIA_EVAL_OUT:-eval-report.json}"

red()   { printf "\033[31m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
yel()   { printf "\033[33m%s\033[0m\n" "$*"; }
fail()  { red "FAIL: $*"; exit 1; }

[[ -f "$INPUT" ]] || fail "input not found: $INPUT"
curl -fsS "$URL/healthz" >/dev/null || fail "qpedia-api not reachable at $URL"

QPEDIA_URL="$URL" QPEDIA_EVAL_INPUT="$INPUT" QPEDIA_EVAL_OUT="$OUT" \
python -u <<'PY'
import json, os, sys, time, urllib.request

URL = os.environ["QPEDIA_URL"]
INPUT = os.environ["QPEDIA_EVAL_INPUT"]
OUT = os.environ["QPEDIA_EVAL_OUT"]

with open(INPUT, "r", encoding="utf-8") as f:
    spec = json.load(f)

questions = spec.get("questions", [])
print(f"running {len(questions)} questions against {URL}\n")

results = []
total_pass = 0
total_fail = 0

for q in questions:
    qid = q.get("id") or q["q"][:32]
    expect_paths = [p.rstrip("/") for p in q.get("expect_paths", [])]
    expect_min   = int(q.get("expect_citations_min", 0))
    expect_unk   = bool(q.get("expect_says_unknown", False))

    body = json.dumps({"message": q["q"], "max_pages": 5}).encode("utf-8")
    req = urllib.request.Request(
        URL + "/api/v1/chat",
        data=body,
        headers={"content-type": "application/json"},
        method="POST",
    )

    t0 = time.time()
    answer = []
    citations = []
    mode = None
    errors = []
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            buf = ""
            while True:
                chunk = r.read1(4096)
                if not chunk: break
                buf += chunk.decode("utf-8", errors="replace")
                while True:
                    idx_lf = buf.find("\n\n")
                    idx_cr = buf.find("\r\n\r\n")
                    if idx_cr >= 0 and (idx_lf < 0 or idx_cr < idx_lf):
                        block, buf = buf[:idx_cr], buf[idx_cr+4:]
                    elif idx_lf >= 0:
                        block, buf = buf[:idx_lf], buf[idx_lf+2:]
                    else:
                        break
                    data = ""
                    for line in block.splitlines():
                        if line.startswith("data:"):
                            data += line[5:].lstrip()
                    if not data: continue
                    try: ev = json.loads(data)
                    except: continue
                    t = ev.get("type")
                    if   t == "meta":  mode = ev.get("mode"); citations = ev.get("retrieved", [])
                    elif t == "token": answer.append(ev.get("text", ""))
                    elif t == "error": errors.append(ev.get("message", ""))
    except Exception as e:
        errors.append(f"http: {e}")
    elapsed_ms = int((time.time() - t0) * 1000)

    full = "".join(answer)
    cited_paths = [c["path"] for c in citations]
    matched_prefixes = [
        p for p in expect_paths
        if any(cp == p or cp.startswith(p + "/") or cp.startswith(p) for cp in cited_paths)
    ]
    n_cit = len(citations)

    fails = []
    if errors:
        fails.append(f"errors: {'; '.join(errors)}")
    if expect_paths and not matched_prefixes:
        fails.append(f"expected citation matching one of {expect_paths!r}; got {cited_paths!r}")
    if n_cit < expect_min:
        fails.append(f"expected >= {expect_min} citations, got {n_cit}")
    if expect_unk:
        lower = full.lower()
        markers = ["don't have", "do not have", "no information", "doesn't contain", "does not contain", "not found", "not in the wiki", "no pages"]
        if not any(m in lower for m in markers):
            fails.append("expected 'unknown' style answer but got a substantive reply")

    passed = not fails
    results.append({
        "id": qid,
        "q": q["q"],
        "elapsed_ms": elapsed_ms,
        "mode": mode,
        "n_citations": n_cit,
        "cited_paths": cited_paths,
        "answer_chars": len(full),
        "passed": passed,
        "fails": fails,
    })
    if passed:
        total_pass += 1
        print(f"  PASS [{qid}] {elapsed_ms} ms  mode={mode}  cites={n_cit}")
    else:
        total_fail += 1
        print(f"  FAIL [{qid}] {elapsed_ms} ms  mode={mode}  cites={n_cit}")
        for f in fails: print(f"        - {f}")

summary = {
    "pass": total_pass,
    "fail": total_fail,
    "total": len(questions),
    "url": URL,
}

with open(OUT, "w", encoding="utf-8") as f:
    json.dump({"summary": summary, "results": results}, f, indent=2)

print()
print(f"summary: {total_pass}/{len(questions)} passed; report -> {OUT}")
sys.exit(0 if total_fail == 0 else 1)
PY
