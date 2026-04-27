#!/usr/bin/env bash
# verify-classification.sh
#
# Uploads a small sample doc and asserts the ingest pipeline reaches the
# Classified state with a populated language + classification record.
# Exits non-zero on any check failure.
#
# Requirements:
#   - qpedia-api running and reachable at $QPEDIA_URL (default 127.0.0.1:18080)
#   - One of ANTHROPIC_API_KEY / OPENAI_API_KEY / OPENROUTER_API_KEY set in
#     the api process's env (NOT this script's env — the api reads its own)
#   - python3 on PATH
#
# Usage:
#   bash scripts/verify-classification.sh
#   QPEDIA_URL=http://localhost:8080 bash scripts/verify-classification.sh

set -u
export MSYS_NO_PATHCONV=1   # git-bash on Windows: don't mangle "/folder" args

URL="${QPEDIA_URL:-http://127.0.0.1:18080}"
TIMEOUT="${QPEDIA_TIMEOUT:-60}"
FIXTURE="${QPEDIA_FIXTURE:-test-fixtures/sample.txt}"

red()   { printf "\033[31m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
yel()   { printf "\033[33m%s\033[0m\n" "$*"; }

fail() { red "FAIL: $*"; exit 1; }

# 0. Pre-flight: server reachable?
if ! curl -fsS "$URL/healthz" >/dev/null; then
  fail "qpedia-api not reachable at $URL — start it first."
fi

# Make sure we have a fixture; create one if missing.
if [[ ! -f "$FIXTURE" ]]; then
  mkdir -p "$(dirname "$FIXTURE")"
  echo "Quarterly revenue forecast for FY26. Q1 projected at 4.2M USD with growth driven by enterprise contracts." > "$FIXTURE"
  yel "created sample fixture: $FIXTURE"
fi

# 1. Upload.
RESP="$(curl -fsS -X POST "$URL/api/v1/sources" \
  -F "folder_path=/verify" \
  -F "file=@${FIXTURE};type=text/plain" )" || fail "upload failed"

ID="$(printf '%s' "$RESP" | python -c 'import sys,json; print(json.load(sys.stdin)["id"])')"
[[ -n "$ID" ]] || fail "upload response missing id: $RESP"
green "uploaded: id=$ID"

# 2. Poll until status is terminal or timeout.
DEADLINE=$(( $(date +%s) + TIMEOUT ))
STATUS=""
while (( $(date +%s) < DEADLINE )); do
  STATUS="$(curl -fsS "$URL/api/v1/sources/$ID" \
    | python -c 'import sys,json; print(json.load(sys.stdin)["status"])')"
  case "$STATUS" in
    classified|failed|dead) break ;;
  esac
  sleep 1
done

# 3. Final fetch + assertions in one python pass.
FINAL="$(curl -fsS "$URL/api/v1/sources/$ID")"
echo "$FINAL" | python -c '
import sys, json, re
r = json.load(sys.stdin)
errors = []

if r.get("status") != "classified":
    errors.append(f"status is {r.get(\"status\")!r}, want \"classified\"")

lang = r.get("language")
if not lang:
    errors.append("language is null/empty")
elif not re.match(r"^[a-z]{2,3}$", lang):
    errors.append(f"language {lang!r} not an ISO-639 code")

c = r.get("classification")
if not isinstance(c, dict):
    errors.append("classification missing or not an object")
else:
    for k in ("doc_type", "language", "sensitivity", "hints"):
        if k not in c:
            errors.append(f"classification missing key {k!r}")
    if "doc_type" in c and c["doc_type"] not in {
        "contract","report","email","slides","manual",
        "invoice","form","code","other",
    }:
        errors.append(f"doc_type {c[\"doc_type\"]!r} not in allowed set")
    if "sensitivity" in c and c["sensitivity"] not in {"low","medium","high"}:
        errors.append(f"sensitivity {c[\"sensitivity\"]!r} not in allowed set")
    if "hints" in c and not isinstance(c["hints"], list):
        errors.append("hints is not a list")

print("--- final source ---")
print(json.dumps(r, indent=2))
print()
if errors:
    print("FAIL:")
    for e in errors:
        print("  -", e)
    sys.exit(1)
print("PASS: classification landed cleanly")
'
