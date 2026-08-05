# Configuration

There are two layers: the static `config.toml` (tokens, bindings) and the
database (everything server-related, editable at runtime).

## config.toml

| Key | Default | Description |
|---|---|---|
| `discord.token` | — | Bot token (env: `JMODGUARD_DISCORD_TOKEN`) |
| `db.path` | `jmodguard.db` | SQLite file (env: `JMODGUARD_DB_PATH`) |
| `db.pool_size` | `8` | connection pool size |
| `web.bind` | `127.0.0.1:8080` | WebUI listen address (env: `JMODGUARD_WEB_BIND`) |
| `web.base_url` | `http://localhost:8080` | public URL used for OAuth redirect (env: `JMODGUARD_WEB_BASE_URL`) |
| `web.oauth_client_id` | — | Discord OAuth client ID (env: `JMODGUARD_OAUTH_CLIENT_ID`) |
| `web.oauth_client_secret` | — | Discord OAuth client secret (env: `JMODGUARD_OAUTH_CLIENT_SECRET`) |
| `bot.default_prefix` | `!` | default command prefix |
| `bot.owner_id` | `0` | owner user ID (owner commands) (env: `JMODGUARD_OWNER_ID`) |
| `bot.retention_days` | `7` | raw message retention for edit/delete evidence |
| `bot.status` | `moderating` | presence text |
| `log.level` | `info` | tracing level (env: `JMODGUARD_LOG_LEVEL`) |
| `log.directory` | `logs` | rolling log directory (empty = console only) |

## Database (runtime config)

All of this is managed via the WebUI or bot commands; the schema lives in
`migrations/0001_init.sql` and is applied automatically.

| Table | Holds |
|---|---|
| `guild_settings` | prefix, bot nickname, retention (row per server; `guild_id=0` = global) |
| `rules` | the 7 automod rules (server + global rows) |
| `commands` | every command — name, aliases, behavior, response, permissions (server + global) |
| `templates` | log event templates (server + global) |
| `log_channels` | event type → channel mapping (server + global) |
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

The whole configuration lives in `jmodguard.db`. Stop the bot and copy the
file (plus `jmodguard.db-wal`, if present) — or back it up while running with
`VACUUM INTO` (SQLite is WAL-safe for hot copies).
