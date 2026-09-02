# jModGuard — agent notes

Contributor-oriented build and design invariants for the public jModGuard repo.
Deploy topology for Bunny: `docs/DEPLOY-BUNNY.md`. Upgrades: `docs/UPGRADE.md`.

## Commands

```bash
cargo test --locked
cargo test --locked --no-default-features --lib
cargo clippy --locked --bins -- -D warnings
cargo clippy --locked --bin jmodguard-bot --no-default-features -- -D warnings
cargo clippy --locked --bin jmodguard-api --no-default-features --features web -- -D warnings
cargo build --locked --bin jmodguard-bot --no-default-features
cargo build --locked --bin jmodguard-api
cargo build --locked --bin jmodguard-api --no-default-features --features web
node --test scripts/frontend-warmup-test.mjs
node --test scripts/publish-frontend-test.mjs
# or: ./scripts/smoke-dual-process.sh
```

Local dual-process:

```bash
cargo run --bin jmodguard-bot --no-default-features   # migrates DB; Discord gateway
cargo run --bin jmodguard-api                         # waits ~30s locally / ~6m remote for bot migrate, then serves HTTP
```

## Version pins (verified)

- Rust **1.96.0** — `rust-toolchain.toml`, CI (`dtolnay/rust-toolchain@1.96.0`),
  and `Dockerfile.{bot,api}` (`rust:1.96-bookworm`) must stay aligned.
  `Cargo.toml` `rust-version = "1.96"`. Do not float CI on `@stable`.
- serenity **0.12.5** — see `docs/RESEARCH.md` for the breaking 0.12.5 API
  changes (EventHandler signatures, `!Send` CacheRef, Timestamp newtype,
  `CommandInteraction` rename, `GuildId::set_commands`).
- libsql 0.9.30 (`core`, `remote`; remote HTTPS uses in-tree rustls 0.23, not
  libsql's `tls`/`hyper-rustls 0.25` stack), axum 0.8, tower-sessions 0.14,
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
- Config writers bump durable `config_version` and replace its revision token
  in the DB layer (same
  transaction as the write); call sites still call `app.invalidate_configs()`
  for same-process freshness. Both bot and API poll `config_version` for
  cross-process invalidation. Command registry writers (commands table /
  command copies) also bump `slash_registry_version` and replace its revision
  token; the bot peeks that
  every tick (independently of `config_version`) and re-syncs Discord slash
  commands for cached guilds when it advances, committing the observed
  version only after a successful sync pass (failed guilds retry next poll;
  retries are limited to the failure set so one bad guild cannot re-POST all
  guilds every tick). Empty guild cache defers without invalidating. A failed
  guild that leaves the cache keeps the observation pending; retry it when it
  returns, then commit only after every failed guild syncs successfully.
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
- Feature `web` (default with `embed-ui`): axum + tower-sessions. `embed-ui`
  bakes `frontend/` into the API for local SETUP.md. Bunny API images build
  `--no-default-features --features web` so containers do not contain the
  admin UI. Build the bot with `--no-default-features`.
- Sessions use `MemoryStore` intentionally for a **single** API process; lost on
  restart. Cookie name is `jmodguard_session` (Magic Container sticky
  sessions must use that name). Shared session store is a future
  multi-replica concern. Discord
  OAuth CSRF (`state`) and `return_to` are browser cookies so a restart
  between `/login` and `/oauth/callback` does not fail the handshake.
  Those cookies are host-only on `web.base_url`; a split-origin SPA still
  sends the user through that hostname for login and callback.
  Successful `/oauth/callback` rotates the session id. API JSON responses
  set `Cache-Control: private, no-store`.
- Cross-origin CDN SPA is not the Bunny topology. Bunny serves `frontend/`
  from Storage and proxies `/api` `/login` `/oauth` to a private API
  container (same origin; no CORS). `web.cors_origins` /
  `JMODGUARD_CORS_ORIGINS` exist only for a split-origin experiment.
  `https` `web.base_url` is the public CDN hostname. `Config::validate_cors`
  runs in `jmodguard-api` only. A public bind (`0.0.0.0`) must also pass
  `validate_public_base_url` (`JMODGUARD_WEB_BASE_URL` https, not localhost).
  Operators must set the CDN origin themselves; Magic Container hostnames
  are not a stable class this check can reject.
- Container images set `JMODGUARD_REQUIRE_REMOTE_DB=1`. Missing remote DB
  credentials must fail startup, not open local SQLite. Unset it only for an
  intentional local-SQLite container run. Runtime user is UID 10001. Env
  `JMODGUARD_DB_POOL_SIZE` (minimum 2) overrides `db.pool_size` when set
  (environment wins over `config.toml`, same as other `JMODGUARD_*` keys).
  Container deploys typically have no toml, so set the env var there.
- Bot `JMODGUARD_HEALTH_BIND` (images: `0.0.0.0:8080`) and API `GET`/`HEAD`
  `/health` are unauthenticated process liveness probes (`ok`, no database).
  Bind and accept them **before** opening the database. On the API public
  bind, only `/health` returns 200 until axum takes over (database open,
  AppState primary-version prime, router build); `/api` returns JSON 503
  `{"error":"not ready"}` and `Retry-After`, and `/login` / `/oauth` return
  HTML 503 that refreshes in place. Do not put session middleware on
  `/health`. If the env var is set, bind failure is fatal.
- Discord snowflake IDs on the WebUI wire are **strings** (never JS `Number`).
  Audit-event `data` blobs stringify integers above JS `Number.MAX_SAFE_INTEGER`
  at write and on read (legacy rows). `WarnRow` / `BanRow` / `MemberRow` /
  `EventRow` / `BadwordView` snowflake fields use the same string serde.
- Bot nickname: WebUI empty field, `!setname` with no args / `clear` / `-`, and
  `/setname` with the optional `name` omitted all clear the Discord nickname and
  persist `bot_name=""`. Settings and a generation-numbered outbox row commit
  atomically; only the bot drains that row and mutates Discord. Transient failures
  use persisted bounded backoff, permanent failures become terminal, and a real
  guild departure makes the row dormant until rejoin. Generation-checked
  acknowledgement makes the final committed value win across bot/API races.
  Nicknames are deliberately guild-only; the WebUI does not offer a global
  nickname override. `guild_create` uses a lifecycle-only transition: reconnects
  preserve terminal failures, dormant work is reactivated, and a genuinely new
  guild gets initial work (including an empty nickname).
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
  overrides without deleting ambiguous complete default blobs, clears corrupt JSON,
  and bumps `config_version` when rows change. Runtime reads ignore corrupt rows
  so inheritance remains available, but durable repair is bot-owned; restart the
  bot to run it. Heal is skipped on bot open when
  `meta.settings_heal_generation` + `meta.settings_heal_checksum` match the
  current `guild_settings` table (fast-path; checksum is a stable FNV-1a so
  bot and API agree across process restarts); bump `SETTINGS_HEAL_GENERATION`
  in code when heal logic or the checksum algorithm changes to force one full
  re-pass. Every committed `guild_settings` mutation (patch, copy settings,
  test upsert, heal) calls
  `refresh_settings_heal_checksum_conn` so the fast-path stays accurate.
- Message retention purge is **per-guild** via the same field-level resolution
  (`0` keeps forever). `bot.retention_days` / `bot.default_prefix` are synced
  into durable `meta.settings_terminal_defaults` on bot migrate-open (bot is
  authority) and read from that meta row on every settings resolve (not cached
  at process open) as the final fallback when neither guild nor global sets a
  field — shared by WebUI / `get_settings` / config cache / purge so bot and
  API cannot diverge. The API refuses to open until that meta row and the
  bot-written bootstrap-complete marker exist (start the bot first).
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
src/db.rs               local/remote libSQL pool + public data-layer facade
src/db/bootstrap.rs     migrations/startup fencing/versioning/settings heal
src/db/configuration.rs config CRUD + resolved snapshots
src/db/settings.rs      inherited settings reads/writes
src/db/nickname_jobs.rs durable Discord nickname reconciliation jobs
src/db/moderation.rs    members/messages/retention/audit/warns/bans
src/db/tests.rs         database unit and regression tests
src/db_compat.rs        synchronous facade + remote deadlines/rollback
src/remote_https.rs     rustls 0.23 HTTPS connector for remote libSQL (hyper 0.14)
src/discord.rs          Discord REST helpers (nickname retries)
src/health.rs          process-local GET/HEAD /health listener (bot; API bind until axum)
src/defaults.rs         seeded rules/commands/templates
src/types.rs            serde models (RuleDef/CommandDef/Behavior/PlanStep)
src/state.rs            AppState + config cache + version poll
src/automod.rs          rule engine + action executor
src/handlers.rs         gateway events
src/commands/           engine, behaviors, plans
src/web/                axum API + OAuth (feature web; embed-ui serves frontend/ locally)
frontend/               admin SPA (Bunny Storage/CDN via private ops runners; local: embedded in API)
migrations/             0001_init through 0005_mutation_receipts_nickname_jobs
Dockerfile.bot / Dockerfile.api
docs/                   SETUP, UPGRADE, ARCHITECTURE, DEPLOY-BUNNY, …
```
