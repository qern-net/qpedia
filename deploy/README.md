# Deploying Qpedia

GitHub Actions builds and deploys onto a single host over SSH. The app runs
**unprivileged** as the `qpedia` user (uid 10001) from **`/opt/qpedia`**; SSH
is as `root` only to provision (create the user, install Docker), never to
run the app.

- **Workflow:** [`.github/workflows/deploy.yml`](../.github/workflows/deploy.yml) — **auto-deploys on every push to `main`** (and `workflow_dispatch`).
- **Server script:** [`deploy/deploy.sh`](deploy.sh) — idempotent provision + `docker compose up -d --build`.
- **Auth:** SSH **key** (no password). The deploy user must be `root` or a passwordless sudoer (the script re-execs under sudo if non-root).
- **Model:** the image is **built on the server** (mirrors `docker compose up --build`; no registry). The container already drops to a non-root user; the compose stack is also invoked by the non-root `qpedia` user.

## 1. Required GitHub secrets

Repo → **Settings → Secrets and variables → Actions → New repository secret**:

| Secret | Value |
|---|---|
| `DEPLOY_HOST` | `62.171.156.199` |
| `DEPLOY_USER` | `root` (or a passwordless-sudo deploy user) |
| `DEPLOY_SSH_KEY` | the **private** key (full PEM/OpenSSH text, **no passphrase**) |
| `PROD_ENV_FILE` | the **entire** production `.env` (multi-line; see below) |

> The host/user are secrets too so the public repo doesn't advertise the prod box.

### Generate the deploy key

On your machine (passphrase **must** be empty — CI can't type one):

```sh
ssh-keygen -t ed25519 -C "qpedia-deploy" -N "" -f qpedia_deploy
```

- Put the **public** half on the server, for `DEPLOY_USER`:
  ```sh
  ssh-copy-id -i qpedia_deploy.pub root@62.171.156.199
  # or: append qpedia_deploy.pub to /root/.ssh/authorized_keys
  ```
- Paste the **private** half (`cat qpedia_deploy` — the whole
  `-----BEGIN…END-----` block) into the `DEPLOY_SSH_KEY` secret, then delete
  the local copies.

### `PROD_ENV_FILE` contents

Base it on your working local `.env`, but **set a strong DB password** and drop
dev-only bits. It must include at least:

```dotenv
# --- database (use a long random password, NOT the dev default) ---
QPEDIA_DB_PASSWORD=<long-random-string>

# --- LLM ---
OPENAI_API_KEY=sk-proj-...               # your real key
# Image OCR/vision (Band 6.1) auto-enables with an OpenAI key and bills per
# image. Set QPEDIA_VISION=0 to disable, or QPEDIA_VISION_MODEL=... to override.

# --- auth / identity (Session/Firebase mode) ---
QPEDIA_FIREBASE_PROJECT_ID=qernnet
QPEDIA_ADMIN_EMAILS=media@qern.net

# --- frontend Firebase config (build-time, inlined into the SPA; public) ---
VITE_FIREBASE_API_KEY=...
VITE_FIREBASE_AUTH_DOMAIN=...
VITE_FIREBASE_PROJECT_ID=qernnet
VITE_FIREBASE_APP_ID=...
VITE_FIREBASE_SSO_PROVIDER_ID=...

# --- HTTPS via Caddy (TLS-terminating reverse proxy) ---
COMPOSE_PROFILES=caddy            # turns on the caddy service for this host
QPEDIA_DOMAIN=qpedia.qern.net     # Caddy auto-provisions a Let's Encrypt cert
```

`compose` reads this one file for both build-arg interpolation (`VITE_*`,
`QPEDIA_DB_PASSWORD`, `QPEDIA_DOMAIN`) and the app container's runtime env. It
also reads `COMPOSE_PROFILES` from it, so adding `COMPOSE_PROFILES=caddy`
enables the reverse proxy without changing the deploy command. `QPEDIA_DB_URL`
is set by compose to point at the `postgres` service, so don't put a host URL
here.

**Before the first deploy with Caddy:** create a DNS **A record** for
`qpedia.qern.net` → `62.171.156.199`. Without it, Caddy can't complete the
Let's Encrypt challenge and HTTPS won't come up (the app is still reachable on
the server's `127.0.0.1:8080` for debugging).

## 2. Trigger a deploy

**Every push to `main` deploys automatically.** You can also run it by hand:
Actions tab → **Deploy (Contabo)** → **Run workflow** (optionally enter a ref).
It scp's the deploy script + env over the SSH key, then runs the build on the
server (first build pulls toolchains/deps — allow ~10–20 min). The script waits
for `/healthz` before declaring success.

## 3. Server requirements

- Debian/Ubuntu, systemd, **≥ 4 GB RAM** and **≥ 20 GB free disk** (the Rust
  release + onnxruntime + the image are large).
- Outbound internet (pulls Docker, crates, npm, pdfium).

## 4. Security — read this

- **HTTPS is terminated by Caddy** (the `caddy` profile). It serves
  `qpedia.qern.net` on **443** with an auto-renewed Let's Encrypt cert and
  redirects **80 → 443**. The app and Postgres bind to **`127.0.0.1` only**, so
  the only public ports are 80 + 443. **Open exactly those** (plus SSH):
  `ufw allow 22,80,443/tcp` — and restrict `:22` to known IPs if you can.
  (Note: Docker's published ports bypass `ufw`, which is *why* the app/DB bind
  to loopback rather than relying on the firewall.)
- **Certs persist** in the `caddy-data` named volume — don't delete it, or you
  risk hitting Let's Encrypt rate limits on re-issue.
- **SSH is key-based** (no password); the private key lives only in the
  `DEPLOY_SSH_KEY` secret. `StrictHostKeyChecking=accept-new` trusts the host
  on first connect (TOFU); pin it harder by adding the server's host key to a
  `known_hosts` if you want. Consider disabling SSH password auth on the
  server (`PasswordAuthentication no`) once key login works.
- The `qpedia` user is in the `docker` group (needed to run compose), which is
  effectively root-equivalent on the host. For stronger isolation, run
  **rootless Docker** as `qpedia` — a larger setup, noted for later.
- `PROD_ENV_FILE` (your OpenAI key, DB password) lives only in GitHub secrets
  and is written `0600` on the server. It is never committed.

## 5. Backups

The Band 3.3 runbook applies on the server: `pg_dump` the `postgres` service
and `git bundle` each per-tenant wiki under `/opt/qpedia/app/data/wiki`. Run
those as root or uid 10001 (which owns the data dirs).

## 6. Alternative: build in CI, pull on the server

For a small VPS, building on the box each deploy is the main cost. To offload
it: have CI `docker build` + push to GHCR (`ghcr.io/qern-net/qpedia`), and have
`deploy.sh` `docker compose pull` a prebuilt image instead of `--build`. That
needs a `docker-compose.prod.yml` pinning `image:` and GHCR auth on the server
(or a public package, since the repo is OSS). Left as a follow-up.
