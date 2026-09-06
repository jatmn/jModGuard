# WebUI

The WebUI is a single-page admin app. On a laptop it is served by
**`jmodguard-api`** (feature `embed-ui`). On Bunny it is files in `frontend/`
on Storage/CDN; the API container stays private and only answers `/api`,
login, and OAuth. The pages never open SQL from the browser. They call the
API; the API (and the bot) open the database. If the CDN page loads while
the API is still starting, `/api/me` returns `503` `{error:"not ready"}`
and the UI keeps waiting (up to eight minutes). Consecutive `502`/`504`,
failed fetches, or origin pulls that never complete stop after 90 seconds
of those failures so a bad origin rule is not an infinite spinner. `/login` and
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

1. Create a Bunny **Storage Zone**.
2. Put the three files in the **root** of that zone so they are `/index.html`
   (or `/`), `/app.js`, and `/style.css`.
3. Create a **Pull Zone** whose origin is that Storage Zone. That hostname
   (or a custom domain on it) is the public admin site.
4. Point `/api`, `/login`, `/logout`, `/oauth`, and `/health` on that Pull
   Zone at the API container with **Change Origin URL** (`OriginUrl`) to
   the API endpoint Pull Zone host (you must create a CDN endpoint on the
   API app first; see DEPLOY-BUNNY.md). Do not use Override Origin: Magic
   Containers on a Storage Pull Zone. Leave `window.JMODGUARD_API_BASE`
   unset. Disable Cookies must be off.

You can upload by hand in the Bunny dashboard (**Browse Files**), or
repeat it with GitHub Actions. The script
[scripts/publish-frontend-bunny.sh](../scripts/publish-frontend-bunny.sh)
and the example workflow
[docs/examples/private-ops/publish-frontend.yml](examples/private-ops/publish-frontend.yml)
are for a **private** ops repo so Bunny Storage passwords are not stored
in this public project. A private repo is not required to try it once;
follow the [private ops installation instructions](examples/private-ops/README.md)
to copy the reviewed publisher and Python helpers. Production updates select a
full source SHA and compatible API digest in a reviewed ops release. Source
pushes do not promote production. The example verifies changed JS/HTML against
the approved active API and application readiness, and records completion only
after CDN purge. CSS-only updates still need approval but not an API rollout.

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

## Tabs

| Tab | What it does |
|---|---|
| **Dashboard** | server stats: prefix, rules enabled, dry-runs, commands, log channels |
| **Automod Rules** | per-rule editor: params JSON, action escalation table, whitelists, dry-run, scope, reset-to-global |
| **Badwords** | add/remove regex patterns (server; global if bot owner), live tester; invalid legacy patterns show **not enforced** |
| **Commands** | edit every command (name, aliases, description, permission, cooldown, roles/channels, response template); create custom commands with the **step editor**; delete custom commands |
| **Templates** | edit event templates with live preview; server or (owner) global scope |
| **Log Channels** | map each event type to a channel ID for that server |
| **Members** | search members: join count, message count, first/last seen |
| **Audit Log** | filterable event history (type, user, actor) |
| **Bans** | ban history + unban via the bot token (API process) |
| **Settings** | prefix, bot nickname (persisted immediately, then asynchronously applied/cleared on Discord by the bot; transient failures use persisted bounded backoff and permanent failures are shown for operator action; empty / `!setname` with no args / `/setname` without `name` all clear), message retention (per-server purge); global prefix (owner only); **copy config from another server** |

## Global vs per-server config

Rules, badwords, commands, and templates are stored either
at **server** scope or **global** scope:

- global values act as defaults for all servers;
- a server value overrides the global one;
- editors show which scope applies ("server override" / "global" badges).

**Log channels are always per-server.** Discord channel IDs belong to one
server, so they cannot be global defaults or copied between servers.

**Global writes are owner-only.** The API requires the signed-in Discord user
to match `bot.owner_id` (non-zero) for:

- path `guild_id=0` (e.g. global prefix via `PUT /api/guilds/0/settings`);
- request body `"scope": "global"`;
- deleting a global badword row.

`/api/me` includes `is_bot_owner`. The SPA hides Global scope controls and the
global-prefix card when that flag is false. Guild admins still see global
badwords (read-only) because the bot enforces them. Badword list entries include
`valid` / `error` — patterns that fail compile limits are shown as **not enforced**.

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
GET    /api/me                         # includes is_bot_owner
GET    /api/guilds/{id}/config
PUT    /api/guilds/{id}/settings
GET    /api/guilds/{id}/rules/{rule}          PUT/DELETE …
GET    /api/guilds/{id}/badwords              POST (add) / DELETE /{word_id}
GET    /api/guilds/{id}/commands              POST (create) / PUT|DELETE /{name}
GET    /api/guilds/{id}/templates/{event}     PUT/DELETE …
GET    /api/guilds/{id}/logchannels/{event}   PUT/DELETE …
POST   /api/guilds/{id}/copy
POST   /api/guilds/{id}/testregex      # pattern/sample length + compile limits
GET    /api/guilds/{id}/members?q=&limit=
GET    /api/guilds/{id}/events?event=&user=&limit=
GET    /api/guilds/{id}/bans
POST   /api/guilds/{id}/unban
GET    /api/guilds/{id}/stats          # guild-scoped aggregates only
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
  `https://`. API JSON sets `Cache-Control: private, no-store`. Expose
  publicly only behind HTTPS (see SETUP.md §8).
