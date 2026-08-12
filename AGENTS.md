# jModGuard — agent notes

Contributor-oriented build and design invariants for the public jModGuard repo.
Deploy topology for Bunny: `docs/DEPLOY-BUNNY.md`. Upgrades: `docs/UPGRADE.md`.

## Commands

```bash
cargo test --locked
cargo clippy --locked --bins -- -D warnings
cargo clippy --locked --bin jmodguard-bot --no-default-features -- -D warnings
cargo build --locked --bin jmodguard-bot --no-default-features
cargo build --locked --bin jmodguard-api
# or: ./scripts/smoke-dual-process.sh
```

Local dual-process:

```bash
cargo run --bin jmodguard-bot --no-default-features   # migrates DB; Discord gateway
cargo run --bin jmodguard-api                         # expects migrated schema (migrate=false)
```

## Version pins (verified)

- Rust **1.96.0** — `rust-toolchain.toml`, CI (`dtolnay/rust-toolchain@1.96.0`),
  and `Dockerfile.{bot,api}` (`rust:1.96-bookworm`) must stay aligned.
  `Cargo.toml` `rust-version = "1.96"`. Do not float CI on `@stable`.
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
- Config writers bump durable `config_version` in the DB layer (same
  transaction as the write); call sites still call `app.invalidate_configs()`
  for same-process freshness. Both bot and API poll `config_version` for
  cross-process invalidation. Command registry writers (commands table /
  command copies) also bump `slash_registry_version`; the bot peeks that
  every tick (independently of `config_version`) and re-syncs Discord slash
  commands for cached guilds when it advances, committing the observed
  version only after a successful sync pass (failed guilds retry next poll;
  retries are limited to the failure set so one bad guild cannot re-POST all
  guilds every tick). Empty guild cache defers without invalidating. When
  previously-failed guilds leave the cache, the observation is committed.
  `AppState::new` primes both version baselines from the DB so the first poll
  has no unobserved window (never use a “skip first observation” sentinel).
- Runtime writers (messages/members/events/warns/bans/users) must **not** bump
  `config_version`.
- Global WebUI routes (`guild_id=0`) and body `scope: "global"` require
  `bot.owner_id` == logged-in user.
- Guild-scoped WebUI authorization (`guild_scope` for real guild ids) always
  re-fetches Discord `/users/@me/guilds` before `can_manage`. The 60s guild
  cache is **display-only** (`/api/me`). Never authorize mutations from the
  display cache (post-demotion access window).
- `AuthCtx` from `guild_scope(0)` may be display-cached (owner identity only).
  Any secondary Discord guild check (e.g. `copy_config` source) must use
  `authorize_manage_guild` / `require_auth_fresh` — never `auth.can_manage`
  on a CachedOk context.
- Badword delete is exact-scope: guild admins delete only their guild's rows;
  global rows require owner. All denials return the same `"badword not found"`
  (never distinguish missing / cross-guild / non-owner global).
- Badwords copy is **validate-then-replace**: build the enforceable replacement
  set first; only then DELETE destination and INSERT. If the source has rows
  but none compile, **refuse** (error) and leave destination unchanged — never
  DELETE-then-filter. Empty source clears destination (intentional).
  `compile_user_regex` is the single gate for add / copy / cache load / tester.
- Feature `web` (default): axum + tower-sessions + embedded SPA. Build the bot
  with `--no-default-features` so it does not link or embed the WebUI.
- Sessions use `MemoryStore` intentionally for a **single** API process; lost on
  restart. Shared session store is a future multi-replica concern.
- Cross-origin CDN SPA: set `web.cors_origins` / `JMODGUARD_CORS_ORIGINS` and
  `https` `web.base_url`; frontend uses `credentials: "include"`. Prefer
  same-origin reverse proxy when possible.
- Discord snowflake IDs on the WebUI wire are **strings** (never JS `Number`).
  Audit-event `data` blobs stringify integers above JS `Number.MAX_SAFE_INTEGER`
  at write and on read (legacy rows). `WarnRow` / `BanRow` / `MemberRow` /
  `EventRow` / `BadwordView` snowflake fields use the same string serde.
- Bot nickname: WebUI empty field, `!setname` with no args / `clear` / `-`, and
  `/setname` with the optional `name` omitted all clear the Discord nickname and
  persist `bot_name=""`. Discord is updated **before** SQLite (WebUI and
  `!setname`); if SQLite then fails, Discord is restored to the previous nick
  (shared retry helper). WebUI "Use global" sends `bot_name: null` to inherit
  the parent nickname. Copy-config preflights refuse conditions (badwords /
  logchannels-only) before any Discord nickname mutation. `guild_create` only
  applies a nickname when `bot_name` is non-empty (does not re-clear on join).
- `guild_settings` rows are **partial overrides** (field-level merge: guild →
  global → config terminal defaults from `bot.default_prefix` /
  `bot.retention_days`). Joining a server does **not** insert a settings row.
  `!prefix` / `!setname` / WebUI settings patch only the changed fields.
  WebUI/API `null` on a field clears that override (inherit parent); omitted
  fields are left unchanged (`de_settings_patch_field` — default Option nesting
  would treat JSON `null` as omitted). Empty prefix/retention in the WebUI
  clears an existing override (`null`); empty bot nickname still means
  clear-to-empty.
  Empty prefix values are rejected by `patch_settings`. Bot migrate-open
  rewrites legacy settings blobs (including global) to parent-relative
  overrides without deleting ambiguous complete default blobs, and clears corrupt JSON (including at
  runtime read), and bumps `config_version` when rows change. The API process
  runs the same idempotent heal on open (`migrate=false`) so legacy rows are
  fixed even if the bot has not restarted yet. Heal is skipped on open when
  `meta.settings_heal_generation` + `meta.settings_heal_checksum` match the
  current `guild_settings` table (fast-path; checksum is a stable FNV-1a so
  bot and API agree across process restarts); bump `SETTINGS_HEAL_GENERATION`
  in code when heal logic or the checksum algorithm changes to force one full
  re-pass. Every committed `guild_settings` mutation (patch, copy settings,
  test upsert, heal, runtime corrupt clear on autocommit) calls
  `refresh_settings_heal_checksum_conn` so the fast-path stays accurate.
- Message retention purge is **per-guild** via the same field-level resolution
  (`0` keeps forever). `bot.retention_days` / `bot.default_prefix` are synced
  into durable `meta.settings_terminal_defaults` on bot migrate-open (bot is
  authority) and read from that meta row on every settings resolve (not cached
  at process open) as the final fallback when neither guild nor global sets a
  field — shared by WebUI / `get_settings` / config cache / purge so bot and
  API cannot diverge. The API refuses to open when that meta row is missing
  (start the bot first).
- Cross-process config lag up to `config_poll_secs` after a successful baseline
  is expected (not a bug).
- Log channel IDs are Discord-guild-scoped and are **not** copied between
  servers (copy skips them with `skipped_logchannels`).
- Log files are per-process: `jmodguard-bot.log` / `jmodguard-api.log` under
  `log.directory`.
- Slash registration: `GuildId::set_commands(http, Vec<CreateCommand>)`,
  responses via `Builder::execute` (`use serenity::builder::Builder;`).

## Layout

```
src/lib.rs              shared library
src/bin/jmodguard-bot.rs
src/bin/jmodguard-api.rs
src/config.rs           config.toml + env (db.url/token, config_poll_secs)
src/db.rs               SQLite (WAL) + migrations + config_version
src/discord.rs          Discord REST helpers (nickname retries)
src/defaults.rs         seeded rules/commands/templates
src/types.rs            serde models (RuleDef/CommandDef/Behavior/PlanStep)
src/state.rs            AppState + config cache + version poll
src/automod.rs          rule engine + action executor
src/handlers.rs         gateway events
src/commands/           engine, behaviors, plans
src/web/                axum API + OAuth (embeds frontend/)
frontend/               SPA assets
migrations/             0001_init + 0002_config_version
Dockerfile.bot / Dockerfile.api
docs/                   SETUP, UPGRADE, ARCHITECTURE, DEPLOY-BUNNY, …
```
