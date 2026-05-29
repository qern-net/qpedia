#!/usr/bin/env bash
# restore.sh — restore a qpedia deployment from a backup.sh directory.
#
# Restore order is the reverse of dependency: raw → Postgres → wiki.
# (The wiki repo derives nothing; Postgres' wiki_pages index can be
# rebuilt from the wiki via the reembed admin job if it drifts.)
#
#   1. Raw blobs  — untar raw.tar.gz into $QPEDIA_DATA_DIR.
#   2. Postgres   — pg_restore --clean --if-exists into the live DB.
#   3. Wiki repos — git clone each <tenant>.bundle into
#                   $QPEDIA_DATA_DIR/wiki/<tenant>/.
#
# DESTRUCTIVE: step 2 drops and recreates objects in the target DB.
# Requires QPEDIA_CONFIRM=yes to proceed (or an interactive y/N).
#
# Usage:
#   bash scripts/restore.sh ./backups/20260529T120000Z
#   QPEDIA_CONFIRM=yes bash scripts/restore.sh /mnt/backups/20260529T120000Z
#   QPEDIA_PG_MODE=dsn QPEDIA_DB_URL=postgres://u:p@host/qpedia \
#     bash scripts/restore.sh ./backups/20260529T120000Z

set -euo pipefail
export MSYS_NO_PATHCONV=1

SRC="${1:-}"
if [ -z "${SRC}" ] || [ ! -d "${SRC}" ]; then
  echo "usage: bash scripts/restore.sh <backup-dir>" >&2
  exit 2
fi

DATA_DIR="${QPEDIA_DATA_DIR:-./data}"
PG_MODE="${QPEDIA_PG_MODE:-compose}"
PG_SERVICE="${QPEDIA_PG_SERVICE:-postgres}"
PG_USER="${QPEDIA_DB_USER:-qpedia_admin}"
PG_DB="${QPEDIA_DB_NAME:-qpedia}"

red()  { printf "\033[31m%s\033[0m\n" "$*"; }
grn()  { printf "\033[32m%s\033[0m\n" "$*"; }
info() { printf "  %s\n" "$*"; }

red "About to restore from ${SRC} into:"
info "data_dir: ${DATA_DIR}"
info "postgres: ${PG_MODE} (db=${PG_DB})"
red "This DROPS and recreates objects in the target Postgres DB."
if [ "${QPEDIA_CONFIRM:-}" != "yes" ]; then
  read -r -p "Proceed? [y/N] " ans
  case "${ans}" in
    y | Y | yes) ;;
    *) echo "aborted"; exit 1 ;;
  esac
fi

# ── 1. Raw blobs ────────────────────────────────────────────────────────
echo "[1/3] Raw blobs"
if [ -f "${SRC}/raw.tar.gz" ]; then
  mkdir -p "${DATA_DIR}"
  tar -xzf "${SRC}/raw.tar.gz" -C "${DATA_DIR}"
  info "restored raw/ under ${DATA_DIR}"
else
  info "(no raw.tar.gz in backup — skipping)"
fi

# ── 2. Postgres ─────────────────────────────────────────────────────────
echo "[2/3] Postgres (pg_restore --clean --if-exists)"
pg_dump_file="${SRC}/postgres.dump"
[ -f "${pg_dump_file}" ] || { red "missing ${pg_dump_file}"; exit 1; }
if [ "${PG_MODE}" = "dsn" ]; then
  : "${QPEDIA_DB_URL:?QPEDIA_DB_URL required when QPEDIA_PG_MODE=dsn}"
  pg_restore --clean --if-exists --no-owner --no-privileges \
    -d "${QPEDIA_DB_URL}" "${pg_dump_file}"
else
  docker compose exec -T "${PG_SERVICE}" \
    pg_restore --clean --if-exists --no-owner --no-privileges \
    -U "${PG_USER}" -d "${PG_DB}" < "${pg_dump_file}"
fi
info "Postgres restored"

# ── 3. Wiki repos ───────────────────────────────────────────────────────
echo "[3/3] Wiki repos (git clone from bundles)"
wiki_dest="${DATA_DIR}/wiki"
mkdir -p "${wiki_dest}"
restored=0
if [ -d "${SRC}/wiki" ]; then
  for bundle in "${SRC}/wiki"/*.bundle; do
    [ -f "${bundle}" ] || continue
    tenant="$(basename "${bundle}" .bundle)"
    target="${wiki_dest}/${tenant}"
    if [ -e "${target}" ]; then
      red "skip '${tenant}': ${target} already exists (move it aside to restore)"
      continue
    fi
    git clone --quiet "${bundle}" "${target}" >/dev/null 2>&1
    info "restored tenant '${tenant}'"
    restored=$((restored + 1))
  done
fi
[ "${restored}" -eq 0 ] && info "(no tenant bundles restored)"

grn "restore complete"
info "If wiki_pages search drifts from git, run the reembed admin job per tenant."
