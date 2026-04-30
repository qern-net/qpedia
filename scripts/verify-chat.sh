#!/usr/bin/env bash
# verify-chat.sh
#
# Posts a question to /api/v1/chat and asserts the SSE stream produced a
# meta event, at least one token, and a done event. Prints the answer.
#
# Requirements:
#   - qpedia-api running at QPEDIA_URL with an LLM key in its env.
#   - Optional: a populated wiki (run verify-embeddings.sh first for a
#     non-trivial answer; otherwise the model will note 'no pages found').
#
# Usage:
#   bash scripts/verify-chat.sh
#   bash scripts/verify-chat.sh "What does the manual say about Q4 risks?"
#   QPEDIA_URL=http://localhost:8080 bash scripts/verify-chat.sh

set -u
export MSYS_NO_PATHCONV=1

URL="${QPEDIA_URL:-http://127.0.0.1:18080}"
QUESTION="${1:-Summarize the wiki in three bullets.}"

red()   { printf "\033[31m%s\033[0m\n" "$*"; }
green() { printf "\033[32m%s\033[0m\n" "$*"; }
fail()  { red "FAIL: $*"; exit 1; }

curl -fsS "$URL/healthz" >/dev/null || fail "qpedia-api not reachable at $URL"

# Build JSON safely via python (handles quotes/newlines in $QUESTION).
BODY="$(QUESTION="$QUESTION" python -c 'import os, json; print(json.dumps({"message": os.environ["QUESTION"], "max_pages": 5}))')"

# Stream SSE and parse it line-by-line in python.
curl -fsS -N -X POST "$URL/api/v1/chat" \
    -H 'content-type: application/json' \
    -d "$BODY" \
| python -u -c '
import sys, json

events = []
tokens = []
meta = None
errors = []
buffer = ""

# Read in small chunks so streaming is visible.
while True:
    chunk = sys.stdin.read(1)
    if not chunk: break
    buffer += chunk
    while True:
        idx = buffer.find("\n\n")
        cr  = buffer.find("\r\n\r\n")
        if cr >= 0 and (idx < 0 or cr < idx):
            block, buffer = buffer[:cr], buffer[cr+4:]
        elif idx >= 0:
            block, buffer = buffer[:idx], buffer[idx+2:]
        else:
            break
        ev = ""
        data = ""
        for raw in block.splitlines():
            if raw.startswith("event:"):  ev = raw[6:].strip()
            elif raw.startswith("data:"): data += raw[5:].lstrip()
        events.append(ev or "?")
        if not data:
            continue
        try:
            d = json.loads(data)
        except Exception:
            continue
        t = d.get("type")
        if   t == "meta":  meta = d
        elif t == "token": tokens.append(d.get("text", ""))
        elif t == "error": errors.append(d.get("message", ""))

print("--- meta ---")
print(json.dumps(meta, indent=2) if meta else "(missing)")
print()
print("--- answer ---")
print("".join(tokens))
print()
print(f"events: {len(events)}  tokens: {len(tokens)}  meta: {1 if meta else 0}  errors: {len(errors)}")

problems = []
if meta is None:                     problems.append("no meta event")
if not tokens and not errors:        problems.append("no token events")
if "done" not in events and not errors: problems.append("no done event")

if problems or errors:
    print()
    if errors: print("server errors:")
    for e in errors: print("  -", e)
    if problems:
        print("missing:")
        for p in problems: print("  -", p)
    sys.exit(1)
print("PASS")
'
