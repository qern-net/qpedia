#!/usr/bin/env bash
# verify-hints-storage.sh
#
# Verifies that the classification JSON column shipped: schema, index, API
# round-trip, and JSON1-indexed query all work — independent of any LLM
# call. Useful when you don't have an API key handy.
#
# Requirements:
#   - qpedia-api running with QPEDIA_DATA_DIR=./data (the default for local dev)
#   - python3 on PATH
#
# Usage:
#   bash scripts/verify-hints-storage.sh
#   QPEDIA_URL=http://localhost:8080 QPEDIA_DB=./data/sqlite/qpedia.db \
#     bash scripts/verify-hints-storage.sh

set -u
export MSYS_NO_PATHCONV=1

URL="${QPEDIA_URL:-http://127.0.0.1:18080}"
DB="${QPEDIA_DB:-./data/sqlite/qpedia.db}"
FIXTURE="${QPEDIA_FIXTURE:-test-fixtures/sample.txt}"

red()   { printf "\033[31m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
yel()   { printf "\033[33m%s\033[0m\n" "$*"; }
fail()  { red "FAIL: $*"; exit 1; }

# 0. Pre-flight.
[[ -f "$DB" ]] || fail "sqlite db not found at $DB (start the api once to create it)"
curl -fsS "$URL/healthz" >/dev/null || fail "qpedia-api not reachable at $URL"
if [[ ! -f "$FIXTURE" ]]; then
  mkdir -p "$(dirname "$FIXTURE")"
  echo "verify-hints-storage sample" > "$FIXTURE"
fi

# 1. Schema checks via sqlite.
python - "$DB" <<'PY' || exit 1
import sqlite3, sys
db = sys.argv[1]
c = sqlite3.connect(db)

errs = []

migs = {row[1] for row in c.execute("SELECT version, description FROM _sqlx_migrations")}
if "init" not in migs:           errs.append("migration 'init' not applied")
if "classification" not in migs: errs.append("migration 'classification' not applied")

cols = {row[1] for row in c.execute("PRAGMA table_info(sources)")}
if "classification_json" not in cols:
    errs.append("column classification_json missing from sources")

idx = {row[0] for row in c.execute(
    "SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='sources'")}
if "sources_doctype" not in idx:
    errs.append("functional index sources_doctype missing")

# Confirm JSON1 is functional.
try:
    c.execute("SELECT json_extract('{\"x\":1}', '$.x')").fetchone()
except sqlite3.OperationalError as e:
    errs.append(f"JSON1 unavailable: {e}")

if errs:
    print("schema check FAIL:")
    for e in errs: print("  -", e)
    sys.exit(1)
print("schema check OK")
PY

# 2. API round-trip: upload → seed classification directly → GET via API.
RESP="$(curl -fsS -X POST "$URL/api/v1/sources" \
  -F "folder_path=/verify-hints" \
  -F "file=@${FIXTURE};type=text/plain" )" || fail "upload failed"
ID="$(printf '%s' "$RESP" | python -c 'import sys,json; print(json.load(sys.stdin)["id"])')"
green "uploaded: id=$ID"

python - "$DB" "$ID" <<'PY' || exit 1
import sqlite3, sys, json
db, sid = sys.argv[1], sys.argv[2]
c = sqlite3.connect(db)
sample = {
    "doc_type":   "report",
    "language":   "en",
    "sensitivity":"low",
    "hints":      ["q4-revenue", "forecast", "enterprise-segment"],
}
c.execute("UPDATE sources SET classification_json = ? WHERE id = ?",
          (json.dumps(sample), sid))
c.commit()
print("seeded classification for", sid)
PY

GET="$(curl -fsS "$URL/api/v1/sources/$ID")"
echo "$GET" | python -c '
import sys, json
r = json.load(sys.stdin)
errs = []
c = r.get("classification")
if not isinstance(c, dict):
    errs.append("classification not present in API response")
else:
    if c.get("doc_type") != "report":
        errs.append("doc_type wrong: " + repr(c.get("doc_type")))
    if c.get("language") != "en":
        errs.append("language wrong: " + repr(c.get("language")))
    if c.get("sensitivity") != "low":
        errs.append("sensitivity wrong: " + repr(c.get("sensitivity")))
    if c.get("hints") != ["q4-revenue","forecast","enterprise-segment"]:
        errs.append("hints wrong: " + repr(c.get("hints")))

print("--- API response ---")
print(json.dumps(r, indent=2))
print()
if errs:
    print("API round-trip FAIL:")
    for e in errs: print("  -", e)
    sys.exit(1)
print("API round-trip OK")
'

# 3. JSON1 query — correctness check.
# (Whether SQLite's planner picks the functional index or a SCAN depends on row
#  count and ANALYZE state; we assert the rows are returned correctly. The
#  index existence is already covered by the schema check above.)
python - "$DB" "$ID" <<'PY' || exit 1
import sqlite3, sys
db, sid = sys.argv[1], sys.argv[2]
c = sqlite3.connect(db)

hits = c.execute(
    "SELECT id FROM sources "
    "WHERE json_extract(classification_json, '$.doc_type') = ?",
    ("report",)
).fetchall()

if not any(h[0] == sid for h in hits):
    print("JSON1 query FAIL:")
    print(f"  - target id {sid} not returned by doc_type='report' (hits={hits})")
    sys.exit(1)

# Hints query (one of our planned filters).
hint_hits = c.execute(
    "SELECT id FROM sources "
    "WHERE EXISTS (SELECT 1 FROM json_each(classification_json, '$.hints') WHERE value = ?)",
    ("forecast",)
).fetchall()

if not any(h[0] == sid for h in hint_hits):
    print("JSON1 hints-membership query FAIL:")
    print(f"  - target id {sid} not returned by hint='forecast' (hits={hint_hits})")
    sys.exit(1)

print(f"JSON1 query OK (doc_type matches: {len(hits)}, hint matches: {len(hint_hits)})")
PY

green "PASS: classification column shipped cleanly (schema + API + JSON1 queries)"
