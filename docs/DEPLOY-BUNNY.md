# Deploy on Bunny.net

**Database compatibility:** this release requires a fresh database or the recognized
`jmodguard-core-1` family. Earlier databases/backups require their matching old release;
they cannot be adopted by this baseline. See [database setup and recovery](DATABASE-BASELINE.md).

Bunny.net is the first-class, out-of-the-box cloud provider: Bunny Database,
private Magic Containers for the bot and API, and the admin WebUI on Bunny
Storage served through a Pull Zone / CDN.

You can still self-host or use another provider ([SETUP.md](SETUP.md)).
Those paths are supported. They are not the designed Bunny install.

The admin UI is the only public HTTPS site. Magic Containers are private
workers: they talk to Bunny Database and (outbound) to Discord. They do not
contain `frontend/`, and they do not get public hostnames for people to
browse.

The UI files are published by **GitHub Actions in your private ops repo**.
Source pushes build artifacts; production changes require a reviewed ops release
selecting the source SHA and exact compatible bot/API image digests. Scheduled
and manual ops runs reconcile that selection. CSS-only assets can move without
an API rollout, but still require release approval. Keep Bunny credentials,
resource IDs and recovery evidence in ops.

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
If you add a custom hostname, enable Force SSL on it, set
`JMODGUARD_WEB_BASE_URL` and the Discord OAuth redirect to **that** host,
and do not log in on the system `*.b-cdn.net` hostname afterward (OAuth
CSRF cookies are host-only). On the admin Pull Zone, **Disable Cookies**
must be **off** (`DisableCookies=false`) or login cannot keep a session.

**First time (manual).** In the Storage Zone, **Browse Files**, and upload
those three files to the **root** so they are `/` (or `/index.html`),
`/app.js`, and `/style.css`. Start from the same git tag as the container
images so the first login works. After that, CSS/layout-only revisions can
publish without a new image; match the container revision again only when
the page needs new API endpoints.

**Later updates.** Publishing is meant to be GitHub Actions in your
**private** ops repo (Bunny Storage passwords do not belong in this public
project), using
[docs/examples/private-ops/publish-frontend.yml](examples/private-ops/publish-frontend.yml)
and its [private ops installation instructions](examples/private-ops/README.md).
A private repo is not required for a one-off upload. Copy the workflow
into the private repo when you want repeats. Store these secrets there:

| Secret | What it is |
|---|---|
| `BUNNY_STORAGE_ZONE` | Storage zone name |
| `BUNNY_STORAGE_PASSWORD` | Storage zone HTTP AccessKey (not the account API key) |
| `BUNNY_STORAGE_ENDPOINT` | Regional host, default `storage.bunnycdn.com` |
| `BUNNY_PULL_ZONE_ID` | Pull Zone id, used to purge CDN cache after upload |
| `BUNNY_API_KEY` | Account API key for API state inspection and CDN purge |
| `JMODGUARD_CHECKOUT_TOKEN` | PAT with `contents:read` on this repository. Required while jModGuard is private (the ops `GITHUB_TOKEN` cannot clone a different private repo). Omit once this repository is public. |

Install all files and variables listed in the
[private ops README](examples/private-ops/README.md), including its disabled
`frontend-release.json` and ops-owned Python helpers. The example publishes only
an approved full source SHA from ops `main`. It is a frontend publisher; use your
container runbook to deploy compatible bot/API digests and apply migrations.

Release review must establish API compatibility and completed recovery evidence.
The frontend checker verifies live Storage contents, publication completion and,
for changed JS/HTML, the approved active API digest plus `/api/me` readiness.
CSS-only changes can publish independently of an API rollout. API deployment
workflows must share the example's concurrency group. `/health` alone cannot
prove application readiness because it responds before database bootstrap.

Publication requires both a valid Pull Zone id and purge credentials before any
Storage access. All backup downloads must complete. On an upload error, restore
all potentially modified assets, including a PUT accepted remotely despite a
lost response. A mutable receipt records completion only after CDN purge; an old
Actions cache entry or matching origin files cannot prove that purge finished.
A failed purge leaves the new origin files and a pending receipt for retry.
Uploads and rollback are not atomic at the CDN; verify the public site afterward.

For a bad UI, commit an approved known-good release in ops and confirm its
publication and purge. Disable publication and wait for in-flight runs before
manual repairs; disabling a workflow does not undo an existing upload.

Leave `window.JMODGUARD_API_BASE` unset. The Pull Zone also sends `/api`,
`/login`, `/logout`, and `/oauth/callback` to the API container (step 6),
so the UI and API are same-origin. Do not put a database URL or token in
these files.

## 3. Build and publish both images

GHCR has two publish styles:

- **`images` workflow** (a GitHub Release, or Run workflow with a tag or
  full SHA): builds **both** images. It still rejects branch names.
- **Path-filtered workflows on `main`**: `Publish bot image` and `Publish
  API image`. Migration changes trigger the bot workflow because the bot
  applies SQL. A push changing both bot code and migrations builds the bot
  image once. These run only on push to `main` (no manual dispatch, so a
  feature branch cannot retag `:latest`). A CSS-only change does not
  build either container. Manual publishes still use the `images`
  workflow with a tag or full SHA.

Buildkite runs automatic tests; these GitHub Actions workflows publish the
container images. They can use self-hosted compute while their logs and status
remain in GitHub Actions. Each publisher first runs a small `pick-runner` job on
`ubuntu-latest`, then sends the image build to an online runner labeled
`self-hosted`, `linux`, and `proxmox`. A busy online runner queues the build.
If no matching runner is online or the runners API cannot be read, the image
build falls back to GitHub-hosted compute. `RUNNER_READ_TOKEN` must be able to
list repository runners for reliable selection. The separate `CI failover`
workflow runs tests on GitHub-hosted compute only when manually dispatched.

There is nothing for Magic Containers to pull until one of those publishes.
Each run retags `:latest` for **that** package and retains older versions.
Match frontend to the running API when the page
needs new endpoints. CSS/layout-only frontend revisions can publish
without a new image.
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
Publishing does not prune older GHCR versions. Retain the currently approved
and rollback digests before building a candidate. Any later manual cleanup must
consult deployment inventories; the public source cannot know which private
installations still use an image. Rebuilding the same source SHA may produce a
different digest and is a new artifact requiring approval, not recovery of the
original image bytes.

Point Magic Containers at those recorded **digests**, not floating
`:latest`. The digest must be the image manifest, not an OCI index — Bunny
resolves `:latest` to the index digest when attestations are attached and
then fails to pull. After ops approves the release, deploy its exact digests
and verify rollout.

The Bunny API image is JSON + OAuth only. It does not embed `frontend/`.
A default local API build still can (`embed-ui`).

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
exactly one instance. If the dashboard refuses min/max 1 (some trial
accounts cap Deploy UI at a higher minimum), create the app with
`POST /mc/apps` instead. Do **not** add a CDN or Anycast endpoint. The bot
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
and `JMODGUARD_BOT_STATUS`. Prefix is written on migrate-open into durable
`meta.settings_terminal_defaults`. Status is the Discord presence text.
`JMODGUARD_LOG_DISCORD_CONCURRENCY` (default 8) and `JMODGUARD_LOG_DISCORD_TIMEOUT_SECS`
(default 10) bound best-effort Discord log delivery. Routine chat/member history is no
longer collected; the historical retention field has no active collection effect.

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
Enable sticky sessions on that endpoint. Prefer cookie name
`jmodguard_session` (the API session cookie). If the dashboard cookie
field is read-only (`BUNNYNET_MC_SESSIONID` on some trial UIs), one
replica is still fine. Do not give operators this endpoint hostname; it
exists so the Storage Pull Zone can origin-pull the API. GET/PATCH of a
Magic Container via the Bunny API returns every env var — never paste
those responses.

The Bunny API image does not embed or serve the UI at `/`; a default
local API build (`embed-ui`) may serve `/`. Do not treat `/` on
the container or its endpoint hostname as a ready check. Platform and
Pull Zone liveness is `GET`/`HEAD /health` on port `8080`. Until axum
takes over (database open, API state primary-version prime, router
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
`JMODGUARD_DB_POOL_SIZE` (this process's pool; the bot has its own). Prefix
and bot status are bot-owned — set those on the bot app.

`JMODGUARD_WEB_BASE_URL` is the **CDN** origin operators type into the
browser (custom hostname if you added one), not a container hostname and
not a leftover `*.b-cdn.net` system host after you cut over. Discord
OAuth redirect URI:

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

1. Add an edge rule whose action is **Change Origin URL** (`OriginUrl`,
   ActionType 2). The origin host is the API **endpoint Pull Zone**
   `*.b-cdn.net` hostname from step 5 (Bunny creates that PZ when you add
   the CDN endpoint). Do **not** give operators that hostname. Do **not**
   use **Override Origin: Magic Containers** (`OriginMagicContainers`,
   ActionType 22) on a Storage-origin Pull Zone: that rule can match and
   still serve Bunny’s “We’re deploying your app” splash while the API is
   already live.
2. Bypass Perma-Cache on the API paths (ExtraAction BypassPermaCache).
   The API also sends `Cache-Control: private, no-store`, but a cached
   `/api/me` would still leak sessions if you skip the bypass.
3. Trigger on request URL for `/api*`, `/login*`, `/logout*`, `/oauth*`,
   and `/health*`. Bunny allows **at most five** PatternMatches per
   trigger. Matching both a custom hostname and the system `*.b-cdn.net`
   host means **two** triggers (five path patterns each), MatchAny.
4. Set **Disable Cookies** to off on this Pull Zone (`DisableCookies` must
   be **false**) so login cookies are not stripped. If you update the zone
   via the Bunny API, send only the fields you are changing — posting the
   full GET body back fails (`model.invalid` on `id`).
5. Everything else stays on Storage (`/`, `/app.js`, `/style.css`).

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
4. Open the **CDN** hostname that matches `JMODGUARD_WEB_BASE_URL`. The
   admin UI loads from Storage. Login with Discord. OAuth returns to that
   origin. Probe `/health` (200 `ok`); do not use `/` on the Bunny API
   origin as a ready check.
5. Change a harmless guild prefix. Confirm the bot picks it up, then
   restore it.
6. Configure an `automod` log channel, enable a dry-run rule and check its Discord
   report. Dry-run observations no longer appear as new SQL Audit Log rows.
7. Restart the **API** app only: bot stays connected; users log in again.
8. Restart the **bot** app only: API stays up; bot returns online.

CI’s `tests/remote_database_smoke.rs` proves bot bootstrap and API
schema-open against a local libSQL server, not Bunny TLS.
`tests/dual_process_smoke.rs` covers the local SQLite process contract.

## Operating constraints

- Bunny Database is Public Preview. Internal snapshots are not an
  operator backup. See [BACKUP-RESTORE.md](BACKUP-RESTORE.md).
- One bot replica. One API replica (in-memory sessions). Sticky session
  cookie on the API Magic Container endpoint: `jmodguard_session` when
  the dashboard allows it; a locked `BUNNYNET_MC_SESSIONID` is acceptable
  at one replica.
- On the Storage Pull Zone, origin-pull the API with **OriginUrl**, not
  OriginMagicContainers. Disable Cookies must be off. Five PatternMatches
  per trigger maximum.
- Never paste Magic Container GET/PATCH JSON (it includes every secret).
- SIGTERM stops the Discord gateway and the API listener cleanly, including
  during remote database retry (startup then exits 0 instead of failing).
- Missing Bunny DB secrets must not fall through to local SQLite
  (`JMODGUARD_REQUIRE_REMOTE_DB=1`).
- Pin bot and API image **digests** in the private ops repo and point
  Magic Containers at those digests, not floating `:latest`. Storage
  `frontend/` can move independently for CSS/layout; do not mix a new
  website with an old API when the page needs new endpoints. A later
  `images` publish retags `:latest` but retains old versions. Review any manual
  pruning against deployed and rollback image inventories; a recorded digest
  alone is not a retained artifact.
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
