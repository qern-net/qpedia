#!/usr/bin/env bash
# backup.sh — point-in-time backup of a qpedia deployment.
#
# Captures the three durable stores, in dependency order:
#   1. Postgres   — pg_dump -Fc (custom format, compressed, parallel-restorable).
#                   The canonical store: tenants, sources, sessions, jobs,
#                   audit, folder_acls, folders, connectors, oidc_pending,
#                   and the wiki_pages search index.
#   2. Wiki repos — one `git bundle` per tenant under /data/wiki/<tenant>/.
#                   A bundle is a single-file clone — restore with `git clone`.
#                   This is the source of truth for page *content*; wiki_pages
#                   is a derived index (rebuildable via the reembed admin job).
#   3. Raw blobs  — tar of /data/raw (uploaded originals + extracted text).
#
# Everything lands in a timestamped directory under $QPEDIA_BACKUP_DIR
# (default ./backups). A `manifest.txt` records what was captured.
#
# Postgres is reached either through the compose service (default) or a
# direct DSN. Wiki + raw are read from the host bind-mount at
# $QPEDIA_DATA_DIR (default ./data) — the same paths docker-compose maps.
#
# Usage:
#   bash scripts/backup.sh
#   QPEDIA_DATA_DIR=/srv/qpedia/data QPEDIA_BACKUP_DIR=/mnt/backups bash scripts/backup.sh
#   QPEDIA_PG_MODE=dsn QPEDIA_DB_URL=postgres://u:p@host/qpedia bash scripts/backup.sh
#
# Restore with: bash scripts/restore.sh <backup-dir>

set -euo pipefail
export MSYS_NO_PATHCONV=1

DATA_DIR="${QPEDIA_DATA_DIR:-./data}"
BACKUP_ROOT="${QPEDIA_BACKUP_DIR:-./backups}"
PG_MODE="${QPEDIA_PG_MODE:-compose}"          # compose | dsn
PG_SERVICE="${QPEDIA_PG_SERVICE:-postgres}"
PG_USER="${QPEDIA_DB_USER:-qpedia_admin}"
PG_DB="${QPEDIA_DB_NAME:-qpedia}"

red()  { printf "\033[31m%s\033[0m\n" "$*"; }
grn()  { printf "\033[32m%s\033[0m\n" "$*"; }
info() { printf "  %s\n" "$*"; }

stamp="$(date -u +%Y%m%dT%H%M%SZ)"
dest="${BACKUP_ROOT}/${stamp}"
mkdir -p "${dest}"

grn "qpedia backup → ${dest}"

# ── 1. Postgres ─────────────────────────────────────────────────────────
echo "[1/3] Postgres (pg_dump -Fc)"
pg_out="${dest}/postgres.dump"
if [ "${PG_MODE}" = "dsn" ]; then
  : "${QPEDIA_DB_URL:?QPEDIA_DB_URL required when QPEDIA_PG_MODE=dsn}"
  pg_dump -Fc --no-owner --no-privileges "${QPEDIA_DB_URL}" > "${pg_out}"
else
  # Through the compose service. -T disables TTY alloc so the binary
  # stream isn't mangled.
  docker compose exec -T "${PG_SERVICE}" \
    pg_dump -Fc --no-owner --no-privileges -U "${PG_USER}" "${PG_DB}" > "${pg_out}"
fi
info "wrote $(du -h "${pg_out}" | cut -f1) → postgres.dump"

# ── 2. Wiki repos — one bundle per tenant ───────────────────────────────
echo "[2/3] Wiki repos (git bundle per tenant)"
wiki_root="${DATA_DIR}/wiki"
mkdir -p "${dest}/wiki"
tenant_count=0
if [ -d "${wiki_root}" ]; then
  for tdir in "${wiki_root}"/*/; do
    [ -d "${tdir}.git" ] || continue
    tenant="$(basename "${tdir}")"
    git -C "${tdir}" bundle create "${dest}/wiki/${tenant}.bundle" --all >/dev/null 2>&1
    info "bundled tenant '${tenant}'"
    tenant_count=$((tenant_count + 1))
  done
fi
[ "${tenant_count}" -eq 0 ] && info "(no tenant wiki repos found under ${wiki_root})"

# ── 3. Raw blobs ────────────────────────────────────────────────────────
echo "[3/3] Raw blobs (tar)"
raw_root="${DATA_DIR}/raw"
if [ -d "${raw_root}" ]; then
  tar -czf "${dest}/raw.tar.gz" -C "${DATA_DIR}" raw
  info "wrote $(du -h "${dest}/raw.tar.gz" | cut -f1) → raw.tar.gz"
else
  info "(no raw dir at ${raw_root})"
fi

# ── manifest ────────────────────────────────────────────────────────────
{
  echo "qpedia backup manifest"
  echo "created_at_utc: ${stamp}"
  echo "data_dir:       ${DATA_DIR}"
  echo "pg_mode:        ${PG_MODE}"
  echo "tenants:        ${tenant_count}"
  echo "files:"
  (cd "${dest}" && find . -type f -printf "  %p  (%s bytes)\n" | sort)
} > "${dest}/manifest.txt"

grn "backup complete: ${dest}"
info "restore with: bash scripts/restore.sh ${dest}"
