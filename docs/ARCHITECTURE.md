# Architecture

jModGuard runs as **two processes** that share one database:

| Process | Role |
|---------|------|
| `jmodguard-bot` | Discord gateway, automod, commands, migrations (`migrate=true`) |
| `jmodguard-api` | OAuth WebUI + JSON API (`migrate=false`; requires feature `web`) |

Neither process calls the other. The **database is the source of truth**.

```
jmodguard-bot  ──writes──►  SQLite / libSQL  ◄──writes──  jmodguard-api
       │                           │                           │
       └──── polls config_version ─┴── both processes poll ────┘
```

For a single-host install, both processes open the same local SQLite file. For
distributed deployment, both connect to one remote libSQL database. Bunny
Database is the documented first-class remote provider; its environment
mapping and generic protocol path have automated coverage, while production
TLS/authentication is verified during deployment acceptance. Local SQLite
uses WAL mode, so its file must not be placed on a network filesystem or opened
by processes on different hosts.

## Feature split

- Feature `web` (default): axum, tower-sessions, embedded SPA from `frontend/`.
- Build the **bot** with `--no-default-features` so it does not link or embed the WebUI.
- The **API** binary requires `web` (`required-features = ["web"]`).

## Config cache invalidation

Config writers bump a durable `meta.config_version` row **inside the DB layer**
(same transaction as the write). Runtime writers (messages, members, events,
warns, bans, users) do **not** bump it.

Both processes poll `bot.config_poll_secs` (default 5) and invalidate their
in-memory guild config cache when the version changes. Command registry writers
also bump `meta.slash_registry_version`; the bot polls that independently each
tick and re-syncs Discord slash commands only when it advances (not on every
settings/rules/template write). `AppState::new` primes both baselines from the
DB so the first poll has no unobserved window.

Expect up to one poll interval of lag locally after a cross-process config
write; remote deployments can additionally wait for provider replica convergence.
Slash-command resync reads its registry version and command rows in one
transaction and retries when that snapshot has not reached the target version.
If a lower counter is observed, jModGuard confirms it through an `IMMEDIATE`
transaction routed to the primary before rebaselining caches. This recovers
from Bunny's documented failover-loss window without accepting a stale replica
as authoritative. Mutating read-modify-write paths also use `IMMEDIATE`
transactions so they begin on the primary and serialize with other writers.

Nickname changes commit a generation-numbered durable outbox row in the same
settings transaction. The bot is the sole Discord nickname writer: it drains
due work through primary-routed reads and acknowledges only the generation it
applied. Transient Discord failures use persisted exponential backoff with
jitter and a bounded retry budget. Permanent rejections become terminal,
operator-visible rows; guild departure makes work dormant until `guild_create`
reactivates it. Reconnects leave terminal failures untouched; only a settings
change or explicit operator repair supersedes them. This makes the final
committed nickname win across bot/API races and survives cancelled requests
without rolling back durable settings.

## Migrations

- Bot applies migrations, atomically seeds globals plus their config/command
  versions, heals settings, then publishes a
  bootstrap-complete marker.
- API opens with `migrate=false`. Until the schema, terminal-defaults meta, and
  bootstrap-complete marker exist—or while SQLite is transiently locked by bot
  startup—it retries for about 30 seconds. Other open failures (remote URL,
  I/O) fail immediately.
- Always start (or migrate with) the bot before the API on a new or upgraded DB.

## WebUI auth (summary)

- Discord OAuth2; sessions in `MemoryStore` (single API process; lost on API restart).
- Guild admins need Discord `MANAGE_GUILD`.
- Global edits (`guild_id=0` or body `scope: "global"`) require `bot.owner_id`.
- Guild-scoped authorization always re-fetches Discord guild membership
  (`authorize_manage_guild`); the short guild-list cache is display-only (`/api/me`).

## Source layout

```
src/lib.rs                 shared library
src/bin/jmodguard-bot.rs
src/bin/jmodguard-api.rs
frontend/                  SPA (served by API locally; CDN-ready)
migrations/                0001_init through 0005_mutation_receipts_nickname_jobs
Dockerfile.bot
Dockerfile.api
```

## Related docs

- [SETUP.md](SETUP.md) — first install
- [UPGRADE.md](UPGRADE.md) — moving to a new version
- [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md) — optional Bunny.net deploy shape
- [CONFIGURATION.md](CONFIGURATION.md) — config.toml and backups
