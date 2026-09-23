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

Publish the five `frontend/` files for an initial manual setup, including the
Administration page. New trusted Buildkite publications also create an independent
WebUI GHCR package; the owner update path is described in
[BUNNY-UPDATES.md](BUNNY-UPDATES.md). A private ops repository is not required.

```text
Browser  →  Bunny CDN (Pull Zone)
              │
              ├─ /  /administration/  /assets/releases/   ← Storage Zone
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

## 1. Create the core and guild databases

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

Create a **separate** Bunny Database for the shared guild store. Configure both
containers with credentials for that same guild database, in addition to the core
credentials above. Never point the guild store at the core database or the frontend
Storage Zone. The guild database must be empty on first initialization or already have
the matching `jmodguard-guild-1` identity; runtime upgrades it to version 8.

```dotenv
JMODGUARD_GUILD_STORE_URL=libsql://GUILD_DATABASE_ID.lite.bunnydb.net
JMODGUARD_GUILD_STORE_TOKEN=SECRET_FOR_GUILD_DATABASE
JMODGUARD_EVIDENCE_BACKEND=none
```

The shared-store URL/token must be set together. No TOML file is needed for this
single shared-store install. An optional `JMODGUARD_GUILD_STORE_ID` selects an existing
32-hex identity; normally leave it unset so both processes rediscover the same stamped
identity. Start the bot first and wait for Gateway startup before starting the API.
Dedicated stores and additional shared stores use protected `[[guild_stores.stores]]`
configuration at `JMODGUARD_CONFIG` (the images default to `/data/config.toml`).

`none` is appropriate for a fresh install without private objects: selected inline text
and cases still work, and no attachment capture worker is enabled. It cannot erase
existing objects. An installation with private evidence must retain its owning backend,
resource identity and credentials; see [tenant runtime](TENANT-RUNTIME.md) and
[Bunny evidence](BUNNY-EVIDENCE.md). The local evidence default requires a durable,
explicit filesystem root when the core is remote, so do not leave that default implicit
in these containers. Never use the public frontend zone or a backup-only zone as the
private evidence resource.

## 2. Publish the admin UI to Storage + CDN

The files for a first manual upload are `frontend/index.html`, `frontend/app.js`,
`frontend/style.css`, `frontend/administration/index.html`, and
`frontend/administration.js`. How to get them and where they
go is also summarized in [WEBUI.md](WEBUI.md#getting-and-hosting-the-files).
The browser loads daisyUI CSS and Headless UI from a CDN at runtime; do not
upload those libraries into Storage. To serve them from your own pull zone
instead of the public defaults, set `window.JMODGUARD_DAISYUI_HREF` and
`window.JMODGUARD_ESM_BASE` before `app.js` (same override pattern as
`window.JMODGUARD_API_BASE`). `app.js` writes the CSS href onto the daisyUI
`<link>`; setting the CSS override above that link in `index.html` avoids a
second stylesheet request.

Create a Storage Zone and a Pull Zone whose origin is that Storage Zone.
When creating the Storage Zone, select a **primary region and at least one replication region**.
Bunny's create form requires a replication region; do not try to skip
replication or create a primary-only zone. Replication regions cannot be
removed later, so pick the smallest set you actually want (one additional
region is enough).

The public admin URL is the hostname operators will type into the browser.
Choose one and use it everywhere (`JMODGUARD_WEB_BASE_URL`, Discord OAuth
redirect, and the login bookmark):

- **Bunny hostname:** the Pull Zone `*.b-cdn.net` hostname.
- **Owned domain or subdomain:** add that hostname on the Pull Zone
  (General → Hostnames), CNAME it to the `*.b-cdn.net` hostname, enable SSL
  and Force SSL on it, then use **that** host. After cutover, do not log in
  on the system `*.b-cdn.net` hostname (OAuth CSRF cookies are host-only).

On the admin Pull Zone, Caching → General, **Strip Response Cookies** must
be **off** (`DisableCookies=false`) or login cannot keep a session.

**First time (manual, public source).** In the Storage Zone, **Browse Files**,
and upload those five files from this repository (or the same paths from a
signed release manifest for the same tag) to the **root** so they are `/` (or `/index.html`),
`/app.js`, `/style.css`, `/administration/index.html`, and
`/administration.js`. Start from the same git tag as the container
images so the first login works. After that, CSS/layout-only revisions can
publish without a new image; match the container revision again only when
the page needs new API endpoints. daisyUI and Headless UI stay on the runtime
CDN; they are not a fourth Storage object.

**Later updates.** The owner Administration page can install a WebUI package
from GHCR using the Storage Zone password after the rollout gates in
[BUNNY-UPDATES.md](BUNNY-UPDATES.md). The older
[scripts/publish-frontend-bunny.sh](../scripts/publish-frontend-bunny.sh)
only handles the three legacy root files and still requires the account API key
for cache purge; it is not part of the managed update path. CSS/layout-only revisions can
publish without a new image; match the container revision again only when the
page needs new API endpoints. That script backs up existing Storage objects,
restores them if an upload fails, and records completion only after a
successful Pull Zone purge. Uploads are not atomic at the CDN; verify the public
site afterward. A failed purge leaves the new origin files for a retry.

| Variable | What it is |
|---|---|
| `BUNNY_STORAGE_ZONE` | Storage zone name |
| `BUNNY_STORAGE_PASSWORD` | Storage zone HTTP AccessKey (not the account API key) |
| `BUNNY_STORAGE_ENDPOINT` | Regional host; default `storage.bunnycdn.com` |
| `BUNNY_PULL_ZONE_ID` | Pull Zone id for the publication receipt and purge target |
| `BUNNY_API_KEY` | Account API key used to purge the Pull Zone after upload |

Optional `JMODGUARD_FRONTEND_ROOT` points at a directory containing
`index.html`, `app.js`, and `style.css` (defaults to this repository's
`frontend/`).

Leave `window.JMODGUARD_API_BASE` unset. The Pull Zone also sends `/api`,
`/login`, `/logout`, and `/oauth/callback` to the API container (step 6),
so the UI and API are same-origin. Do not put a database URL or token in
these files.

## 3. Build and publish both images

Buildkite's self-hosted `linux` queue runs tests and publishes the bot/API
images after both test jobs pass. This repository has no GitHub Actions CI or
image jobs or runner fallback. Dependabot version updates use GitHub-hosted
dynamic workflows only while **Dependabot on self-hosted runners** stays
disabled; they are not image publication. GitHub still displays the Buildkite status and a link to
its build logs. See [Buildkite setup](BUILDKITE.md) for GHCR credentials,
agent requirements, tag builds and manual publication.

- **Push to `main`**: each component compares its Docker inputs with its last
  published source revision by inspecting public GHCR history before fetching GHCR push credentials. Unchanged inputs (including frontend-only changes)
  skip the image build. Private packages still require registry access and may rebuild. Both Dockerfiles consume shared Rust source and migrations,
  so those changes can rebuild both images, once each. Missing revision/history
  information conservatively causes a rebuild.
- **Tag build**: when Buildkite's **Build tags** setting is enabled, a pushed tag
  publishes both images after tests. This replaces the GitHub Release trigger;
  publishing a GitHub Release alone does not create an additional image build.
- **Manual publication**: start a Buildkite build on current `main` with
  `JMODGUARD_PUBLISH_SOURCE` set to a full lowercase 40-character commit SHA or
  `refs/tags/<tag>`. A single preparation job resolves that source for both
  images. Plain branch names and abbreviated SHAs are rejected. A manual build
  without this variable runs tests only. PR builds never publish.

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

A tag build also tags the digest with the tag name (for example `v1.2.3`).
A manual tag source adds that tag too, with `/` and `+` replaced by `-`
(`release/2025` → `release-2025`). An invalid Docker tag omits only the extra
tag; `latest` and the full SHA are still published. Both components use one
resolved source SHA, so moving a tag between jobs cannot split their sources.
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

Magic Containers trusts public GHCR. Set each image package public in GitHub
if it is not already public; publishing does not change package visibility.
Use
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
That add-endpoint screen creates the endpoint; it does **not** expose sticky-session settings.
After the endpoint exists, open it and enable sticky sessions. Prefer cookie
name `jmodguard_session` (the API session cookie). If the dashboard cookie
field is read-only (`BUNNYNET_MC_SESSIONID` on some trial UIs), one replica
is still fine. Do not give operators this endpoint hostname; it exists so the
Storage Pull Zone can origin-pull the API. GET/PATCH of a Magic Container via
the Bunny API returns every env var — never paste those responses.

The Bunny API image does not embed or serve the UI at `/`; a default
local API build (`embed-ui`) may serve `/`. Do not treat `/` on
the container or its endpoint hostname as a ready check. Platform and
Pull Zone liveness is `GET`/`HEAD /health` on port `8080`. Until axum
takes over (database open, API state primary-version prime, router
build), only `/health` returns `200`. `/api` returns `503` with JSON
`{"error":"not ready"}` and `Retry-After`; `/login` and `/oauth/callback`
return HTML `503` that refreshes the same URL (so an in-flight Discord
redirect is not replaced by a JSON error page). The admin page retries
`POST /api/me` while the origin is still starting (`not ready` for up to eight
minutes). Consecutive `502`/`504`, dropped origin pulls, or origin pulls
that never complete stop after 90 seconds of those failures with an error
instead of spinning forever. OAuth CSRF is a cookie, so a restart between
`/login` and Discord's return still completes. Login sessions live in
that one process (`jmodguard_session`).

```dotenv
JMODGUARD_WEB_BIND=0.0.0.0:8080
JMODGUARD_WEB_BASE_URL=https://YOUR-PUBLIC-ADMIN-HOST
JMODGUARD_OAUTH_CLIENT_ID=SECRET
JMODGUARD_OAUTH_CLIENT_SECRET=SECRET
JMODGUARD_OWNER_ID=123456789012345678
JMODGUARD_CONFIG_POLL_SECS=5
JMODGUARD_LOG_LEVEL=info
JMODGUARD_LOG_DIRECTORY=
```

With the shared-store/evidence environment above, no `config.toml` is required. Optional:
`JMODGUARD_DB_POOL_SIZE` (this process's pool; the bot has its own). Prefix
and bot status are bot-owned — set those on the bot app.

`JMODGUARD_WEB_BASE_URL` is the **public admin host** operators type into the
browser: the Pull Zone `*.b-cdn.net` hostname, or the owned
domain/subdomain if you added one. It is not a Magic Container hostname
and not a leftover `*.b-cdn.net` system host after an owned-host cutover.
Discord OAuth redirect URI (same host):

```text
https://YOUR-PUBLIC-ADMIN-HOST/oauth/callback
```

Leave `JMODGUARD_CORS_ORIGINS` unset. Same-origin via the Pull Zone does
not need CORS. The API uses OAuth for login and guild permissions; only the bot
receives `JMODGUARD_DISCORD_TOKEN`. WebUI unban/log-channel changes queue actions
for the bot and return receipts. Deploy matching bot/API/frontend builds and reload
old pages. See [action settings and status](BOT-ACTIONS.md); set the same `[actions]`
storage policy in both processes, and worker controls only on the bot.

Both containers need **full-access** database tokens. A read-only Bunny
token will fail API startup (the API probes a no-op write after schema
open).

The API never applies migrations. It waits for the bot bootstrap marker
(about six minutes on a remote database), then exits if that wait
expires — start the bot first and wait for `database ready (migrate=true)`.

## 6. Point API paths on the Pull Zone at the API container

On the Pull Zone from step 2 (Storage origin for the files):

1. Add an edge rule whose action is **Override Origin URL** (`OriginUrl`,
   ActionType 2). The origin host is the API **endpoint Pull Zone**
   `*.b-cdn.net` hostname from step 5 (Bunny creates that PZ when you add
   the CDN endpoint). Do **not** give operators that hostname. Do **not**
   use **Override Origin: Magic Containers** (`OriginMagicContainers`,
   ActionType 22) on a Storage-origin Pull Zone: that rule can match and
   still serve Bunny’s “We’re deploying your app” splash while the API is
   already live.
2. On that same rule, add ExtraActions **Bypass Perma-Cache**
   (`BypassPermaCache`) **and Override Cache Time = 0**
   (`OverrideCacheTime`, ActionParameter1 `0`). Bypass Perma-Cache does
   **not** skip Bunny’s ordinary edge cache. A cached `GET /api/me` is
   served to browsers that never sent a session cookie and shows a
   signed-in workspace whose later `PUT`s fail with `not authenticated`.
   The API also sends `Cache-Control: private, no-store`,
   `CDN-Cache-Control: no-store`, and `Vary: Cookie`. After adding the
   cache-time override, **purge the Pull Zone** so an already-cached
   `/api/me` cannot keep leaking. The SPA bootstraps with `POST /api/me`
   so leftover GET cache entries are not used for login. The SPA also loads
   guild settings with `POST /api/guilds/{id}/config`. Log-channel receipt
   checks and the Bot actions table use `POST /api/guilds/{id}/actions`. A
   failed log-channel receipt reads `POST /api/guilds/{id}/logchannels/{event}`.
   Dashboard counts, the audit log, bans, case lists, a case, and its
   evidence list use POST as well. Case history stays GET because POST
   on that path adds a note.
3. Trigger on request URL for `/api*`, `/login*`, `/logout*`, `/oauth*`,
   and `/health*`. Bunny allows **at most five** PatternMatches per
   trigger. If operators might open both an owned hostname and the system
   `*.b-cdn.net` host (before cutover), that is **two** triggers (five
   path patterns each), MatchAny. After an owned-host cutover, keep the
   owned-host trigger; do not send login to the leftover `*.b-cdn.net`
   host.
4. On this Pull Zone, Caching → General, leave **Strip Response Cookies**
   off (`DisableCookies` must be **false**) so login cookies are not
   stripped. If you update the zone via the Bunny API, send only the
   fields you are changing — posting the full GET body back fails
   (`model.invalid` on `id`).
5. Everything else stays on Storage (`/`, `/app.js`, `/style.css`). daisyUI and
   Headless UI are fetched by the browser from the configured CDN, not Storage.

After this, a browser on the **public admin host** (`JMODGUARD_WEB_BASE_URL`:
the Pull Zone `*.b-cdn.net` hostname, or the owned domain/subdomain if you
added one) loads the admin UI from Storage and calls `/api/...` on the same
host; Bunny forwards those requests to the API container. The Magic Container
endpoint hostname is not the product URL.

## Discord application (required for Bunny)

Do this in the [Discord developer portal](https://discord.com/developers/applications)
in addition to the container env vars. SETUP.md has the same list for
self-host.

1. **Bot → Privileged Gateway Intents:** enable **Server Members** and
   **Message Content**. Presence is unused.
2. **OAuth2 → Redirects:** add exactly
   `https://YOUR-PUBLIC-ADMIN-HOST/oauth/callback` (must match
   `JMODGUARD_WEB_BASE_URL`). That host is either the Pull Zone
   `*.b-cdn.net` hostname or the owned domain/subdomain on that zone, not
   a Magic Container hostname. After an owned-host cutover, do not also
   keep a leftover `*.b-cdn.net` redirect as the login URL.
3. **Invite** with scopes `bot` and `applications.commands`. Tick bot
   permissions in this Discord URL Generator order (same list as
   [SETUP.md](SETUP.md#3-invite-the-bot-to-your-server)):
   - **General:** View Audit Log (ban/unban moderator and reason details), Kick Members, Ban Members, Manage Nicknames, Manage Roles (only if you use role plan steps), Moderate Members
   - **Text:** View Channels, Send Messages, Manage Messages, Read Message History

## 7. Acceptance test

1. Bot logs: `database ready (migrate=true)`, then shard startup. No
   public hostname.
2. API logs: `database ready (migrate=false)` and listening. No public
   website hostname on the container.
3. Each app: one region, one replica.
4. Open the **public admin host** that matches `JMODGUARD_WEB_BASE_URL`
   (the Pull Zone `*.b-cdn.net` hostname, or the owned domain/subdomain
   if you added one — not a leftover `*.b-cdn.net` host after owned-host
   cutover). The admin UI loads from Storage. Login with Discord. OAuth
   returns to that origin. Probe `/health` (200 `ok`); do not use `/` on
   the Bunny API origin as a ready check.
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
- One bot replica. One API replica (in-memory sessions). Sticky sessions
  are enabled on the API Magic Container endpoint **after** it exists, not
  on the add-endpoint screen. Prefer cookie `jmodguard_session` when the
  dashboard allows it; a locked `BUNNYNET_MC_SESSIONID` is acceptable at
  one replica.
- On the Storage Pull Zone, origin-pull the API with **Override Origin URL**
  (`OriginUrl`), not OriginMagicContainers. Strip Response Cookies must
  be off. Five PatternMatches per trigger maximum.
- Never paste Magic Container GET/PATCH JSON (it includes every secret).
- SIGTERM stops the Discord gateway and the API listener cleanly, including
  during remote database retry (startup then exits 0 instead of failing).
- Missing Bunny DB secrets must not fall through to local SQLite
  (`JMODGUARD_REQUIRE_REMOTE_DB=1`).
- Pin bot and API image **digests** in your container runbook and point
  Magic Containers at those digests, not floating `:latest`. Storage
  `frontend/` can move independently for CSS/layout; do not mix a new
  website with an old API when the page needs new endpoints. A later
  `images` publish retags `:latest` but retains old versions. Review any manual
  pruning against deployed and rollback image inventories; a recorded digest
  alone is not a retained artifact.
- Do not add public Magic Container endpoints for the bot. Do not use a
  Magic Container hostname as the product URL.

## References

- [Public installation](INSTALL.md)
- [AWS EC2 path](DEPLOY-AWS.md)
- [Cost formulas](COST.md)
- [Bunny Database overview](https://docs.bunny.net/database)
- [Bunny Storage static sites](https://docs.bunny.net/storage/static-site-hosting)
- [Magic Containers endpoints](https://docs.bunny.net/magic-containers/endpoints)
- [Magic Containers health checks](https://docs.bunny.net/magic-containers/health-checks)
- [Backup and restore status](BACKUP-RESTORE.md)
- [Configuration reference](CONFIGURATION.md)
- [Architecture](ARCHITECTURE.md)
