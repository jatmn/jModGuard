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

The bot and the API share the database. Only the bot opens it for writing.
The API reads, and sends settings and other changes to the bot on a private
link that is not published to the internet. The bot’s extra outbound is the
Discord gateway. The API’s extra outbound is Discord OAuth, guild listing,
and unban.

Local `cargo run` still embeds `frontend/` in the API for SETUP.md. That
is a laptop / self-host convenience. It is not the Bunny deploy.

## 1. Create the core and guild databases

Create a Bunny Database. In **Access**, generate full-access credentials
for each container. Only the bot credential can write: it writes moderation
state and migrations, and it applies configuration changes the API sends it.
The API credential is read-only.

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
the matching `jmodguard-guild-1` identity; runtime upgrades it to version 15.

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
and upload those five files from the source commit selected for the deployed
API image to the **root** so they are `/` (or `/index.html`),
`/app.js`, `/style.css`, `/administration/index.html`, and
`/administration.js`. A trusted `main` image published under its full source
SHA is sufficient; no Stable or Nightly release tag is required. After that,
CSS/layout-only revisions can
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
`/login`, `/logout`, and `/oauth/callback` to the API container (step 5),
so the UI and API are same-origin. Do not put a database URL or token in
these files.

## 3. Build and publish both images

Buildkite's self-hosted `linux` queue runs the checks selected for that
diff and publishes the bot/API images after those checks pass. Docs-only
and other non-image diffs do not run the full Rust suite. Scheduled, tag,
and manual publication builds still run every check. This repository has no GitHub Actions CI or
image jobs or runner fallback. The Socket Security manifest scan is not a CI or image job. Set the repository
Actions secret `SOCKET_SECURITY_API_KEY` to a Socket API key before relying on
the scan. A missing secret skips the scan and does not fail the job, and the key is not stored in git.
The scan uploads discovered Cargo and npm manifests outside ignored paths to Socket, including workspace and member `Cargo.toml` files, `Cargo.lock`, and `tools/eslint/package.json` and its lockfile. It may comment on a pull request with the GitHub Actions token.
A new blocking alert can fail this check only when Socket compares a pull request change that touches a supported manifest. Push and full-report scans do not fail for alerts. Socket API errors do not fail this check. Buildkite remains the merge gate.
After enabling the key, let a scan run on `main` before relying on pull request comparisons; without a main baseline, Socket may treat existing dependencies as newly added.
Dependabot version updates use GitHub-hosted
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

Use `JMODGUARD_LOG_LEVEL=info` for production (the default). It overrides
`log.level` in `config.toml` and controls all tracing targets; `RUST_LOG`
is ignored. At `info`, per-message and per-guild debug diagnostics are suppressed.
Enable `debug` only briefly for troubleshooting: debug logs include message,
guild and channel IDs, sizes and attachment counts. Although the per-message
line omits the author ID and message text, this metadata and other diagnostics
can still reveal per-user activity. Restrict access to collected logs.

## 4. Create the app (bot and API containers, one pod)

Create one Magic Container app with the bot and API containers in the same
pod over localhost. **Single region**, exactly one instance (sessions are
in-memory). If the dashboard refuses min/max 1 (some trial accounts cap
Deploy UI at a higher minimum), create the app with `POST /mc/apps` instead.
One copy, because only the copy whose bot holds the writer lease applies
changes.

The bot container has no endpoint. Do **not** add a CDN or Anycast endpoint
for it. The bot is not a website. Its platform liveness check is HTTP GET
`/health` on port `8081`. Move that probe from 8080 to 8081 during the
single-app rollout. The control port is never exposed as an endpoint.

Only the API container's port `8080` gets a CDN endpoint. Do **not** attach
a custom domain to this app. The Pull Zone from step 2 is the public URL.

**Warning:** The bot's `/ready` endpoint must never be configured as a Bunny
or other platform readiness check. `/ready` returns 200 only after the bot
holds the active-bot lease and is connected to Discord. During a rolling
update the old bot still holds the lease until it is stopped, so a readiness
check on `/ready` stalls the new bot until Bunny's timeout and then rolls the
update back. In this one-app layout, keep platform checks to liveness
`/health` on both containers (API port 8080, bot port 8081). The API's
`/ready` stays 503 until the bot has upgraded the schema, and the new bot
cannot take the writer lease while the old copy still runs, so a readiness
check on the API's `/ready` can stall a rolling update the same way.

Listeners: API web and health on port 8080 (`JMODGUARD_WEB_BIND`); bot
health on port 8081 (`JMODGUARD_HEALTH_BIND`); bot control on loopback
`127.0.0.1:8082` (`JMODGUARD_CONTROL_BIND`). The control bind is
loopback-only. Wildcard binds are refused. It is never a public or CDN
endpoint.

Set `JMODGUARD_CONTROL_SECRET` on both containers to the same value of at
least 32 bytes for the control link. It is independent of both database
tokens. Without that secret the bot still moderates, and API writes return
`control link unavailable`. The API database token can be read-only. The bot
database token can write.

The bot runs maintenance recovery only after it holds the writer lease.
The bot launcher writes update-control under its own short fence, including
while the bot process is stopped. Schema and base changes ship in new images.
Self-update stays off by default and is owner-triggered only. Signing is
tracked in issue 188.

### Bot container

```dotenv
JMODGUARD_DISCORD_TOKEN=SECRET
JMODGUARD_OWNER_ID=123456789012345678
JMODGUARD_CONFIG_POLL_SECS=5
JMODGUARD_LOG_LEVEL=info
JMODGUARD_LOG_DIRECTORY=
JMODGUARD_HEALTH_BIND=0.0.0.0:8081
JMODGUARD_CONTROL_BIND=127.0.0.1:8082
JMODGUARD_CONTROL_SECRET=SECRET
```

The bot control listener is `JMODGUARD_CONTROL_BIND` (default
`127.0.0.1:8082`). A refused bind disables the control link; the bot keeps
its lease and moderates. Bot health is `JMODGUARD_HEALTH_BIND` (port 8081).

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

### API container

Bunny can only origin-pull a Magic Container through an **Endpoint**. On
the API container, **Endpoints → Add CDN endpoint**, container port `8080`.
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
Pull Zone liveness is `GET`/`HEAD /health` on port `8080`. `/ready`
returns `503` until startup finishes, then `200`. That check does not
query the database. Until axum takes over (database
open, API state primary-version prime, router build), `/health` returns
`200` and `/ready` stays `503`.
`/api` returns `503` with JSON
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
JMODGUARD_CONTROL_TARGET=127.0.0.1:<control-port>
JMODGUARD_CONTROL_SECRET=SECRET
```

Set `JMODGUARD_CONTROL_SECRET` to the same value as the bot, at least 32
bytes. It is independent of both database tokens. The API dials
`JMODGUARD_CONTROL_TARGET`. That setting has no default. Use
`127.0.0.1:<control-port>` for the one-pod localhost path. `<control-port>`
is the bot's control port, 8082 by default. Unset or empty, the API still
serves reads and writes return `control link unavailable`. Do not put the
control listener behind a public or CDN endpoint.

The API listens on port 8080, including `/health` and `/ready`. It does not
migrate. An older schema keeps `/health` at 200 and `/ready` at 503 until
the bot upgrades the schema. A newer incompatible schema exits.

With the shared-store/evidence environment above, no `config.toml` is required. Optional:
`JMODGUARD_DB_POOL_SIZE` (this process's pool; the bot has its own). Prefix
and bot status are bot-owned — set those on the bot container.

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

The bot database token can write. The API database token can be read-only.
A read-only token on the bot will fail startup, because only the bot applies
migrations and dashboard writes.

The API never applies migrations. It keeps retrying with a capped delay.
`/health` stays 200. `/ready` and app routes stay 503 until it is ready.
It exits on a newer unsupported schema, rejected remote credentials and
other non-retryable open errors, and bad config such as an empty OAuth
client id or secret. Start the bot first and wait for `database ready
(migrate=true)`.

## 5. Point API paths on the Pull Zone at the API container

On the Pull Zone from step 2 (Storage origin for the files):

1. Add an edge rule whose action is **Override Origin URL** (`OriginUrl`,
   ActionType 2). The origin host is the API **endpoint Pull Zone**
   `*.b-cdn.net` hostname from step 4 (Bunny creates that PZ when you add
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

## 6. Acceptance test

1. Bot logs: `database ready (migrate=true)`, then shard startup. No
   public hostname.
2. API logs: `database ready (migrate=false)` and listening. No public
   website hostname on the container.
3. The app: one region, one replica.
4. Open the **public admin host** that matches `JMODGUARD_WEB_BASE_URL`
   (the Pull Zone `*.b-cdn.net` hostname, or the owned domain/subdomain
   if you added one — not a leftover `*.b-cdn.net` host after owned-host
   cutover). The admin UI loads from Storage. Login with Discord. OAuth
   returns to that origin. Probe `/health` (200 `ok`) for liveness.
   Do not use `/` on the API origin as a ready check.
5. Change a harmless guild prefix. Confirm the bot picks it up, then
   restore it.
6. Configure an `automod` log channel, enable a dry-run rule and check its Discord
   report. Dry-run observations no longer appear as new SQL Audit Log rows.
7. Restart the **API container** only: bot stays connected; users log in again.
8. Restart the **bot container** only: API stays up; bot returns online.

CI’s `tests/remote_database_smoke.rs` proves bot bootstrap and API
schema-open against a local libSQL server, not Bunny TLS.
`tests/dual_process_smoke.rs` covers the local SQLite process contract.

## Expired database streams

The bot and API reset the saved remote autocommit stream before each pooled
database operation. This prevents an idle connection from reusing a Bunny
Hrana stream that has expired. It needs no environment setting or manual SQL.
The reset does not retry the database operation or any Discord action.

If `bot action queue unavailable` continues after installing this correction,
retain the full error chain, including the `remote database ... failed`
operation name. An expired stream during an active transaction is distinct
from an idle pooled stream; this change does not extend Bunny transaction
lifetimes. The accompanying `while no runtime was available` warning can come
from libSQL's best-effort cleanup of a discarded stream and does not by itself
mean the database runtime stopped. Discord connectivity and `/health` alone
do not prove that the action queue is working.

## Operating constraints

- Bunny Database is Public Preview. Internal snapshots are not an
  operator backup. Verify a complete database and evidence restore drill before production.
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
- [Configuration reference](CONFIGURATION.md)
- [Architecture](ARCHITECTURE.md)
