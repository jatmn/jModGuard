# Configuration

There are two layers: the static `config.toml` (tokens, bindings) and the
database (everything server-related, editable at runtime).

## config.toml

| Key | Default | Description |
|---|---|---|
| `discord.token` | — | Bot token (env: `JMODGUARD_DISCORD_TOKEN`). Required by bot; also by API for WebUI unban |
| `db.path` | `jmodguard.db` | Local SQLite file (env: `JMODGUARD_DB_PATH`) |
| `db.url` | — | Remote libSQL / Bunny Database URL. `JMODGUARD_DB_URL` takes precedence over Bunny's injected `BUNNY_DATABASE_URL`; empty = local path |
| `db.token` | — | Remote DB access token. `JMODGUARD_DB_TOKEN` takes precedence over Bunny's injected `BUNNY_DATABASE_AUTH_TOKEN`; required with `db.url` |
| `db.pool_size` | `8` | maximum concurrent database operations in each process |
| `web.bind` | `127.0.0.1:8080` | API/WebUI listen address (env: `JMODGUARD_WEB_BIND`) |
| `web.base_url` | `http://localhost:8080` | public URL used for OAuth redirect (env: `JMODGUARD_WEB_BASE_URL`) |
| `web.oauth_client_id` | — | Discord OAuth client ID (env: `JMODGUARD_OAUTH_CLIENT_ID`) |
| `web.oauth_client_secret` | — | Discord OAuth client secret (env: `JMODGUARD_OAUTH_CLIENT_SECRET`) |
| `web.cors_origins` | `[]` | Extra SPA origins for credentialed CORS (env: `JMODGUARD_CORS_ORIGINS`, comma-separated). Empty = same-origin only |
| `bot.default_prefix` | `!` | seeded into the global settings row on first boot; synced to durable `meta.settings_terminal_defaults` on bot migrate-open (final fallback when global/guild omit the field) |
| `bot.owner_id` | `0` | owner user ID (owner commands + global WebUI config) (env: `JMODGUARD_OWNER_ID`) |
| `bot.retention_days` | `7` | seeded into the global settings row on first boot; synced to the same meta terminal-defaults row (WebUI / config cache / purge share it; `0` = keep forever) |
| `bot.status` | `moderating` | presence text |
| `bot.config_poll_secs` | `5` | how often each process polls durable `config_version` (env: `JMODGUARD_CONFIG_POLL_SECS`) |
| `log.level` | `info` | tracing level (env: `JMODGUARD_LOG_LEVEL`) |
| `log.directory` | `logs` | rolling log directory (env: `JMODGUARD_LOG_DIRECTORY`; empty = console only). Bot writes `jmodguard-bot.log`; API writes `jmodguard-api.log`. Container images default this env var to empty for platform logging |

## Database (runtime config)

All of this is managed via the WebUI or bot commands; the schema lives in
`migrations/` (`0001_init.sql` through `0003_archive_global_log_channels.sql`) and is applied by
`jmodguard-bot` on startup (API opens with `migrate=false` and waits ~30s for
the bot to finish bootstrap or release a transient local SQLite startup lock,
then exits).

| Table | Holds |
|---|---|
| `meta` | durable keys (`config_version` for cross-process cache invalidation; `slash_registry_version` for Discord slash re-sync) |
| `guild_settings` | prefix, bot nickname, retention as **partial overrides** (row per server; `guild_id=0` = global). Missing fields inherit from the next layer. |
| `rules` | the 7 automod rules (server + global rows) |
| `commands` | every command — name, aliases, behavior, response, permissions (server + global) |
| `templates` | log event templates (server + global) |
| `log_channels` | event type → channel mapping (per server only) |
| `legacy_global_log_channels` | global mappings retired by migration 0003; inspect them while recreating per-server mappings |
| `badwords` | regex patterns (server + global) |
| `members` | join/leave history, join count, message count, last seen |
| `messages` | recent message content + attachments (retention-limited) |
| `events` | append-only audit log |
| `warns` / `bans` | warnings and ban history |
| `users` | username/avatar cache |

Environment variables override `config.toml` values (see table).

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

Bunny's durability layer maintains service snapshots and WAL segments, but
those are provider-managed recovery mechanisms rather than an
operator-controlled backup with chosen retention.

Bunny's `bsql` shell supports an interactive `.dump` that emits schema and data
as SQL, and it can execute a `.sql` file against a new database. A manual round
trip was verified with `@bunny.net/database-shell` 0.2.3:

1. Stop both apps to prevent writes during the logical export.
2. Connect with `bsql`, using environment-based credentials so the access token
   is not stored in shell history.
3. Run `.unmask`, then `.dump`, confirm the full-table read warning, and save
   only the emitted `CREATE` and `INSERT` SQL in protected storage. `.unmask` is
   necessary because the shell masks sensitive-looking columns by default.
4. Create a separate empty Bunny Database and execute the dump file with
   `bsql DATABASE_URL --token TOKEN backup.sql` (prefer environment-based
   credentials during the real operation).
5. Start the bot, then API, and verify known data plus one reversible write.

`bsql .dump` currently requires an interactive TTY and confirmation; it rejects
piped stdin. This is a usable manual staging recovery path, not scheduled,
encrypted, monitored production backup automation. Establish retention,
off-service storage, failure alerts, and a successful restore drill before
production use. Local SQLite file-copy instructions do not apply to Bunny
Database.

References: [Bunny Database shell](https://docs.bunny.net/database/connect/database-shell)
and [durability/consistency](https://docs.bunny.net/database/durability-and-consistency).
