#!/usr/bin/env bash
# verify-agent.sh
#
# End-to-end check of the multi-page ingest agent: upload a source, wait
# for the pipeline to reach Committed, then assert the agent produced a
# valid wiki commit (new summary page, index.md updated, log.md appended).
#
# Requirements:
#   - qpedia-api running with an LLM provider configured in its env
#     (ANTHROPIC_API_KEY or OPENAI_API_KEY exported BEFORE starting the
#     api process — Claude Code scrubs keys from subprocess env)
#   - python3 + git on PATH
#
# Usage:
#   bash scripts/verify-agent.sh
#   QPEDIA_URL=http://localhost:8080 bash scripts/verify-agent.sh

set -u
export MSYS_NO_PATHCONV=1

URL="${QPEDIA_URL:-http://127.0.0.1:18080}"
WIKI="${QPEDIA_WIKI:-./data/wiki}"
TIMEOUT="${QPEDIA_TIMEOUT:-180}"
FIXTURE="${QPEDIA_FIXTURE:-test-fixtures/sample.txt}"

red()   { printf "\033[31m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
yel()   { printf "\033[33m%s\033[0m\n" "$*"; }
fail()  { red "FAIL: $*"; exit 1; }

# 0. Pre-flight.
curl -fsS "$URL/healthz" >/dev/null || fail "qpedia-api not reachable at $URL"
[[ -d "$WIKI/.git" ]] || fail "wiki not initialized at $WIKI (start the api once)"

if [[ ! -f "$FIXTURE" ]]; then
  mkdir -p "$(dirname "$FIXTURE")"
  cat > "$FIXTURE" <<'EOF'
Internal note — Project Atlas Q4 readiness review

Atlas is the ML-driven inventory forecasting product launched in Q2.
Quarterly revenue from Atlas reached $4.2M, ahead of the $3.5M plan.
Top customers by ARR: Acme Corp ($820K), Globex ($610K), Initech ($430K).
Risks for Q4: data-pipeline latency on the Globex feed; pricing pressure
from competitor Vandelay's recent ML offering; engineering hiring lagging
plan by 3 FTE.
EOF
  yel "created sample fixture: $FIXTURE"
fi

WIKI_HEAD_BEFORE="$(git -C "$WIKI" rev-parse HEAD)"
yel "wiki HEAD before: $WIKI_HEAD_BEFORE"

# 1. Upload.
RESP="$(curl -fsS -X POST "$URL/api/v1/sources" \
  -F "folder_path=/verify-agent" \
  -F "file=@${FIXTURE};type=text/plain" )" || fail "upload failed"
ID="$(printf '%s' "$RESP" | python -c 'import sys,json; print(json.load(sys.stdin)["id"])')"
[[ -n "$ID" ]] || fail "upload response missing id: $RESP"
green "uploaded: id=$ID"

# 2. Poll for Committed (or terminal failure).
DEADLINE=$(( $(date +%s) + TIMEOUT ))
STATUS=""
LAST=""
while (( $(date +%s) < DEADLINE )); do
  STATUS="$(curl -fsS "$URL/api/v1/sources/$ID" \
    | python -c 'import sys,json; print(json.load(sys.stdin)["status"])')"
  if [[ "$STATUS" != "$LAST" ]]; then
    yel "  status: $STATUS"
    LAST="$STATUS"
  fi
  case "$STATUS" in
    committed|done|failed|dead) break ;;
  esac
  sleep 2
done

if [[ "$STATUS" != "committed" && "$STATUS" != "done" ]]; then
  fail "pipeline ended at status=$STATUS (want committed/done)"
fi

# 3. Wiki-side assertions.
WIKI_HEAD_AFTER="$(git -C "$WIKI" rev-parse HEAD)"
[[ "$WIKI_HEAD_AFTER" != "$WIKI_HEAD_BEFORE" ]] || fail "wiki HEAD did not advance"
green "wiki HEAD after:  $WIKI_HEAD_AFTER"

# 4. Inspect the new commit and the produced summary page.
echo
echo "==> new commits"
git -C "$WIKI" log --oneline "${WIKI_HEAD_BEFORE}..${WIKI_HEAD_AFTER}"

echo
echo "==> files changed"
CHANGED="$(git -C "$WIKI" diff --name-only "${WIKI_HEAD_BEFORE}" "${WIKI_HEAD_AFTER}")"
echo "$CHANGED"

SUMMARY_PATH="summaries/${ID}.md"
echo "$CHANGED" | grep -qx "$SUMMARY_PATH" || fail "expected $SUMMARY_PATH in changed files"
echo "$CHANGED" | grep -qx "index.md"      || fail "expected index.md in changed files"
echo "$CHANGED" | grep -qx "log.md"        || fail "expected log.md in changed files"

echo
echo "==> summary page (head)"
git -C "$WIKI" show "${WIKI_HEAD_AFTER}:${SUMMARY_PATH}" | head -25

# 5. Validate frontmatter + body shape.
python - "$WIKI/$SUMMARY_PATH" <<'PY' || exit 1
import sys, re
path = sys.argv[1]
text = open(path, encoding="utf-8").read()
errs = []

if not text.lstrip().startswith("---"):
    errs.append("page does not start with frontmatter")
else:
    # Extract frontmatter block.
    after = text.lstrip()[3:]
    m = re.search(r"\n---", after)
    if not m:
        errs.append("frontmatter block not closed")
    else:
        fm = after[:m.start()]
        for key in ("title", "kind"):
            if not re.search(rf"^\s*{key}\s*:", fm, re.M):
                errs.append(f"frontmatter missing key: {key}")
        body = after[m.end():]
        if len(body.strip()) < 100:
            errs.append(f"body suspiciously short ({len(body.strip())} chars)")

if errs:
    print("FAIL:")
    for e in errs: print("  -", e)
    sys.exit(1)
print("page shape OK")
PY

green "PASS: agent ingest completed and produced a valid wiki commit"
