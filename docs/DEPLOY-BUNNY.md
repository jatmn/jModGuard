# Deploy on Bunny.net (optional)

jModGuard is an open-source Discord moderation bot you can self-host anywhere.
Today, run its bot and API processes on the **same host or shared filesystem**
so they can open the same SQLite database. Bunny CDN/Storage can still serve
the static WebUI in front of that API.

This document is **guidance**, not a managed service. You remain the operator
(Discord app, secrets, billing, backups). For product install on a single host,
use [SETUP.md](SETUP.md). To upgrade an existing deploy, use [UPGRADE.md](UPGRADE.md).

Remote Bunny Database (libSQL) support is reserved in config (`db.url` /
`db.token`) and currently **fails fast until a remote client is wired**.
Consequently, the current release does not support bot and API in separate
Magic Container apps: their local SQLite paths would be different databases.

## Supported shape

```
admin.yourdomain.com
   ├─ Bunny DNS + CDN (TLS)
   │     SPA from Storage
   │     /api/* and /oauth/* → your API host
   └─ One host or shared volume
         ├─ jmodguard-bot
         ├─ jmodguard-api
         └─ shared SQLite database
```

The two processes remain separately restartable on that host. Do not put them
in separate containers unless their `db.path` resolves to the same shared
filesystem. Independent Magic Container apps require the future libSQL backend.

Bot never calls the API. Both talk to the DB. See [ARCHITECTURE.md](ARCHITECTURE.md).

## Current layout

| Decision | Recommendation |
|----------|----------------|
| Bot host | Always-on process/container on the shared-DB host |
| API host | Second process/container on that same host or shared volume |
| Frontend | Static `frontend/` on Storage + CDN |
| TLS / DNS | Bunny DNS + CDN on the admin hostname |
| Images | `Dockerfile.bot` / `Dockerfile.api` (`rust:*-bookworm` → `debian:bookworm-slim`) |
| Alpine | Avoid for this project (edition 2024 / current Dockerfiles use bookworm) |

## Components and secrets

| Component | Needs |
|-----------|--------|
| `jmodguard-bot` | Discord bot token, DB, `bot.owner_id`, poll interval |
| `jmodguard-api` | DB, OAuth client id/secret, `web.base_url`, **bot token** (WebUI unban via Discord REST) |
| SPA | No secrets |

Never commit tokens. Prefer platform secrets / env injection.

## CDN and OAuth

1. Point the admin hostname at Bunny CDN.
2. Serve SPA files from Storage (contents of `frontend/` for your release tag).
3. Origin rules: `/api/*` and `/oauth/*` (and login routes you expose) → API container.
4. Prefer same hostname for SPA + API so relative `fetch('/api/...')` works.
   If the SPA is truly on another origin, set `window.JMODGUARD_API_BASE` **and**
   configure `web.cors_origins` (or `JMODGUARD_CORS_ORIGINS`) to that SPA origin.
   The SPA sends its configured origin to the API for login/logout return navigation;
   only an exact configured CORS origin is accepted, so include the public SPA origin.
   The API enables credentialed CORS only when `cors_origins` is non-empty;
   `web.base_url` must be `https://…` so session cookies use `Secure` + `SameSite=None`.
5. Discord OAuth redirect URI must match
   `{web.base_url}/oauth/callback` (HTTPS in production).
6. Session cookies use the `Secure` flag when `web.base_url` starts with
   `https://`.

## Containers

Repo Dockerfiles:

- `Dockerfile.bot` — `cargo build --release --bin jmodguard-bot --no-default-features`
- `Dockerfile.api` — `cargo build --release --bin jmodguard-api` (feature `web`)

Both images use `WORKDIR /data`, `VOLUME ["/data"]`,
`JMODGUARD_DB_PATH=/data/jmodguard.db`, and `JMODGUARD_CONFIG=/data/config.toml`.
The API image also sets `JMODGUARD_WEB_BIND=0.0.0.0:8080` and exposes 8080 —
the host default `127.0.0.1` is unreachable from outside the container.

Mount **one** shared volume on `/data` for both containers (config + SQLite):

```bash
docker volume create jmodguard-data
docker run -d --name jmodguard-bot \
  -v jmodguard-data:/data \
  -e JMODGUARD_DISCORD_TOKEN=… \
  your-registry/jmodguard-bot:tag
docker run -d --name jmodguard-api -p 8080:8080 \
  -v jmodguard-data:/data \
  -e JMODGUARD_OAUTH_CLIENT_ID=… \
  -e JMODGUARD_OAUTH_CLIENT_SECRET=… \
  -e JMODGUARD_WEB_BASE_URL=https://admin.example.com \
  your-registry/jmodguard-api:tag
```

Do not give each container its own anonymous volume: that is two databases.

Push images to a registry your chosen host can pull. Pin tags to jModGuard
releases (`vX.Y.Z`).

## Database notes

- Config writers bump durable `config_version`; both processes poll it.
  Command registry writers also bump `slash_registry_version`; the bot
  re-syncs Discord slash commands when that advances.
- Migrations run in the **bot**. Start/migrate bot before API.
- Until remote open is implemented, leave `db.url` empty and configure both
  processes with the exact same local `db.path` on a shared filesystem. A
  separate container-local path is not a shared database.
- Bunny Database/libSQL becomes an option only after the remote client is
  implemented; then both processes can use `JMODGUARD_DB_URL` /
  `JMODGUARD_DB_TOKEN` and run independently.

## Sessions

API uses in-memory sessions (`MemoryStore`): fine for **one** API replica.
Restarting the API logs everyone out. Multi-replica needs a shared session
store (not shipped yet).

## First-time checklist (operator)

1. Discord application: bot token, privileged intents, OAuth2 redirect, invite.
2. Build/push bot + API images from a release tag.
3. Create the shared SQLite DB path; configure both processes with that exact
   same path on the same host/shared filesystem.
4. Upload SPA to Storage; configure CDN + origin to the API host.
5. Set env / config: token, OAuth, `base_url`, `owner_id`, bind as required.
6. Start **bot**, confirm migrate + shards; start **API**; login at the admin URL.
7. Plan backups (export / `VACUUM INTO` / provider tools). Preview backup
   tooling may be thin — automate exports yourself.

## Upgrade on this topology

Follow [UPGRADE.md](UPGRADE.md): backup → new images from tag → bot first
(migrations) → API → refresh SPA on Storage/CDN → verify. Rollback = previous
image tags + DB backup.

## Rough cost awareness

CDN and Storage are separate from your shared-DB bot/API host. Treat
third-party pricing pages as source of truth; numbers go stale quickly.
Anycast IPs add cost if you attach one — often unnecessary if CDN terminates
TLS.

## Related

- [ARCHITECTURE.md](ARCHITECTURE.md)
- [UPGRADE.md](UPGRADE.md)
- [CONFIGURATION.md](CONFIGURATION.md)
- [WEBUI.md](WEBUI.md)
