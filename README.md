# jModGuard

A **data-driven Discord moderation bot** written in Rust — open source, dual-process
(bot + API/WebUI), SQLite-backed, multi-server.

It is designed to self-host cleanly on a single machine. Bunny CDN/Storage can
serve the static admin UI, but the bot and API currently need the same host or
shared filesystem for their SQLite database. You do not need Bunny to run it.

- **Automod** against spam bots: @everyone/@here pings, multi-channel
  attachment floods (crypto scams), links in the first minutes after joining,
  bypass-resistant badword filtering, invite links, message floods, and
  brand-new accounts — each rule with its own parameters, whitelists, and an
  **action escalation table** (delete → warn → timeout → kick → ban) plus
  **dry-run mode** to tune safely.
- **Everything is config**: commands, rules, badwords, log events, and
  templates live in SQLite (not code) and are editable from the WebUI or in
  chat. Rename `!ban` to `!yeet`, write custom commands as **action plans**
  (reply, dm, log, roles, timeouts…), reword every log message.
- **Per-server config with global defaults**: one WebUI manages all your
  servers; mark rules/templates/commands global to share them, or copy an
  entire configuration from one server to another.
- **Logging**: every event type (joins, leaves, bans, edits, deletes, automod
  hits, mod actions) routes to its own channel with editable templates.
- **Member tracking**: join/leave history, join counts, message counts.
- **Prefix + slash commands**, multi-server, Windows/Linux binary (SQLite and
  TLS are bundled — zero system dependencies).

## Quick start

```bash
# Bot without the `web` feature (no axum / sessions / SPA embed).
cargo build --release --bin jmodguard-bot --no-default-features
# API with the default `web` feature.
cargo build --release --bin jmodguard-api
cp config.example.toml config.toml     # add your token + OAuth ids
./target/release/jmodguard-bot         # Discord gateway (migrates DB)
./target/release/jmodguard-api         # WebUI/API on http://localhost:8080
# open http://localhost:8080 and login with Discord
```

Start the bot once before the API so migrations are applied. Both processes
share the same SQLite file (`db.path`).

Full walkthrough (Discord developer portal, intents, invite URL, systemd /
Task Scheduler): **[docs/SETUP.md](docs/SETUP.md)**.

Upgrading an existing install: **[docs/UPGRADE.md](docs/UPGRADE.md)**.

## Deploy options

| Path | Doc |
|------|-----|
| Single host / VPS (binaries or your own containers) | [docs/SETUP.md](docs/SETUP.md) |
| Optional Bunny CDN/Storage front end for a shared-DB host | [docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md) |
| Dual-process design | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |

## Documentation

| Doc | Contents |
|---|---|
| [docs/SETUP.md](docs/SETUP.md) | from zero to a running bot on Windows & Linux |
| [docs/UPGRADE.md](docs/UPGRADE.md) | backup, migrate, rollout, rollback |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | bot + API, shared DB, `config_version` |
| [docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md) | optional Bunny.net deployment guide |
| [docs/COMMANDS.md](docs/COMMANDS.md) | built-in commands, permissions, custom command plans |
| [docs/AUTOMOD.md](docs/AUTOMOD.md) | the 7 rules, escalation, whitelists, dry-run, evidence |
| [docs/TEMPLATES.md](docs/TEMPLATES.md) | event templates + variable reference |
| [docs/WEBUI.md](docs/WEBUI.md) | WebUI tabs, global vs per-server config, REST API |
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | config.toml reference, database layout, backups |
| [docs/RESEARCH.md](docs/RESEARCH.md) | version-specific findings (serenity 0.12.5 etc.) |

## In-chat help

`!help` (or `/help`) lists every command with its description, generated from
the command registry — the same data powers `docs/COMMANDS.md`.

## License

Licensed under the **PolyForm Noncommercial License 1.0.0** — see
[LICENSE.md](LICENSE.md). This license is **permanent**: there is no
conversion clause. Anyone may use, study, and fork the software for any
**non-commercial** purpose (with the notice retained), but **no one may sell
it or use it commercially without a written license from the author** —
commercial licensing is granted by the author only.

## Development

```bash
cargo test --locked
cargo clippy --locked --bins -- -D warnings
cargo clippy --locked --bin jmodguard-bot --no-default-features -- -D warnings
cargo build --locked --bin jmodguard-bot --no-default-features
cargo build --locked --bin jmodguard-api
# or: ./scripts/smoke-dual-process.sh
```

Feature `web` (default) enables axum + tower-sessions + the embedded SPA.
Always build the bot with `--no-default-features` so it does not link or embed
the WebUI. The API binary requires `web` (`required-features = ["web"]`).

## Source layout

```
lib.rs         shared crate root (modules + init_tracing; web behind feature)
bin/jmodguard-bot.rs   Discord gateway only (migrate=true, config_version poll)
bin/jmodguard-api.rs   axum API/WebUI only (migrate=false; requires feature web)
config.rs      config.toml + env overrides (db.url/token, config_poll_secs)
db.rs          SQLite pool (WAL), migrations, config_version, data layer
defaults.rs    seeded rules / commands / templates
types.rs       serde models (RuleDef, CommandDef, Behavior, PlanStep, …)
state.rs       AppState, OAuth client, per-guild config cache + version poll
normalize.rs   bypass-resistant text normalization (NFD, zero-width, leetspeak)
templates.rs   {variable} renderer shared by everything
logging.rs     event → channel router + audit log
automod.rs     rule engine (sliding windows, escalation) + action executor
handlers.rs    gateway events (join/leave, messages, edits, deletes, bans)
commands/      data-driven engine: dispatch, permissions, behaviors, plans, slash
web/           axum WebUI: OAuth login, JSON API (feature web; embeds frontend/)
frontend/      SPA assets (served by API locally; CDN target when deployed)
migrations/    0001_init + 0002_config_version
Dockerfile.bot / Dockerfile.api
```
