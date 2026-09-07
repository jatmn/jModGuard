# Configuration

There are two layers: the static `config.toml` (tokens, bindings) and the
database (everything server-related, editable at runtime).

## config.toml

| Key | Default | Description |
|---|---|---|
| `discord.token` | — | Bot token (env: `JMODGUARD_DISCORD_TOKEN`). Required by bot; also by API for WebUI unban |
| `db.path` | `jmodguard.db` | Local SQLite file (env: `JMODGUARD_DB_PATH`) |
| `db.url` | — | Remote libSQL URL; empty = local path. Environment overrides are accepted only as a complete URL/token pair. |
| `db.token` | — | Remote DB access token; required with `db.url`. Complete `JMODGUARD_DB_URL`/`JMODGUARD_DB_TOKEN` takes precedence over the complete Bunny-injected pair. |
| `db.pool_size` | `8` | reusable connections and maximum concurrent database operations in each process (minimum 2) (env: `JMODGUARD_DB_POOL_SIZE`) |
| `web.bind` | `127.0.0.1:8080` | API/WebUI listen address (env: `JMODGUARD_WEB_BIND`) |
| `web.base_url` | `http://localhost:8080` | public URL used for OAuth redirect (env: `JMODGUARD_WEB_BASE_URL`) |
| `web.oauth_client_id` | — | Discord OAuth client ID (env: `JMODGUARD_OAUTH_CLIENT_ID`) |
| `web.oauth_client_secret` | — | Discord OAuth client secret (env: `JMODGUARD_OAUTH_CLIENT_SECRET`) |
| `web.cors_origins` | `[]` | Extra SPA origins only for a split-origin UI (env: `JMODGUARD_CORS_ORIGINS`). Empty = same-origin. Bunny CDN proxies `/api` onto the UI hostname, so this stays empty. |
| `bot.default_prefix` | `!` | seeded into the global settings row on first boot; synced to durable `meta.settings_terminal_defaults` on bot migrate-open (final fallback when global/guild omit the field) (env: `JMODGUARD_DEFAULT_PREFIX`) |
| `bot.owner_id` | `0` | owner user ID (owner commands + global WebUI config) (env: `JMODGUARD_OWNER_ID`) |
| `bot.status` | `moderating` | presence text (env: `JMODGUARD_BOT_STATUS`) |
| `bot.config_poll_secs` | `5` | how often each process polls durable `config_version` (env: `JMODGUARD_CONFIG_POLL_SECS`) |
| `log.level` | `info` | tracing level (env: `JMODGUARD_LOG_LEVEL`) |
| `log.directory` | `logs` | rolling log directory (env: `JMODGUARD_LOG_DIRECTORY`; empty = console only). Bot writes `jmodguard-bot.log`; API writes `jmodguard-api.log`. Container images default this env var to empty for platform logging |

Environment-only (not in `config.toml`):

| Variable | Default | Description |
|---|---|---|
| `JMODGUARD_REQUIRE_REMOTE_DB` | unset | If `1`/`true`, startup fails unless both a remote URL and token are configured. Container images set this so a missed Bunny secret cannot open a private SQLite file. |
| `JMODGUARD_HEALTH_BIND` | unset | Bot-only HTTP liveness listener (`GET`/`HEAD /health` returns `ok`). Bound before database open. Container bot image sets `0.0.0.0:8080`; bind failure exits the process. Empty = disabled. The API serves the same path on `web.bind` until axum takes over; other paths on that bind return `503` until then. |

## Database (runtime config)

All of this is managed via the WebUI or bot commands; the schema lives in
`migrations/` (`0001_init.sql` through `0005_mutation_receipts_nickname_jobs.sql`) and is applied by
`jmodguard-bot` on startup (API opens with `migrate=false` and waits ~30s
locally, or about six minutes on a remote database, for the bot to finish
bootstrap or for a transient connect/lock to clear, then serves HTTP; it
exits if that wait expires).

| Table | Holds |
|---|---|
| `meta` | durable counters plus non-reusable revision tokens (`config_version`/`config_revision` for cache invalidation; `slash_registry_version`/`slash_registry_revision` for Discord slash re-sync) |
| `nickname_reconcile` | generation-numbered nickname jobs with pending/dormant/terminal state, persisted retry schedule, attempt count, and last operator-visible error |
| `mutation_receipts` | short-lived commit-outcome receipts that establish remote mutation results after an ambiguous COMMIT response |
| `guild_settings` | prefix and bot nickname as **partial overrides** (row per server; `guild_id=0` = global). Missing fields inherit from the next layer. |
| `rules` | the 7 automod rules (server + global rows) |
| `commands` | every command — name, aliases, behavior, response, permissions (server + global) |
| `templates` | log event templates (server + global) |
| `log_channels` | event type → channel mapping (per server only) |
| `legacy_global_log_channels` | global mappings retired by migration 0003; inspect them while recreating per-server mappings |
| `badwords` | regex patterns (server + global) |
| `members` | retired schema scaffolding; no live collection or WebUI member history |
| `messages` | retired schema scaffolding; no live collection |
| `events` | transitional retained moderation records; routine channel reports do not append here |
| `warns` / `bans` | warnings and ban history |
| `users` | retired schema scaffolding; no live collection |

Environment variables override `config.toml` values (see table).

Remote database calls and waits for a pooled connection have a fixed 75-second
per-wire-attempt deadline. Transaction rollback cleanup and an authoritative
receipt check after a failed COMMIT can each use an additional deadline before
the logical operation returns. A connection whose operation timed out is
discarded rather than reused with potentially stale remote stream state. Each
normal post-bootstrap application mutation carries a receipt in its transaction;
idempotent migrations and bootstrap fencing are reverified on startup instead. If a COMMIT
response is lost, jModGuard checks that receipt in a fresh primary-routed
transaction and returns the established result rather than inviting a duplicate
retry. If that authoritative check also fails, the operation remains explicitly
indeterminate and the process conservatively advances its local configuration
floor so it cannot immediately serve an older snapshot.

## Remote providers

The remote transport uses the standard libSQL protocol; the portable
configuration contract is `db.url` / `db.token` or `JMODGUARD_DB_URL` /
`JMODGUARD_DB_TOKEN`. Environment credentials are selected as an atomic pair:
if either portable variable is non-empty, both must be non-empty; otherwise the
same rule applies to Bunny's injected pair. Blank or whitespace-only values are
treated as unset. Partial or mixed-provider overrides fail startup instead of
pairing a URL with an unrelated token. It is not hard-coded to a Bunny hostname.

Bunny Database is the first-class launch provider: its injected environment
names and generic libSQL protocol path have automated tests, and its container
topology is documented. CI's disposable server does not reproduce Bunny's
production TLS/authentication service. Other libSQL-compatible services may
work through the portable settings, but they are not claimed as supported
until their authentication, consistency behavior, deployment, and recovery
workflow have provider-specific tests and documentation.

## Behavior on first boot

If the database is new, global defaults are seeded: 7 rules, ~20 commands,
and default templates. Servers inherit these until you override them.

## Backups

For a local deployment, the whole configuration lives in `jmodguard.db`,
shared by **both** `jmodguard-bot` and `jmodguard-api`. Either process can hold
WAL connections open.

**Cold copy (preferred for restores):** stop **both** the bot and the API,
then copy `jmodguard.db` plus `jmodguard.db-wal` / `jmodguard.db-shm` if
present, then start both again.

**Hot copy:** use SQLite's online backup API or
`sqlite3 jmodguard.db "VACUUM INTO 'jmodguard-backup.db'"` while the
processes keep running — do **not** file-copy the `.db` while either process
is still writing.

### Bunny Database

Bunny's internal snapshots are not an operator-controlled backup. The current
`bsql .dump` output also omits schema objects required by jModGuard, so it is
not a complete recovery path. See [BACKUP-RESTORE.md](BACKUP-RESTORE.md) for
the exact limitation and the restore drill required before production.

### Discord log delivery

`log.discord_concurrency` (default 8, environment `JMODGUARD_LOG_DISCORD_CONCURRENCY`)
and `log.discord_timeout_secs` (default 10, environment `JMODGUARD_LOG_DISCORD_TIMEOUT_SECS`)
are positive bot bootstrap controls. Admission never waits; the deadline includes config
lookup and delivery. Saturated/failed reports are dropped without a persistent queue or
replay. A cumulative counter produces sparse process warnings without raw report content.
Operator UI controls and per-guild fairness remain pending. These defaults are tunable,
not measured throughput budgets. See [logging/data status](LOGGING-DATA.md).

The internal settings/schema still contain the retired `retention_days` field during
this staged refactor. It has no live message-retention effect and is absent from API
responses; settings requests containing it are rejected. New tenant schema work will
remove the old helpers/tables. No existing deployment data is migrated or cleaned here.
