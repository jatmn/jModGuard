# WebUI

The WebUI is a single-page admin app. On a laptop it is served by
**`jmodguard-api`** (feature `embed-ui`). On Bunny it is files in `frontend/`
on Storage/CDN; the API container stays private and only answers `/api`,
login, and OAuth. The pages never open SQL from the browser. They call the
API; the API (and the bot) open the database. If the CDN page loads while
the API is still starting, `POST /api/me` (and `GET /api/me`) returns `503` `{error:"not ready"}`
and the UI keeps waiting (up to eight minutes). Consecutive `502`/`504`,
failed fetches, or origin pulls that never complete stop after 90 seconds
of those failures so a bad origin rule is not an infinite spinner. After login,
the SPA loads guild configuration with `POST /api/guilds/{id}/config` (GET remains
for compatibility) so a CDN cannot reuse a cached settings document. `/login` and
`/oauth/callback` return an HTML wait page that reloads until axum can
redirect. OAuth CSRF lives in a cookie, so an API restart between Login
and Discord's return still completes the handshake.

Start `jmodguard-bot` once first so the database is migrated, then start
`jmodguard-api`. Both use the same local SQLite file or remote libSQL database.

## Getting and hosting the files

The website is three files in this repository, in the `frontend/` folder:

| File | What it is |
|---|---|
| `frontend/index.html` | the page |
| `frontend/app.js` | the app |
| `frontend/style.css` | the look |

Use the **same git tag or commit** as the bot and API when the page needs
new endpoints. CSS-only updates can ship without a new image.

**Self-host (one machine, [SETUP.md](SETUP.md)).** You do not copy these
files anywhere. A normal `jmodguard-api` build already includes them and
serves them at `http://localhost:8080`.

**Bunny (first-class cloud path, [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md)).**

1. Create a Bunny **Storage Zone**. The create form requires a primary
   region and at least one replication region; do not skip replication.
2. Put the three files in the **root** of that zone so they are `/index.html`
   (or `/`), `/app.js`, and `/style.css`.
3. Create a **Pull Zone** whose origin is that Storage Zone. The public
   admin site is either the Pull Zone `*.b-cdn.net` hostname or an owned domain/subdomain
   you add on that zone. Use that same host for `JMODGUARD_WEB_BASE_URL` and
   the Discord OAuth redirect.
4. Point `/api`, `/login`, `/logout`, `/oauth`, and `/health` on that Pull
   Zone at the API container with **Override Origin URL** (`OriginUrl`) to
   the API endpoint Pull Zone host (you must create a CDN endpoint on the
   API app first; sticky sessions are set on that endpoint after it
   exists, not on the add-endpoint screen; see DEPLOY-BUNNY.md). Do not
   use Override Origin: Magic Containers on a Storage Pull Zone. Leave
   `window.JMODGUARD_API_BASE` unset. Strip Response Cookies must be off
   (Caching → General; `DisableCookies=false`).

You can upload by hand in the Bunny dashboard (**Browse Files**) from this
public `frontend/` directory or the `frontend/` paths in a Nightly, Stable
tag, or manual release manifest. Buildkite does not upload a separate frontend
bundle on every main SHA. To repeat an upload from a checkout, run
[scripts/publish-frontend-bunny.sh](../scripts/publish-frontend-bunny.sh) with
`BUNNY_STORAGE_ZONE`, `BUNNY_STORAGE_PASSWORD`, `BUNNY_STORAGE_ENDPOINT`,
`BUNNY_PULL_ZONE_ID`, and `BUNNY_API_KEY` in the environment (never git); see
[DEPLOY-BUNNY.md](DEPLOY-BUNNY.md) for meanings. Optional `JMODGUARD_FRONTEND_ROOT`
 selects a non-default `frontend/` directory. A private ops repo is **not required** to install.

**Another host or CDN.** Copy the same three files to the site root
(match the API revision when the page needs new endpoints; CSS/layout-only
files can be newer). If the API is on a **different** hostname than the pages,
set `window.JMODGUARD_API_BASE` to the API origin before `app.js` loads,
and set `JMODGUARD_CORS_ORIGINS` on the API to the page origin. Discord
OAuth still happens on `web.base_url`: `/login` and `/oauth/callback`
must be that same hostname so the CSRF cookies Discord cannot set are
sent back on the redirect. Bunny’s designed shape does not need that
split: the Pull Zone is one hostname for both the files and `/api`.

Never put a database URL or token in these files.

## Access

`http://localhost:8080` → **Login with Discord** → pick a server. Only servers
where your account has **Manage Server** permission are shown, so one WebUI
instance manages all your servers.

## Workspace and saved changes

The sidebar keeps your server and all moderation sections within reach. On
small screens, navigation becomes a horizontally scrollable bar. Tables scroll
within the workspace, and editors support keyboard navigation.

The selected server and section are remembered in this browser for each signed-in
account and API origin. Restored server IDs are checked against your current
manageable-server list. If a server is no longer available, the first accessible
server is selected. Browser storage is optional; if blocked, the app still works,
but selection will not survive a reload. Settings themselves always come from the
API and are never restored from browser storage.

Use **Save** in an editor to apply changes. While an action is running, controls
are disabled to prevent duplicate saves or switching its target server. If a
configuration refresh fails, the workspace shows **Retry** instead of stale
settings. Server loads ignore out-of-order responses. Navigation warns before
discarding unsaved edits, including action/step additions and removals. Ordinary
settings, rule, badword, command and template saves apply the authoritative
representation returned by that mutation instead of downloading the complete
configuration bundle again. Multi-section copies and responses without a
representation still reload the authoritative snapshot. Other edited fields stay
marked **Unsaved changes**, including when a field-specific settings action runs or
a failed refresh needs Retry. Log-channel saves immediately show the accepted queued
value, clear the submitted row's dirty state, and reconcile the terminal bot-action
receipt automatically; the queued badge does not claim bot validation has completed.
Drafts remain in memory only and are discarded when
you confirm leaving the workspace.

A Global rule save writes the global rule before clearing the current server's
override. If that second request fails, the workspace refreshes the saved state
and reports the partial result; if the refresh also fails, use **Retry**. Closing
the editor does not undo an acknowledged global write.

## Tabs

| Tab | What it does |
|---|---|
| **Dashboard** | server configuration plus identified moderation records, warns and bans; unavailable counts are explicit |
| **Automod Rules** | per-rule editor: params JSON, action escalation table, whitelists, dry-run, scope, reset-to-global |
| **Badwords** | add/remove regex patterns (server; global if bot owner), live tester; invalid legacy patterns show **not enforced** |
| **Commands** | edit every command (name, aliases, description, permission, cooldown, roles/channels, response template); create custom commands with the **step editor**; delete custom commands |
| **Templates** | edit event templates with live preview; server or (owner) global scope |
| **Log Channels** | map each event type to a channel in that server (ID, `<#id>` mention, or Discord channel URL); queued for the bot |
| **Audit Log** | retained event history (type, user, actor), including older routine rows; new routine events report to Discord channels |
| **Bans** | ban history + unban via the bot token (API process) |
| **Bot actions** | latest queued unban/log-channel receipts; cancel unstarted work; force-cancel a stuck log-channel request so a new save can run (does not undo Discord) |
| **Settings** | prefix, bot nickname (persisted immediately, then asynchronously applied/cleared on Discord by the bot; transient failures use persisted bounded backoff and permanent failures are shown for operator action; empty / `!setname` with no args / `/setname` without `name` all clear); global prefix (owner only); **copy config from another server** |

Routine message/member history and chat-retention controls are removed. Saved Members
tabs reopen on Dashboard. Publish matching API/frontend versions; older pages submitting
retention settings must reload. See [logging/data behavior](LOGGING-DATA.md).

## Global vs per-server config

Rules, badwords, commands, and templates are stored either
at **server** scope or **global** scope:

- global values act as defaults for all servers;
- a server value overrides the global one;
- editors show which scope applies ("server override" / "global" badges).

**Log channels are always per-server.** Discord channel IDs belong to one
server, so they cannot be global defaults or copied between servers. The
Log Channels tab accepts a raw snowflake, `<#id>` mention, or channel URL
as a **string** (never a JavaScript `Number`). The same snowflake parsing
applies to role/user/channel ID fields on Rules, Commands, and Settings.

**Global writes are owner-only.** The API requires the signed-in Discord user
to match `bot.owner_id` (non-zero) for:

- path `guild_id=0` (e.g. global prefix via `PUT /api/guilds/0/settings`);
- request body `"scope": "global"`;
- deleting a global badword row.

`/api/me` includes `is_bot_owner`, optional `conductor_url`, and `preview` when this
tab is a read-only permission preview. The SPA hides Global scope controls and the
global-prefix card when that flag is false. Guild admins still see global
badwords (read-only) because the bot enforces them. Badword list entries include
`valid` / `error` — patterns that fail compile limits are shown as **not enforced**.

The configured bot owner can open **View as…** for a guild they can manage. That new
tab sends `x-jmodguard-preview` and shows **Preview — changes disabled**. The original
owner tab keeps write access. The bearer is handed to the new tab in a URL fragment
(not a query parameter), remains there as a reload-safe tab marker until explicit exit,
and expires after one hour. Administrator, Moderator, and
Member apply distinct synthetic permission masks to permission-dependent responses;
the owner's live Manage Server permission is still rechecked before every guild read.
Preview never makes `is_bot_owner` true. An
Administration link appears only when `web.conductor_url` / `JMODGUARD_CONDUCTOR_URL`
is set; that origin uses a separate conductor login. When the conductor has a
backup root configured, that Administration UI can create, download, upload and
restore local archives. Guild API sessions cannot authenticate it.

The **Settings → Copy configuration from another server** action copies any
subset (everything / rules / badwords / commands / templates / settings) from
one of your other servers into the current one — one-click carry-over between
servers. Log channels are intentionally excluded because their Discord channel
IDs are server-specific. You must have Manage Server on the source guild.

Badwords copy is **validate-then-replace**: the destination is replaced with
the enforceable subset of source patterns. Invalid source patterns are counted
as `skipped_badwords`. If the source has patterns but **none** are enforceable,
the copy is **refused** (HTTP error) and the destination is left unchanged —
including when copying “everything” (the whole transaction rolls back).
Empty source clears destination badwords (intentional empty replace).

## API

The UI talks to a JSON API under `/api/...`; every guild endpoint checks the
session can manage that guild (or bot ownership for global scope):

```
GET    /api/me                         # includes is_bot_owner, conductor_url, preview
POST   /api/me                         # same body as GET; SPA login bootstrap (not CDN-cached)
POST   /api/preview                    # owner-only; returns a tab-scoped preview token
DELETE /api/preview                    # exit preview; owner session is unchanged
GET    /api/guilds/{id}/config
POST   /api/guilds/{id}/config         # same body as GET; SPA workspace load (not CDN-cached)
PUT    /api/guilds/{id}/settings
GET    /api/guilds/{id}/rules/{rule}          PUT/DELETE …
GET    /api/guilds/{id}/badwords              POST (add) / DELETE /{word_id}
GET    /api/guilds/{id}/commands              POST (create) / PUT|DELETE /{name}
GET    /api/guilds/{id}/templates/{event}     PUT/DELETE …
GET    /api/guilds/{id}/logchannels/{event}   PUT/DELETE …
POST   /api/guilds/{id}/copy
POST   /api/guilds/{id}/testregex      # pattern/sample length + compile limits
GET    /api/guilds/{id}/events?event=&user=&limit=
GET    /api/guilds/{id}/bans
POST   /api/guilds/{id}/unban
GET    /api/guilds/{id}/stats          # guild-scoped aggregates only
GET    /api/guilds/{id}/case-delete-roles
POST   /api/guilds/{id}/case-delete-roles  # same body as GET; SPA settings load
PUT    /api/guilds/{id}/case-delete-roles
```

Scopes in request bodies: `"scope": "guild" | "global"` (`global` requires
bot owner).

## Auth & sessions

- Discord OAuth2 (scopes `identify guilds`), sessions last 7 days of
  inactivity. Sessions use an **in-memory store on the single API process**
  (`jmodguard_session` cookie): re-login after an API restart. A shared
  session store is required only if you run multiple API replicas.
- OAuth CSRF is a host-only cookie compared to Discord's echoed `state`
  (empty or mismatched state is rejected). Successful login rotates the
  session id.
- Session cookies use the `Secure` flag when `web.base_url` starts with
  `https://`. API JSON sets `Cache-Control: private, no-store`,
  `CDN-Cache-Control: no-store`, and `Vary: Cookie`. The page loads the
  signed-in workspace with `POST /api/me` so a CDN cannot reuse a cached
  `GET /api/me` identity after the in-memory session is gone. Guild settings
  load with `POST /api/guilds/{id}/config` for the same reason. Log-channel
  receipt checks and the Bot actions table use `POST /api/guilds/{id}/actions`.
  A failed log-channel receipt reads
  `POST /api/guilds/{id}/logchannels/{event}`. Expose
  publicly only behind HTTPS (see SETUP.md §8). On Bunny, API-path edge
  rules must Override Cache Time = 0 in addition to Bypass Perma-Cache;
  see [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md).

### Appearance

Choose Light, Dark, or System from the Appearance selector on the sign-in screen or sidebar. Light uses a pale blue workspace; Dark pairs it with navy surfaces and soft blue accents. The browser remembers this preference across reloads and sign-ins. System follows your device appearance, including changes while the page is open. If browser storage is unavailable, the choice still applies for the current page.
