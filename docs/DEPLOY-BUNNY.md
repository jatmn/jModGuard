# Deploy on Bunny.net

Bunny.net is the first-class, out-of-the-box cloud provider: Bunny Database,
private Magic Containers for the bot and API, and the admin WebUI on Bunny
Storage served through a Pull Zone / CDN.

You can still self-host or use another provider ([SETUP.md](SETUP.md)).
Those paths are supported. They are not the designed Bunny install.

The admin UI is the only public HTTPS site. Magic Containers are private
workers: they talk to Bunny Database and (outbound) to Discord. They do not
contain `frontend/`, and they do not get public hostnames for people to
browse.

The UI files are published by **GitHub Actions in your private ops repo**
(not by this public repository). That private runner checks out a jModGuard
tag and uploads `frontend/` to Storage. Bunny credentials stay in that
private repo’s secrets. This public repo is the source; it is not the
place those keys live.

```text
Browser  →  Bunny CDN (Pull Zone)
              │
              ├─ /  /app.js  /style.css   ← Storage Zone (frontend/)
              └─ /api  /login  /oauth/*   ← API Magic Container (origin only)
                                              │
Discord  ←  bot Magic Container               │
              │                               │
              └──────── Bunny Database ───────┘
                       (remote libSQL)
```

The pages never open the database from the browser. A database token in
the webpage would be public. The browser talks to the API; the API (and
the bot) are what open SQL.

The bot and the API also never call each other. They share the database.
The bot’s extra outbound is the Discord gateway. The API’s extra outbound
is Discord OAuth, guild listing, and unban.

Local `cargo run` still embeds `frontend/` in the API for SETUP.md. That
is a laptop / self-host convenience. It is not the Bunny deploy.

## 1. Create the database

Create a Bunny Database. In **Access**, generate full-access credentials
for each container. Both need write access: the bot writes moderation
state and migrations; the API writes configuration changes from the UI.

Bunny can inject:

```dotenv
BUNNY_DATABASE_URL=libsql://DATABASE_ID.lite.bunnydb.net
BUNNY_DATABASE_AUTH_TOKEN=SECRET
```

jModGuard reads those names. Portable aliases `JMODGUARD_DB_URL` and
`JMODGUARD_DB_TOKEN` take precedence when both are non-empty. URL and
token are an atomic pair. Blank values count as unset. Never commit an
access token.

## 2. Publish the admin UI to Storage + CDN

The files are `frontend/index.html`, `frontend/app.js`, and
`frontend/style.css` in this repository. How to get them and where they
go is also summarized in [WEBUI.md](WEBUI.md#getting-and-hosting-the-files).

Create a Storage Zone and a Pull Zone whose origin is that Storage Zone.
The Pull Zone hostname (or a custom domain on it) is what operators open.

**First time (manual).** In the Storage Zone, **Browse Files**, and upload
those three files to the **root** so they are `/` (or `/index.html`),
`/app.js`, and `/style.css`. Use the same git tag as the container images.

**Later updates.** Publishing is meant to be GitHub Actions in your
**private** ops repo (Bunny Storage passwords do not belong in this public
project), using
[docs/examples/private-ops/publish-frontend.yml](examples/private-ops/publish-frontend.yml)
and [scripts/publish-frontend-bunny.sh](../scripts/publish-frontend-bunny.sh).
A private repo is not required for a one-off upload. Copy the workflow
into the private repo when you want repeats. Store these secrets there:

| Secret | What it is |
|---|---|
| `BUNNY_STORAGE_ZONE` | Storage zone name |
| `BUNNY_STORAGE_PASSWORD` | Storage zone HTTP AccessKey (not the account API key) |
| `BUNNY_STORAGE_ENDPOINT` | Regional host, default `storage.bunnycdn.com` |
| `BUNNY_PULL_ZONE_ID` | Pull Zone id, used to purge CDN cache after upload |
| `BUNNY_API_KEY` | Account API key, used only for the purge |
| `JMODGUARD_CHECKOUT_TOKEN` | PAT with `contents:read` on this repository. Required while jModGuard is private (the ops `GITHUB_TOKEN` cannot clone a different private repo). Omit once this repository is public. |

Dispatch the workflow with the **same** 40-character jModGuard SHA recorded
in the ops pin file (`| Source SHA |` in `docs/BUNNY.md` in the example —
not the first hex in that file, which may be a blob URL). The copied
workflow checks out that pin file first and **rejects** any other `ref`
(including `main` and tags) before it clones this repository, so Storage
cannot drift from the running images. The runner then uploads `index.html`,
`app.js`, and `style.css` to the Storage Zone root (`/`, `/app.js`,
`/style.css`).
If a later file fails to upload, the script restores files it successfully
backed up (HTTP 200) or deletes files it confirmed were missing (HTTP 404).
It refuses to start if it cannot read the current objects. A failed Pull
Zone purge leaves the new files on Storage and exits non-zero so you can
purge the zone; rolling Storage back cannot un-cache edges that already
fetched the new files.

Leave `window.JMODGUARD_API_BASE` unset. The Pull Zone also sends `/api`,
`/login`, `/logout`, and `/oauth/callback` to the API container (step 6),
so the UI and API are same-origin. Do not put a database URL or token in
these files.

## 3. Build and publish both images

GHCR has two publish styles:

- **`images` workflow** (a GitHub Release, or Run workflow with a tag or
  full SHA): builds **both** images. It still rejects branch names.
- **Path-filtered workflows on `main`**: `Publish bot image`, `Publish
  database image` (bot image, because the bot applies SQL), and `Publish
  API image`. A CSS-only change does not build either container.

There is nothing for Magic Containers to pull until one of those publishes.
Each run retags `:latest` for **that** package and may delete older versions
of that package only. Do not mix a new website with an old API when the page
needs new endpoints.
That builds **single-manifest** `linux/amd64` images (no Buildx
provenance/SBOM index) and pushes:

```text
ghcr.io/jatmn/jmodguard-bot:latest
ghcr.io/jatmn/jmodguard-api:latest
```

The same digest is also tagged with the release name (for example `v1.2.3`).
A manual run tags the digest with the peeled commit SHA and, if
`source_ref` was a Git tag, that tag with `/` replaced by `-`
(`release/2025` → `release-2025`). Tags are resolved to a commit SHA
before checkout so a force-moved tag cannot change what gets compiled.
The workflow then deletes older GHCR versions so only the current release
image is stored. A digest recorded from a previous publish may stop
pulling after the next run unless GHCR retention keeps it or you re-run
`images` with that earlier `source_ref`. Record bot and API digests
before the next publish if you may need to roll back.

Point Magic Containers at those recorded **digests**, not floating
`:latest`. The digest must be the image manifest, not an OCI index — Bunny
resolves `:latest` to the index digest when attestations are attached and
then fails to pull. After each publish, redeploy or restart the apps so
they pull the intended image.

The API image is JSON + OAuth only. It does not embed `frontend/`.

Magic Containers trusts public GHCR. The `images` workflow tries to mark
those packages public after the first push. If that is denied, set
visibility on
`https://github.com/jatmn/jModGuard/pkgs/container/jmodguard-bot` and
`.../jmodguard-api`. If you keep them private, add a GitHub registry PAT
in Magic Containers.

To build locally (`GITHUB_TOKEN` must be set; BuildKit secret is required):

```bash
docker build --platform linux/amd64 --secret id=github_token,env=GITHUB_TOKEN -f Dockerfile.bot -t ghcr.io/jatmn/jmodguard-bot:TAG .
docker build --platform linux/amd64 --secret id=github_token,env=GITHUB_TOKEN -f Dockerfile.api -t ghcr.io/jatmn/jmodguard-api:TAG .
docker push ghcr.io/jatmn/jmodguard-bot:TAG
docker push ghcr.io/jatmn/jmodguard-api:TAG
```

Magic Containers currently requires `linux/amd64`.

The images default `JMODGUARD_LOG_DIRECTORY` to empty so logs go to
stdout/stderr. They set `JMODGUARD_REQUIRE_REMOTE_DB=1`: missing Bunny
secrets exit instead of opening SQLite on ephemeral disk.

## 4. Create the bot app (no public endpoint)

Create a Magic Container app from the bot image. **Single region**,
exactly one instance. Do **not** add a CDN or Anycast endpoint. The bot
is not a website. Platform health checks (Monitoring → HTTP GET `/health`
on port `8080`) stay inside Bunny; they are not a public URL.

```dotenv
JMODGUARD_DISCORD_TOKEN=SECRET
JMODGUARD_OWNER_ID=123456789012345678
JMODGUARD_CONFIG_POLL_SECS=5
JMODGUARD_LOG_LEVEL=info
JMODGUARD_LOG_DIRECTORY=
```

Optional when you have no `config.toml` on the bot container (typical):
`JMODGUARD_DB_POOL_SIZE` (this process's pool), `JMODGUARD_DEFAULT_PREFIX`,
`JMODGUARD_RETENTION_DAYS`, and `JMODGUARD_BOT_STATUS`. Prefix and retention
are written on migrate-open into durable `meta.settings_terminal_defaults`.
Status is the Discord presence text.

Plus the database pair from step 1.

The bot is the schema authority. Wait for logs containing
`database ready (migrate=true)` before expecting the API to open.

Create the Discord application **before** the containers (intents, OAuth
redirect, invite). Those steps are in this file under [Discord](#discord-application-required-for-bunny)
so you do not have to bounce to SETUP.md.

## 5. Create the API app (origin only, no public website)

Create a second Magic Container app from the API image. Same
single-region, exactly-one-instance topology (sessions are in-memory).
Do **not** attach a custom domain to this app. The Pull Zone from step 2 is
the public URL.

Bunny can only origin-pull a Magic Container through an **Endpoint**. On
this API app, **Endpoints → Add CDN endpoint**, container port `8080`.
Enable sticky sessions on that endpoint and use cookie name
`jmodguard_session` (the API session cookie). Do not give operators this
endpoint hostname; it exists so the Storage Pull Zone can target
`MagicContainersAppId` + `MagicContainersEndpointId`.

The API still listens on container port `8080` so Bunny can origin-pull
it and so platform health checks can hit `GET`/`HEAD /health`. Until
axum takes over (database open, AppState primary-version prime, router
build), only `/health` returns `200`. `/api` returns `503` with JSON
`{"error":"not ready"}` and `Retry-After`; `/login` and `/oauth/callback`
return HTML `503` that refreshes the same URL (so an in-flight Discord
redirect is not replaced by a JSON error page). The admin page retries
`/api/me` while the origin is still starting (`not ready` for up to eight
minutes). Consecutive `502`/`504`, dropped origin pulls, or origin pulls
that never complete stop after 90 seconds of those failures with an error
instead of spinning forever. OAuth CSRF is a cookie, so a restart between
`/login` and Discord's return still completes. Login sessions live in
that one process (`jmodguard_session`).

```dotenv
JMODGUARD_DISCORD_TOKEN=SECRET
JMODGUARD_WEB_BIND=0.0.0.0:8080
JMODGUARD_WEB_BASE_URL=https://YOUR-CDN-HOSTNAME
JMODGUARD_OAUTH_CLIENT_ID=SECRET
JMODGUARD_OAUTH_CLIENT_SECRET=SECRET
JMODGUARD_OWNER_ID=123456789012345678
JMODGUARD_CONFIG_POLL_SECS=5
JMODGUARD_LOG_LEVEL=info
JMODGUARD_LOG_DIRECTORY=
```

Optional when you have no `config.toml` on the API container (typical):
`JMODGUARD_DB_POOL_SIZE` (this process's pool; the bot has its own). Prefix,
retention, and bot status are bot-owned — set those on the bot app.

`JMODGUARD_WEB_BASE_URL` is the **CDN** origin operators open, not a
container hostname. Discord OAuth redirect URI:

```text
https://YOUR-CDN-HOSTNAME/oauth/callback
```

Leave `JMODGUARD_CORS_ORIGINS` unset. Same-origin via the Pull Zone does
not need CORS. The API still needs the Discord bot token for OAuth, the
guild list, and WebUI unban.

Both containers need **full-access** database tokens. A read-only Bunny
token will fail API startup (the API probes a no-op write after schema
open).

The API never applies migrations. It waits for the bot bootstrap marker
(about six minutes on a remote database), then exits if that wait
expires — start the bot first and wait for `database ready (migrate=true)`.

## 6. Point API paths on the Pull Zone at the API container

On the Pull Zone from step 2 (Storage origin for the files):

1. Add an edge rule whose action is **Override Origin: Magic Containers**
   (`OriginMagicContainers`). Pick the API app and the CDN endpoint from
   step 5 (those two IDs are required; “Change Origin URL” to a public
   Magic Container hostname is the wrong tool).
2. Trigger on request URL for these paths, and **bypass cache /
   Perma-Cache** on them. The API also sends `Cache-Control: private,
   no-store`, but a cached `/api/me` would still leak sessions if you
   skip the bypass:
   - `/api*`
   - `/login*`
   - `/logout*`
   - `/oauth*`
   - `/health*`
3. Everything else stays on Storage (`/`, `/app.js`, `/style.css`).

After this, a browser on the CDN hostname loads the admin UI from
Storage and calls `/api/...` on the same host; Bunny forwards those
requests to the API container. The Magic Container endpoint hostname is
not the product URL.

## Discord application (required for Bunny)

Do this in the [Discord developer portal](https://discord.com/developers/applications)
in addition to the container env vars. SETUP.md has the same list for
self-host.

1. **Bot → Privileged Gateway Intents:** enable **Server Members** and
   **Message Content**. Presence is unused.
2. **OAuth2 → Redirects:** add exactly
   `https://YOUR-CDN-HOSTNAME/oauth/callback` (must match
   `JMODGUARD_WEB_BASE_URL`).
3. **Invite** with scopes `bot` and `applications.commands`, and at least
   Kick Members, Ban Members, Moderate Members, Manage Messages, Read
   Message History, View Channels, Send Messages, Manage Nicknames
   (Manage Roles only if you use role plan steps).

## 7. Acceptance test

1. Bot logs: `database ready (migrate=true)`, then shard startup. No
   public hostname.
2. API logs: `database ready (migrate=false)` and listening. No public
   website hostname on the container.
3. Each app: one region, one replica.
4. Open the **CDN** hostname. The admin UI loads from Storage. Login
   with Discord. OAuth returns to the CDN origin.
5. Change a harmless guild prefix. Confirm the bot picks it up, then
   restore it.
6. Configure a dry-run rule and check the audit event.
7. Restart each container. Both reconnect to the same database. The UI
   still comes from CDN.

CI’s `tests/remote_database_smoke.rs` proves bot bootstrap and API
schema-open against a local libSQL server, not Bunny TLS.
`tests/dual_process_smoke.rs` covers the local SQLite process contract.

## Operating constraints

- Bunny Database is Public Preview. Internal snapshots are not an
  operator backup. See [BACKUP-RESTORE.md](BACKUP-RESTORE.md).
- One bot replica. One API replica (in-memory sessions). Sticky session
  cookie on the API Magic Container endpoint: `jmodguard_session`.
- SIGTERM stops the Discord gateway and the API listener cleanly, including
  during remote database retry (startup then exits 0 instead of failing).
- Missing Bunny DB secrets must not fall through to local SQLite
  (`JMODGUARD_REQUIRE_REMOTE_DB=1`).
- Keep bot image, API image, and Storage `frontend/` on the same git
  tag or commit SHA. Pin image **digests** in the private ops repo and
  point Magic Containers at those digests. A later `images` publish
  retags `:latest` and may prune older GHCR versions, so a recorded
  digest may stop pulling unless you re-run `images` for that earlier
  `source_ref` or configure GHCR retention.
- Do not add public Magic Container endpoints for the bot. Do not use a
  Magic Container hostname as the product URL.

## References

- [Bunny Database overview](https://docs.bunny.net/database)
- [Bunny Storage static sites](https://docs.bunny.net/storage/static-site-hosting)
- [Magic Containers endpoints](https://docs.bunny.net/magic-containers/endpoints)
- [Magic Containers health checks](https://docs.bunny.net/magic-containers/health-checks)
- [Backup and restore status](BACKUP-RESTORE.md)
- [Configuration reference](CONFIGURATION.md)
- [Architecture](ARCHITECTURE.md)
