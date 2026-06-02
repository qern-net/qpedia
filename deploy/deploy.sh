#!/usr/bin/env bash
#
# Provision + deploy Qpedia on a single Debian/Ubuntu host (Contabo et al.).
#
# Runs as ROOT over SSH (that's the access a fresh VPS gives you), but the
# application itself runs UNPRIVILEGED as the `qpedia` user (uid 10001, the
# same uid the container drops to — see the Dockerfile) from /opt/qpedia.
#
# Idempotent: only creates what's missing. Invoked by
# .github/workflows/deploy.yml; safe to run by hand too.
#
# Inputs (env):
#   GIT_REF   git ref (branch/tag/sha) to deploy        [default: main]
#   ENV_SRC   path to the production .env to install     [default: /tmp/qpedia.env]
#   REPO_URL  source repository                          [default: public qpedia]
set -euo pipefail

REPO_URL="${REPO_URL:-https://github.com/qern-net/qpedia.git}"
GIT_REF="${GIT_REF:-main}"
ENV_SRC="${ENV_SRC:-/tmp/qpedia.env}"

QPEDIA_USER="qpedia"
QPEDIA_UID="10001"          # MUST match `useradd -u 10001` in the Dockerfile
QPEDIA_HOME="/opt/qpedia"
APP_DIR="${QPEDIA_HOME}/app"

log() { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }

# Provisioning (users, Docker, dirs) needs root, but the deploy user may be an
# unprivileged sudoer rather than root — re-exec under sudo if so. The script's
# ENV_SRC / GIT_REF defaults (/tmp/qpedia.env, main) are correct for the
# push-to-deploy path even if sudo drops the env, so this is safe either way.
if [ "$(id -u)" -ne 0 ]; then
  command -v sudo >/dev/null 2>&1 || { echo "deploy.sh needs root: set DEPLOY_USER=root or give the user passwordless sudo"; exit 1; }
  exec sudo -E -- bash "$0" "$@"
fi
[ -f "${ENV_SRC}" ] || { echo "missing env file at ${ENV_SRC}"; exit 1; }

log "Base packages (git, curl)"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq git curl ca-certificates

log "Docker (install if missing)"
if ! command -v docker >/dev/null 2>&1; then
  curl -fsSL https://get.docker.com | sh
fi
systemctl enable --now docker
docker compose version >/dev/null || { echo "docker compose v2 plugin missing"; exit 1; }

log "Service user '${QPEDIA_USER}' (uid ${QPEDIA_UID}), non-root"
if ! id "${QPEDIA_USER}" >/dev/null 2>&1; then
  useradd --system --create-home --home-dir "${QPEDIA_HOME}" \
          --shell /usr/sbin/nologin --uid "${QPEDIA_UID}" "${QPEDIA_USER}" \
    || useradd --system --create-home --home-dir "${QPEDIA_HOME}" \
          --shell /usr/sbin/nologin "${QPEDIA_USER}"
fi
usermod -aG docker "${QPEDIA_USER}"
install -d -o "${QPEDIA_USER}" -g "${QPEDIA_USER}" "${QPEDIA_HOME}" "${APP_DIR}"

log "Source @ ${GIT_REF}"
if [ ! -d "${APP_DIR}/.git" ]; then
  sudo -u "${QPEDIA_USER}" git clone "${REPO_URL}" "${APP_DIR}"
fi
sudo -u "${QPEDIA_USER}" git -C "${APP_DIR}" fetch origin "${GIT_REF}"
sudo -u "${QPEDIA_USER}" git -C "${APP_DIR}" checkout -f FETCH_HEAD

log "Data dirs (owned by container uid ${QPEDIA_UID})"
# The compose bind-mounts ./data/{wiki,raw,models}; they must be writable by
# the container's qpedia (uid 10001) or ingestion fails with EACCES. Postgres
# uses a named volume, so it needs no host-side chown.
install -d -o "${QPEDIA_UID}" -g "${QPEDIA_UID}" \
  "${APP_DIR}/data/wiki" "${APP_DIR}/data/raw" "${APP_DIR}/data/models"

log ".env (0600, owner ${QPEDIA_USER})"
install -o "${QPEDIA_USER}" -g "${QPEDIA_USER}" -m 600 "${ENV_SRC}" "${APP_DIR}/.env"
shred -u "${ENV_SRC}" 2>/dev/null || rm -f "${ENV_SRC}"

log "Build & start the stack as ${QPEDIA_USER} (non-root, ${APP_DIR})"
# `sudo -u` initialises the group list, so the freshly-added docker group is
# active. Builds the Rust release + SPA on the host — give the VPS >=4 GB RAM.
cd "${APP_DIR}"
sudo -u "${QPEDIA_USER}" docker compose up -d --build --remove-orphans

log "Wait for health"
for i in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:8080/healthz" >/dev/null 2>&1; then
    echo "healthz: ok"; break
  fi
  sleep 2
  [ "$i" -eq 30 ] && { echo "health check did not pass in time"; sudo -u "${QPEDIA_USER}" docker compose logs --tail 40 app; exit 1; }
done

log "Status"
sudo -u "${QPEDIA_USER}" docker compose ps
if grep -q '^COMPOSE_PROFILES=.*caddy' "${APP_DIR}/.env" 2>/dev/null; then
  DOMAIN="$(sed -n 's/^QPEDIA_DOMAIN=//p' "${APP_DIR}/.env" | tr -d '\r')"
  printf '\nDeployed. Caddy is terminating TLS for https://%s (cert provisions within ~1 min once DNS + ports 80/443 resolve).\n' "${DOMAIN:-<QPEDIA_DOMAIN unset>}"
else
  printf '\nDeployed. App on 127.0.0.1:8080 (no caddy profile) — front it with TLS before public use; see deploy/README.md.\n'
fi
