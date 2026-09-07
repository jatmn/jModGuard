# jModGuard

**Database compatibility:** this release requires a fresh database or the recognized
`jmodguard-core-1` family. Earlier databases/backups require their matching old release;
they cannot be adopted by this baseline. See [database setup and recovery](docs/DATABASE-BASELINE.md).

An optional conductor foundation is available for local development; operational
updates and backups are still pending. The existing bot/API installation does not
require it. See [implementation status](docs/OVERHAUL.md).

A **data-driven Discord moderation bot** written in Rust — open source, dual-process
(bot + API/WebUI), libSQL-backed, multi-server.

It can use a local SQLite file for single-host self-hosting or a remote libSQL
service. **Bunny.net is the first-class cloud provider**: Bunny Database, private
Magic Containers for the bot and API, and the admin UI on Storage/CDN published
from a private GitHub Actions repo. You do not need Bunny to run it
([SETUP.md](docs/SETUP.md)); that is the self-host / other-provider path.

- **Automod** against spam bots: @everyone/@here pings, multi-channel
  attachment floods (crypto scams), links in the first minutes after joining,
  bypass-resistant badword filtering, invite links, message floods, and
  brand-new accounts — each rule with its own parameters, whitelists, and an
  **action escalation table** (delete → warn → timeout → kick → ban) plus
  **dry-run mode** to tune safely.
- **Everything is config**: commands, rules, badwords, log events, and
  templates live in the database (not code) and are editable from the WebUI or in
  chat. Rename `!ban` to `!yeet`, write custom commands as **action plans**
  (reply, dm, log, roles, timeouts…), reword every log message.
- **Per-server config with global defaults**: one WebUI manages all your
  servers; mark rules/templates/commands global to share them, or copy an
  entire configuration from one server to another.
- **Logging**: every event type (joins, leaves, bans, edits, deletes, automod
  hits, mod actions) routes to its own channel with editable templates.
- **Minimal routine storage**: basic Discord-channel reports without SQL message/member
  history. Selected moderation records remain durable; see [logging/data](docs/LOGGING-DATA.md).
- **Prefix + slash commands**, multi-server, Linux binary (SQLite and
  TLS are bundled — zero system dependencies).

## Quick start

While this repository is private, authenticate **HTTPS** GitHub access
**before** cloning (`gh auth login` then `gh auth setup-git`). That covers
both the clone and Cargo's later git fetch of this same repo. An SSH key or
`git@github.com:` clone is not enough by itself — see
[docs/SETUP.md](docs/SETUP.md). Once the repository is public, skip that.

```bash
git clone https://github.com/jatmn/jModGuard.git
cd jModGuard
# Bot without the `web` feature (no axum / sessions / SPA embed).
cargo build --locked --release --bin jmodguard-bot --no-default-features
# API with the default `web` feature.
cargo build --locked --release --bin jmodguard-api
cp config.example.toml config.toml     # add your token + OAuth ids
./target/release/jmodguard-bot         # Discord gateway (migrates DB)
./target/release/jmodguard-api         # WebUI/API on http://localhost:8080
# open http://localhost:8080 and login with Discord
```

Start the bot once before the API so schema creation/migrations and default-data
bootstrap are complete. The API waits about 30 seconds locally, or about six
minutes against a remote database. Both
processes use the same local SQLite file (`db.path`) or remote libSQL database
(`db.url` and `db.token`).

Full walkthrough (Discord developer portal, intents, invite URL, systemd):
**[docs/SETUP.md](docs/SETUP.md)**.

Upgrading an existing install: **[docs/UPGRADE.md](docs/UPGRADE.md)**.

## Deploy options

| Path | Doc |
|------|-----|
| Single host / VPS (binaries or your own containers) | [docs/SETUP.md](docs/SETUP.md) |
| Bunny Database + private Magic Containers + CDN admin UI | [docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md) |
| Dual-process design | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |

## Documentation

| Doc | Contents |
|---|---|
| [docs/SETUP.md](docs/SETUP.md) | from zero to a running bot on Linux |
| [docs/UPGRADE.md](docs/UPGRADE.md) | backup, migrate, rollout, rollback |
| [docs/BACKUP-RESTORE.md](docs/BACKUP-RESTORE.md) | local recovery procedure and Bunny production blocker |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | bot + API, shared DB, `config_version` |
| [docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md) | Bunny: CDN admin UI, private bot/API containers, Database |
| [docs/COMMANDS.md](docs/COMMANDS.md) | built-in commands, permissions, custom command plans |
| [docs/AUTOMOD.md](docs/AUTOMOD.md) | the 7 rules, escalation, whitelists, dry-run, evidence |
| [docs/TEMPLATES.md](docs/TEMPLATES.md) | event templates + variable reference |
| [docs/WEBUI.md](docs/WEBUI.md) | admin UI files, how to host them, tabs, REST API |
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
cargo test --locked --workspace
cargo clippy --locked --bins -- -D warnings
cargo clippy --locked --bin jmodguard-bot --no-default-features -- -D warnings
cargo clippy --locked --bin jmodguard-api --no-default-features --features web -- -D warnings
cargo build --locked --bin jmodguard-bot --no-default-features
cargo build --locked --bin jmodguard-api
# or: ./scripts/smoke-dual-process.sh
```

Feature `web` enables axum + tower-sessions. Default also enables `embed-ui`,
which bakes `frontend/` into the API for a local SETUP.md run. Always build
the bot with `--no-default-features`. The Bunny API image builds
`--no-default-features --features web` so the container has no admin UI.
The API binary requires `web` (`required-features = ["web"]`).

## Source layout

```
lib.rs         shared crate root (modules + init_tracing; web behind feature)
bin/jmodguard-bot.rs   Discord gateway only (migrate=true, config_version poll)
bin/jmodguard-api.rs   axum API/WebUI only (migrate=false; requires feature web)
config.rs      config.toml + env overrides (db.url/token, config_poll_secs)
db.rs          local/remote libSQL pool and public data-layer facade
db/bootstrap.rs        migrations, startup fencing, versions, settings healing
db/configuration.rs    rules, commands, templates, badwords, config snapshots
db/settings.rs         inherited settings reads/writes
db/nickname_jobs.rs    durable Discord nickname reconciliation jobs
db/moderation.rs       moderation records and transitional historical data helpers
db_compat.rs   synchronous libSQL transaction/deadline/outcome facade
defaults.rs    seeded rules / commands / templates
types.rs       serde models (RuleDef, CommandDef, Behavior, PlanStep, …)
state.rs       BotState, compiled guild config cache + version poll
web/state.rs   ApiState, dashboard caches + config read observation
web/oauth.rs   dashboard OAuth client + identity models
normalize.rs   bypass-resistant text normalization (NFD, zero-width, leetspeak)
templates.rs   {variable} renderer shared by everything
logging.rs     event → channel router + audit log
automod.rs     rule engine (sliding windows, escalation) + action executor
handlers.rs    gateway events (join/leave, messages, edits, deletes, bans)
crates/runtime/src/health.rs  GET/HEAD /health liveness listener (no axum; API serves same path until axum)
commands/      data-driven engine: dispatch, permissions, behaviors, plans, slash
web/           axum API + OAuth (feature web; embed-ui serves frontend/ locally)
frontend/      admin SPA (Bunny Storage/CDN; local API embed)
migrations/    0001_init through 0006_bot_actions
Dockerfile.bot / Dockerfile.api
```
