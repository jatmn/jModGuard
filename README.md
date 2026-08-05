# jModGuard

A **data-driven Discord moderation bot** written in Rust.

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
cargo build --release
cp config.example.toml config.toml     # add your token + OAuth ids
./target/release/jmodguard             # or jmodguard.exe
# open http://localhost:8080 and login with Discord
```

Full walkthrough (Discord developer portal, intents, invite URL with the
right permissions, systemd/Task Scheduler, first-run tuning):
**docs/SETUP.md**

## Documentation

| Doc | Contents |
|---|---|
| [docs/SETUP.md](docs/SETUP.md) | from zero to a running bot on Windows & Linux |
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
cargo test          # unit tests (normalizer, templates, db seeding, web)
cargo clippy        # lint
cargo build --release
```

## Architecture (src layout)

```
main.rs        startup: config → db → WebUI bind → Discord client
config.rs      config.toml + env overrides
db.rs          SQLite pool (WAL), migrations, data layer, config-parts loader
defaults.rs    seeded rules / commands / templates
types.rs       serde models (RuleDef, CommandDef, Behavior, PlanStep, …)
state.rs       AppState, OAuth client, per-guild config cache (global fallback)
normalize.rs   bypass-resistant text normalization (NFD, zero-width, leetspeak)
templates.rs   {variable} renderer shared by everything
logging.rs     event → channel router + audit log
automod.rs     rule engine (sliding windows, escalation) + action executor
handlers.rs    gateway events (join/leave, messages, edits, deletes, bans)
commands/      data-driven engine: dispatch, permissions, behaviors, plans, slash
web/           axum WebUI: OAuth login, JSON API, static single-page app
```
