# jModGuard

[![CI](https://github.com/jatmn/jModGuard/actions/workflows/ci.yml/badge.svg)](https://github.com/jatmn/jModGuard/actions/workflows/ci.yml)

**Database compatibility:** this release requires a fresh database or the recognized
`jmodguard-core-1` family. Earlier databases/backups require their matching old release;
they cannot be adopted by this baseline. See [database setup and recovery](docs/DATABASE-BASELINE.md).

Owner Administration is available on the existing WebUI and API.
See [public installation](docs/INSTALL.md). See [implementation status](docs/OVERHAUL.md).

A **data-driven Discord moderation bot** written in Rust — source available, dual-process
(bot + API/WebUI), libSQL-backed, multi-server.

It can use a local SQLite file for single-host self-hosting or a remote libSQL
service. **Bunny.net is the first-class cloud provider**: Bunny Database, private
Magic Containers for the bot and API, and the admin UI on Storage/CDN from this
public `frontend/`. You do not need
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
- **Bounded routine storage**: Discord-channel reports with rolling latest-message snapshots and no persistent member
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
# API with the default embedded frontend (Node.js 22.13+).
npm ci --ignore-scripts --prefix tools/webui
npm run build --prefix tools/webui
cargo build --locked --release -p jmodguard-api
cp config.example.toml config.toml     # add your token + OAuth ids
./target/release/jmodguard-bot         # Discord gateway (migrates DB)
./target/release/jmodguard-api         # WebUI/API on http://localhost:8080
# open http://localhost:8080 and login with Discord
```

Start the bot once before the API so schema creation/migrations and default-data
bootstrap are complete. The API does not migrate. It keeps retrying with a
capped delay. `/health` stays 200. `/ready` and app routes stay 503 until it
is ready. It exits on a newer unsupported schema, rejected remote credentials
and other non-retryable open errors, and bad config such as an empty OAuth
client id or secret. Both processes use the same local SQLite file (`db.path`)
or remote libSQL database (`db.url` and `db.token`).

Full walkthrough (Discord developer portal, intents, invite URL, systemd):
**[docs/SETUP.md](docs/SETUP.md)**.

Upgrading an existing install: **[docs/UPGRADE.md](docs/UPGRADE.md)**.

## Deploy options

| Path | Doc |
|------|-----|
| Public install (Bunny, local, AWS) | [docs/INSTALL.md](docs/INSTALL.md) |
| Single host / VPS (binaries or your own containers) | [docs/SETUP.md](docs/SETUP.md) |
| Bunny Database + private Magic Containers + CDN admin UI | [docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md) |
| AWS EC2 + SQLite/libSQL | [docs/DEPLOY-AWS.md](docs/DEPLOY-AWS.md) |
| Cost formulas | [docs/COST.md](docs/COST.md) |
| Dual-process design | [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) |

## Documentation

| Doc | Contents |
|---|---|
| [docs/SETUP.md](docs/SETUP.md) | from zero to a running bot on Linux |
| [docs/INSTALL.md](docs/INSTALL.md) | public Bunny/local/AWS install and deferred live checks |
| [docs/DEPLOY-AWS.md](docs/DEPLOY-AWS.md) | EC2 + SQLite/libSQL; not RDS |
| [docs/COST.md](docs/COST.md) | deployment cost estimation |
| [docs/UPGRADE.md](docs/UPGRADE.md) | backup, migrate, rollout, rollback |
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

jModGuard is **source available** under the **Apache License 2.0 with the
Commons Clause License Condition v1.0**. You may use, study, modify, fork, and
redistribute it under those terms, retaining the required license and attribution
notices. Personal use, internal business use, and running it for a business's own
free Discord community are allowed.

Selling or reselling jModGuard, or charging for a product or service whose value
derives entirely or substantially from jModGuard's functionality, requires a
separate written license from **jatmn**. This includes paid bot hosting and
consulting/support services covered by the Commons Clause definition of "Sell".
The restriction is not a blanket ban on commercial use or every larger paid
product that includes jModGuard.

See [LICENSE.md](LICENSE.md) for the complete terms; this summary does not replace
them. Because the license restricts selling, jModGuard is not OSI open source.
Third-party dependencies and vendored code retain their own licenses. Previously
released versions remain subject to the license supplied with those versions.

## Development

Owner-only, on-demand Pullfrog commands and diagnostics are documented in [docs/PULLFROG.md](docs/PULLFROG.md).
It does not replace GitHub Actions CI.

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
frontend/                  static dashboard (Bunny Storage/CDN or local API embed; daisyUI/Headless bundled at build time)
migrations/                jmodguard-core-1, steps 0001–0009
Dockerfile.bot / Dockerfile.api
```
