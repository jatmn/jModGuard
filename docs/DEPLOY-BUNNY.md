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

Dispatch the workflow with the same jModGuard git tag as the container
images. The runner checks out that tag and uploads `index.html`, `app.js`,
and `style.css` to the Storage Zone root (`/`, `/app.js`, `/style.css`).
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

CI builds `linux/amd64` images and, on `main` and version tags, pushes
them to GHCR as packages of this public repo:

```text
ghcr.io/jatmn/jmodguard-bot:<tag>
ghcr.io/jatmn/jmodguard-api:<tag>
```

Use the `sha-<short git SHA>` tag (or `main` after that commit is on the default
branch). `latest` is only applied to version tags (`v*`).

The API image is JSON + OAuth only. It does not embed `frontend/`.

Magic Containers trusts public GHCR. The `images` workflow tries to mark
those packages public after the first push. If that is denied, set
visibility on
`https://github.com/jatmn/jModGuard/pkgs/container/jmodguard-bot` and
`.../jmodguard-api`. If you keep them private, add a GitHub registry PAT
in Magic Containers.

To build locally:

```bash
docker build --platform linux/amd64 -f Dockerfile.bot -t ghcr.io/jatmn/jmodguard-bot:TAG .
docker build --platform linux/amd64 -f Dockerfile.api -t ghcr.io/jatmn/jmodguard-api:TAG .
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

Plus the database pair from step 1.

The bot is the schema authority. Wait for logs containing
`database ready (migrate=true)` before expecting the API to open.

## 5. Create the API app (origin only, no public website)

Create a second Magic Container app from the API image. Same
single-region, exactly-one-instance topology (sessions are in-memory).
Do **not** attach a custom domain or a browseable Magic Containers CDN
hostname. The Pull Zone from step 2 is the public URL.

The API still listens on container port `8080` so Bunny can origin-pull
it and so platform health checks can hit `GET`/`HEAD /health`. Until
axum takes over (database open, AppState primary-version prime, router
build), only `/health` returns `200`. `/api` returns `503` with JSON
`{"error":"not ready"}` and `Retry-After`; `/login` and `/oauth/callback`
return HTML `503` that refreshes the same URL (so an in-flight Discord
redirect is not replaced by a JSON error page). The admin page retries
`/api/me` while the origin is still starting (`not ready`, `502`/`504`,
or a dropped origin pull). OAuth CSRF is a cookie, so a restart between
`/login` and Discord's return still completes. Enable sticky sessions
on that origin: login sessions live in that one process.

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

`JMODGUARD_WEB_BASE_URL` is the **CDN** origin operators open, not a
container hostname. Discord OAuth redirect URI:

```text
https://YOUR-CDN-HOSTNAME/oauth/callback
```

Leave `JMODGUARD_CORS_ORIGINS` unset. Same-origin via the Pull Zone does
not need CORS. The API still needs the Discord bot token for OAuth, the
guild list, and WebUI unban.

The API never applies migrations. It waits for the bot bootstrap marker
(about six minutes on a remote database).

## 6. Point API paths on the Pull Zone at the API container

On the Pull Zone from step 2 (Storage origin for the files):

1. Add the API Magic Container as an alternate origin (Magic Containers
   origin / endpoint for container port `8080`).
2. Add edge rules so these paths go to that origin and are **not**
   cached:
   - `/api*`
   - `/login*`
   - `/logout*`
   - `/oauth*`
   - `/health*`
3. Everything else stays on Storage (`/`, `/app.js`, `/style.css`).
4. Disable caching / bypass Perma-Cache on the API paths. Cached
   `/api/me` would leak sessions.

After this, a browser on the CDN hostname loads the admin UI from
Storage and calls `/api/...` on the same host; Bunny forwards those
requests to the private API container.

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
- One bot replica. One API replica (in-memory sessions).
- SIGTERM stops the Discord gateway and the API listener cleanly, including
  during remote database retry (startup then exits 0 instead of failing).
- Missing Bunny DB secrets must not fall through to local SQLite
  (`JMODGUARD_REQUIRE_REMOTE_DB=1`).
- Keep bot image, API image, and Storage `frontend/` on the same git
  tag. Pin image digests in the private ops repo if you want an
  immutable rollout record.
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
