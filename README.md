# jModGuard

[![Build status](https://badge.buildkite.com/38ea07d60d3ab8cf765aa025cb56cbdb65d3e17d437b5f0c98.svg)](https://buildkite.com/jatmn/jmodguard)

**Database compatibility:** this release requires a fresh database or the recognized
`jmodguard-core-1` family. Earlier databases/backups require their matching old release;
they cannot be adopted by this baseline. See [database setup and recovery](docs/DATABASE-BASELINE.md).

An optional conductor can serve owner Administration locally when configured,
including local backup, restore, guild-store moves, signed release discovery,
and owner-selected updates. Public install validation lives on the conductor
(`init-local`, `validate-install`, `/api/install`). See
[public installation](docs/INSTALL.md). The existing bot/API installation does
not require the conductor. See [implementation status](docs/OVERHAUL.md).

A **data-driven Discord moderation bot** written in Rust — open source, dual-process
(bot + API/WebUI), libSQL-backed, multi-server.

It can use a local SQLite file for single-host self-hosting or a remote libSQL
service. **Bunny.net is the first-class cloud provider**: Bunny Database, private
Magic Containers for the bot and API, and the admin UI on Storage/CDN from this
public `frontend/` (a private GitHub Actions lock is optional). You do not need
Bunny to run it ([SETUP.md](docs/SETUP.md)); that is the self-host / other-provider
path.

- **Automod** against spam bots: @everyone/@here pings, multi-channel
  [exact-image and repeated-text spam](docs/IMAGE-SPAM.md) (test or delete-and-ban),
  links in the first minutes after joining,
  bypass-resistant badword filtering, invite links, message floods, and
  brand-new accounts — each rule with its own parameters and whitelists. Generic
  rules keep an **action escalation table** (delete → warn → timeout → kick → ban)
  plus **dry-run mode** to tune safely.
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
# Independent bot package (no dashboard stack).
cargo build --locked --release -p jmodguard-bot
# API with the default embedded frontend.
cargo build --locked --release -p jmodguard-api
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
| Public install (Bunny, local, AWS) | [docs/INSTALL.md](docs/INSTALL.md) |
| Single host / VPS (binaries or your own containers) | [docs/SETUP.md](docs/SETUP.md) |
| Bunny Database + private Magic Containers + CDN admin UI | [docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md) |
| AWS EC2 + local adapter + SQLite/libSQL | [docs/DEPLOY-AWS.md](docs/DEPLOY-AWS.md) |
| Cost formulas | [docs/COST.md](docs/COST.md) |
| Dual-process design | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |

## Documentation

| Doc | Contents |
|---|---|
| [docs/SETUP.md](docs/SETUP.md) | from zero to a running bot on Linux |
| [docs/INSTALL.md](docs/INSTALL.md) | public Bunny/local/AWS install and deferred live checks |
| [docs/DEPLOY-AWS.md](docs/DEPLOY-AWS.md) | EC2 + local adapter; not RDS |
| [docs/COST.md](docs/COST.md) | measured local resource formulas |
| [docs/UPGRADE.md](docs/UPGRADE.md) | backup, migrate, rollout, rollback |
| [docs/BACKUP-RESTORE.md](docs/BACKUP-RESTORE.md) | local recovery procedure and Bunny production blocker |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | bot + API, shared DB, `config_version` |
| [docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md) | Bunny: CDN admin UI, private bot/API containers, Database |
| [docs/COMMANDS.md](docs/COMMANDS.md) | built-in commands, permissions, custom command plans |
| [docs/AUTOMOD.md](docs/AUTOMOD.md) | the 8 rules, escalation, whitelists, dry-run, evidence |
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
bash scripts/ci-preflight.sh
# or: ./scripts/smoke-dual-process.sh
# optional: bash scripts/install-git-hooks.sh  # full preflight; per-worktree; see AGENTS.md for --base
```

Bot and API are independent Cargo packages. The API defaults to `embed-ui` for local
use; `cargo build -p jmodguard-api --no-default-features` builds JSON/OAuth without
embedding the frontend, as used by Bunny. The bot never depends on the dashboard stack.
The old root `web` feature is removed. Select production builds with `-p`; bare workspace
commands include all packages, while `cargo test --workspace` preserves full test coverage.

## Source layout

```text
src/                       shared model, configuration, database, normalization and templates
crates/bot/src/            Discord Gateway, moderation, commands and action/nickname workers
crates/api/src/            HTTP/OAuth API and dashboard state
crates/api/tests/          actual API startup regression
crates/integration-tests/  shared-DB, cross-service and optional remote-libSQL regressions
crates/runtime/            shared process health/shutdown utilities
crates/conductor/          independent control service (owner Administration and local backups)
frontend/                  static dashboard (Bunny Storage/CDN or local API embed)
migrations/                jmodguard-core-1, steps 0001–0009
Dockerfile.bot / Dockerfile.api / Dockerfile.conductor
```
