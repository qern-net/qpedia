#!/usr/bin/env bash
# verify-embeddings.sh
#
# End-to-end check that Weaviate write-back + hybrid search work:
#   1. Upload a source.
#   2. Wait for the pipeline to reach Done (full state machine including
#      AgentDistilling -> Committed -> Embedding -> Done).
#   3. /api/v1/wiki/search responds with mode="hybrid".
#   4. The new summary page is among the hits.
#   5. Weaviate's REST schema endpoint reports the WikiPage class.
#
# Requirements:
#   - Weaviate running (docker compose up weaviate or full compose).
#   - qpedia-api running with QPEDIA_WEAVIATE_URL pointing at it AND an
#     LLM key (ANTHROPIC_API_KEY or OPENAI_API_KEY) in its env.
#   - python3, curl, git on PATH.
#
# Usage:
#   bash scripts/verify-embeddings.sh
#   QPEDIA_URL=http://localhost:8080 \
#     QPEDIA_WEAVIATE=http://localhost:8081 \
#     bash scripts/verify-embeddings.sh

set -u
export MSYS_NO_PATHCONV=1

URL="${QPEDIA_URL:-http://127.0.0.1:18080}"
WEAVIATE="${QPEDIA_WEAVIATE:-http://127.0.0.1:8080}"
TIMEOUT="${QPEDIA_TIMEOUT:-240}"
FIXTURE="${QPEDIA_FIXTURE:-test-fixtures/sample.txt}"

red()   { printf "\033[31m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
yel()   { printf "\033[33m%s\033[0m\n" "$*"; }
fail()  { red "FAIL: $*"; exit 1; }

# 0. Pre-flight.
curl -fsS "$URL/healthz" >/dev/null || fail "qpedia-api not reachable at $URL"
curl -fsS "$WEAVIATE/v1/.well-known/ready" >/dev/null || fail "weaviate not ready at $WEAVIATE"

# Confirm WikiPage schema exists (api should have created it on startup).
SCHEMA="$(curl -fsS "$WEAVIATE/v1/schema/WikiPage")" || fail "WikiPage class missing — did qpedia-api connect to weaviate?"
echo "$SCHEMA" | python -c '
import sys, json
s = json.load(sys.stdin)
print("WikiPage class:", s.get("class"))
print("vectorizer    :", s.get("vectorizer"))
print("properties    :", [p["name"] for p in s.get("properties", [])])
'

if [[ ! -f "$FIXTURE" ]]; then
  mkdir -p "$(dirname "$FIXTURE")"
  cat > "$FIXTURE" <<'EOF'
Project Atlas Q4 readiness review

Atlas is the ML-driven inventory forecasting product launched in Q2.
Quarterly revenue from Atlas reached $4.2M, ahead of the $3.5M plan.
Top customers by ARR: Acme Corp ($820K), Globex ($610K), Initech ($430K).
Risks for Q4: data-pipeline latency on the Globex feed; pricing pressure
from competitor Vandelay's recent ML offering; engineering hiring lagging
plan by 3 FTE.
EOF
  yel "created sample fixture: $FIXTURE"
fi

# 1. Upload.
RESP="$(curl -fsS -X POST "$URL/api/v1/sources" \
  -F "folder_path=/verify-embed" \
  -F "file=@${FIXTURE};type=text/plain" )" || fail "upload failed"
ID="$(printf '%s' "$RESP" | python -c 'import sys,json; print(json.load(sys.stdin)["id"])')"
[[ -n "$ID" ]] || fail "upload response missing id"
green "uploaded: id=$ID"

# 2. Poll for Done.
DEADLINE=$(( $(date +%s) + TIMEOUT ))
STATUS=""; LAST=""
while (( $(date +%s) < DEADLINE )); do
  STATUS="$(curl -fsS "$URL/api/v1/sources/$ID" \
    | python -c 'import sys,json; print(json.load(sys.stdin)["status"])')"
  if [[ "$STATUS" != "$LAST" ]]; then
    yel "  status: $STATUS"
    LAST="$STATUS"
  fi
  case "$STATUS" in
    done|failed|dead) break ;;
  esac
  sleep 2
done
[[ "$STATUS" == "done" ]] || fail "pipeline ended at status=$STATUS (want done)"

# 3. Search via API — must report hybrid mode and include the new summary.
SEARCH="$(curl -fsS "$URL/api/v1/wiki/search?q=Atlas%20revenue&limit=10")"
echo
echo "==> /api/v1/wiki/search (q='Atlas revenue')"
echo "$SEARCH" | python -m json.tool

echo "$SEARCH" | python -c '
import sys, json, os
r = json.load(sys.stdin)
errs = []
if r.get("mode") != "hybrid":
    errs.append(f"mode={r.get(\"mode\")!r}, want hybrid (Weaviate not engaged)")
hits = r.get("hits", [])
if not hits:
    errs.append("no hits returned")
sid = os.environ["ID"]
expected = f"summaries/{sid}.md"
if not any(h.get("path") == expected for h in hits):
    errs.append(f"expected hit on {expected}, got: {[h.get(\"path\") for h in hits]}")
if errs:
    print("FAIL:")
    for e in errs: print("  -", e)
    sys.exit(1)
print("search OK")
' || ID="$ID" fail "search assertions failed"

# 4. Cross-check via Weaviate directly: count WikiPage objects.
COUNT="$(curl -fsS "$WEAVIATE/v1/graphql" -H 'content-type: application/json' \
  -d '{"query":"{ Aggregate { WikiPage { meta { count } } } }"}' \
  | python -c 'import sys,json; r=json.load(sys.stdin); print(r["data"]["Aggregate"]["WikiPage"][0]["meta"]["count"])')"
echo "==> WikiPage objects in Weaviate: $COUNT"
[[ "$COUNT" -ge 1 ]] || fail "expected at least 1 WikiPage object in Weaviate, got $COUNT"

green "PASS: embeddings + Weaviate hybrid search engaged end-to-end"
