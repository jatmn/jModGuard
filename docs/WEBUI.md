# WebUI

The WebUI is a single-page app served by **`jmodguard-api`** (not the Discord
bot process). It manages everything the bot does — no config file editing
needed beyond the token / OAuth credentials and `bot.owner_id`.

Start `jmodguard-bot` once first so the database is migrated, then start
`jmodguard-api`. Both use the same local SQLite file or remote libSQL database.

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
  inactivity. Sessions use an **in-memory store on the single API process**:
  re-login after an API restart. A shared session store is required only if
  you run multiple API replicas.
- The login state parameter is checked against the session to prevent CSRF
  (empty or mismatched state is rejected).
- Session cookies use the `Secure` flag when `web.base_url` starts with
  `https://`. Expose publicly only behind HTTPS (see SETUP.md §8).
