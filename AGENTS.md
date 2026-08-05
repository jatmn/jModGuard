# jModGuard — agent notes

## Commands

```bash
cargo build --release    # release binary: target/release/jmodguard(.exe)
cargo test               # unit tests (normalizer, templates, db seeding, web router)
cargo clippy -- -D warnings
```

## Version pins (verified)

- serenity **0.12.5** — see `docs/RESEARCH.md` for the breaking 0.12.5 API
  changes (EventHandler signatures, `!Send` CacheRef, Timestamp newtype,
  `CommandInteraction` rename, `GuildId::set_commands`).
- rusqlite 0.40 (bundled), r2d2_sqlite 0.35, axum 0.8, tower-sessions 0.14,
  reqwest 0.13, edition 2024.

## Invariants to respect

- Never hold a serenity cache ref (`cache.guild()`, `cache.user()`) across
  `.await` — clone owned data first (CacheRef is `!Send`).
- `Db::with_conn` closures are `Send + 'static`: pass owned `String`s, never
  borrowed `&str`.
- Automod state must stay bounded: prune buckets, keep lookups O(1)
  per-message (per-(guild,user) buckets), no full-map scans.
- All user-visible strings flow through the template renderer
  (`src/templates.rs`) — no hardcoded chat messages in behaviors.
- Writes must call `app.invalidate_configs()` or the config cache stays stale.
- Slash registration: `GuildId::set_commands(http, Vec<CreateCommand>)`,
  responses via `Builder::execute` (`use serenity::builder::Builder;`).

## Layout

```
src/main.rs        startup
src/config.rs      config.toml + env
src/db.rs          SQLite (WAL) + data layer
src/defaults.rs    seeded rules/commands/templates
src/types.rs       serde models (RuleDef/CommandDef/Behavior/PlanStep)
src/state.rs       AppState + config cache (global fallback, 15s TTL)
src/automod.rs     rule engine + action executor
src/handlers.rs    gateway events
src/commands/      engine, behaviors, plans
src/web/           axum WebUI + OAuth + static UI
docs/              setup + reference docs
```
