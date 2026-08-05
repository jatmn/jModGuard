# WebUI

The WebUI is a single-page app served by the bot itself. It manages
everything the bot does — no config file editing needed beyond the token/OAuth
credentials.

## Access

`http://localhost:8080` → **Login with Discord** → pick a server. Only servers
where your account has **Manage Server** permission are shown, so one WebUI
instance manages all your servers.

## Tabs

| Tab | What it does |
|---|---|
| **Dashboard** | server stats: prefix, rules enabled, dry-runs, commands, log channels |
| **Automod Rules** | per-rule editor: params JSON, action escalation table, whitelists, dry-run, global/server scope, reset-to-global |
| **Badwords** | add/remove regex patterns (server or global), live tester showing raw/normalized/compact matches |
| **Commands** | edit every command (name, aliases, description, permission, cooldown, roles/channels, response template); create custom commands with the **step editor**; delete custom commands |
| **Templates** | edit event templates with live preview; server or global scope |
| **Log Channels** | map each event type to a channel ID; server or global |
| **Members** | search members: join count, message count, first/last seen |
| **Audit Log** | filterable event history (type, user, actor) |
| **Bans** | ban history + unban via the bot |
| **Settings** | prefix, bot nickname, message retention, global prefix, and **copy config from another server** |

## Global vs per-server config

Every item (rule, badword, command, template, log channel) is stored either
at **server** scope or **global** scope:

- global values act as defaults for all servers;
- a server value overrides the global one;
- editors show which scope applies ("server override" / "global" badges).

The **Settings → Copy configuration from another server** action copies any
subset (everything / rules / badwords / commands / templates / log channels /
settings) from one of your other servers into the current one — one-click
carry-over between servers.

## API

The UI talks to a JSON API under `/api/...`; every guild endpoint checks the
session can manage that guild:

```
GET    /api/me
GET    /api/guilds/{id}/config
PUT    /api/guilds/{id}/settings
GET    /api/guilds/{id}/rules/{rule}          PUT/DELETE …
GET    /api/guilds/{id}/badwords              POST (add) / DELETE /{word_id}
GET    /api/guilds/{id}/commands              POST (create) / PUT|DELETE /{name}
GET    /api/guilds/{id}/templates/{event}     PUT/DELETE …
GET    /api/guilds/{id}/logchannels/{event}   PUT/DELETE …
POST   /api/guilds/{id}/copy
POST   /api/guilds/{id}/testregex
GET    /api/guilds/{id}/members?q=&limit=
GET    /api/guilds/{id}/events?event=&user=&limit=
GET    /api/guilds/{id}/bans
POST   /api/guilds/{id}/unban
GET    /api/guilds/{id}/stats
```

Scopes in request bodies: `"scope": "guild" | "global"`.

## Auth & sessions

- Discord OAuth2 (scopes `identify guilds`), sessions last 7 days of
  inactivity (in-memory store — re-login after a bot restart).
- The login state parameter is checked against the session to prevent CSRF.
- Expose publicly only behind HTTPS (see SETUP.md §8).
