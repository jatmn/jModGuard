# jModGuard — agent notes

Contributor-oriented build and design invariants for the public jModGuard repo.
Deploy topology for Bunny: `docs/DEPLOY-BUNNY.md`. Upgrades: `docs/UPGRADE.md`.

## Incremental overhaul

The first compatible foundation is integrated here. See `docs/OVERHAUL.md` for scope.
The workspace adds `crates/runtime` (shared process primitives) and `crates/conductor`
(independent control process). Do not import the broad `jmodguard` application crate,
Serenity or bot/API state into those crates. The existing bot/API feature/state/config
invariants below describe current code; replace them deliberately as their overhaul
slice is implemented, with corresponding regression coverage.

The conductor currently reports liveness/version but readiness remains 503. Do not
expose operational routes until owner authorization and durable control state exist.
No live provider testing or deployment is part of the current local implementation.
Bot/API configuration remains available. The current baseline requires a fresh database
or the recognized `jmodguard-core-1` family; see `docs/DATABASE-BASELINE.md`. Preserve
family/version checks before bootstrap writes. Never adopt an unmarked old database.

Routine collection of message text/attachments and member/user activity is retired.
See `docs/LOGGING-DATA.md`. Historical archive tables and helpers are removed.
Basic events report to Discord using supplied fields, with bounded delivery and no
history reconstruction. General message caching stays disabled. Keep selected moderation
records and moderation-plan summaries; routine plan/dry-run/log/delete observations do
not create durable audit rows. Internal retention fields remain transitional with no
collection effect. Unsupported databases are refused without changing application rows.

Bot and API construct distinct runtime state: `state::BotState` and `web::state::ApiState`.
Keep OAuth/display caches in `web`; keep automod, compiled guild configs, cooldowns,
log-delivery limits and slash synchronization in bot state. The API reads configuration
from the DB directly and maintains only its version observation/read floor.
`BotState` accepts `BotProcessConfig` (no dashboard fields); `ApiState` accepts
`ApiProcessConfig`. Keep the full loader adapter private. Bot parsing/environment
application must not retain OAuth config; API loading must retain its CORS/public-origin
checks. Both config types still need bot tokens for their current Discord operations.
Common bot/log settings, DB privileges and package dependencies remain transitional.
Process `db_open_options()` fixes the migration role: bot true, API false. Tracing takes
only `LogConfig`. See `docs/ARCHITECTURE.md`; this is not an OS secret boundary.

The action queue is an inactive data-layer foundation; see `docs/BOT-ACTIONS.md`.
Do not add runtime admission, workers or cleanup scheduling until enrollment, fresh
permission checks and dispatch/outcome fencing are complete. Keep claim identity checks,
monotonic replay fencing, bounded cleanup and direct log-config supersession atomic.
Queue history is operational metadata, separate from retained moderation audit records.
The service-boundary guard rejects runtime queue consumers in this slice.

The bot records its own membership observations; see `docs/ENROLLMENTS.md`. They now
bind inactive queue storage, the nickname outbox and API configuration scope.
Positive repair must stay bounded, use the existing bot HTTP/identity, never fetch a
member roster or infer removal from omissions/errors. Missing/outage Gateway context
cannot retire an observation. Healthy matching joins write no rows/receipts; resuming a
dormant request in that exact membership is a mutation with recovery receipts. No
configuration version changes on observations.
Only BotProcessConfig loads enrollment repair controls; API ignores their TOML/env values.

Queue membership changes invalidate old work atomically. Preserve delayed receipt creation
for matching/stale observation no-ops. Future dashboard queue callers must use captured
`*_enrolled_*` scope helpers, not guild-ID helpers that resolve membership at execution.
Keep old uncertain outcomes separate from newer membership config writes. No queue HTTP
admission/worker is enabled; full Discord permission/membership checks remain prerequisites.

The API uses captured configuration operations; see `docs/CONFIGURATION-SCOPES.md`.
Require fresh Discord permission then primary active membership before guild access.
Loaded configuration supplies a membership token; require it for guild configuration edits
and separately for a selected copy source. Never silently refresh/rebind an old browser edit.
Keep request/target and copy source/destination checks in the mutation transaction; scope
objects confer no user/owner permission. Global targets through guild paths retain the guild
check; explicit guild-0 routes remain independently owner-only. Preserve settings/read floors,
nickname/checksum/version and validate-before-replace semantics. Empty captured settings
patches verify using a primary read without mutation receipts. Direct unban/log-channel
execution, complete tenant isolation and worker coordination remain separate work.

## Commands

```bash
cargo test --locked --workspace
cargo test --locked --no-default-features --lib
cargo clippy --locked --bins -- -D warnings
cargo clippy --locked --bin jmodguard-bot --no-default-features -- -D warnings
cargo clippy --locked --bin jmodguard-api --no-default-features --features web -- -D warnings
cargo build --locked --bin jmodguard-bot --no-default-features
cargo build --locked --bin jmodguard-api
cargo build --locked --bin jmodguard-api --no-default-features --features web
node --test scripts/frontend-state-test.mjs
node --test scripts/frontend-warmup-test.mjs
node --test scripts/publish-frontend-test.mjs
python3 scripts/frontend-release-test.py
python3 .buildkite/publish-image-test.py
cargo clippy --locked -p jmodguard-runtime -p jmodguard-conductor --all-targets -- -D warnings
cargo build --locked -p jmodguard-conductor
python3 scripts/check-service-boundaries.py
# or: ./scripts/smoke-dual-process.sh
```

Local dual-process:

```bash
cargo run --bin jmodguard-bot --no-default-features   # migrates DB; Discord gateway
cargo run --bin jmodguard-api                         # waits ~30s locally / ~6m remote for bot migrate, then serves HTTP
```

## Version pins (verified)

- Rust **1.96.0** — `rust-toolchain.toml`, the Buildkite agents, and
  `Dockerfile.{bot,api,conductor}` (`rust:1.96-bookworm`) must stay aligned. `Cargo.toml` `rust-version = "1.96"`. Do not float the agent toolchain on `stable`. CI and image publishing use
  Buildkite; see `docs/BUILDKITE.md`.
- serenity **0.12.5** — see `docs/RESEARCH.md` for the breaking 0.12.5 API
  changes (EventHandler signatures, `!Send` CacheRef, Timestamp newtype,
  `CommandInteraction` rename, `GuildId::set_commands`).
- libsql 0.9.30 (`core`, `remote`; remote HTTPS uses in-tree rustls 0.23, not
  libsql's `tls`/`hyper-rustls 0.25` stack; `libsql-sqlite3-parser` is
  git-patched from branch `vendor/libsql-sqlite3-parser-0.13.1` (rev
  `f2391d6`) for GHSA-8m95-fffc-h4c5; CI/Docker `cargo fetch` uses
  `GITHUB_TOKEN`),
  axum 0.8, tower-sessions 0.14, reqwest 0.13, edition 2024.

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
  transaction as the write); bot call sites still call `app.invalidate_configs()`
  for same-process freshness. API writers need no compiled-cache invalidation; DB writes
  advance their read floor. Both bot and API poll `config_version` for
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
  `BotState::new` primes both version baselines; `ApiState::new` primes its config
  observation. The first poll has no unobserved window (never use a “skip first
  observation” sentinel). Serialize API observation/primary reconciliation and preserve
  the DB write-generation check against concurrent local writes.
- Runtime moderation writers (events/warns/bans) must **not** bump
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
  `https` `web.base_url` is the public CDN hostname. CORS validation
  runs through `ApiProcessConfig::load` only. A public bind (`0.0.0.0`) must also pass
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
  ApiState primary-version prime, router build); `/api` returns JSON 503
  `{"error":"not ready"}` and `Retry-After`, and `/login` / `/oauth` return
  HTML 503 that refreshes in place. Do not put session middleware on
  `/health`. If the env var is set, bind failure is fatal.
- Discord snowflake IDs on the WebUI wire are **strings** (never JS `Number`).
  Audit-event `data` blobs stringify integers above JS `Number.MAX_SAFE_INTEGER`
  at write and on read (legacy rows). `WarnRow` / `BanRow` /
  `EventRow` / `BadwordView` snowflake fields use the same string serde.
- Bot nickname: WebUI empty field, `!setname` with no args / `clear` / `-`, and
  `/setname` with the optional `name` omitted all clear the Discord nickname and
  persist `bot_name=""`. Settings and a nickname outbox row commit atomically with a
  random request ID, captured nickname and current membership ID. Only the bot drains
  it, checking the stored request before/after a bounded own-member Discord read.
  Acknowledgment/retry/failure must match the complete request and active membership.
  Never adopt old or unbound jobs on rejoin. A fresh save or explicit Re-sync submits
  replacement work; missing enrollment leaves it dormant. Matching observations may
  resume only same-membership dormant work, preserving terminal errors and healthy
  no-op receipt behavior. Retirement/replacement invalidates jobs atomically; missing
  Gateway context must not mutate jobs through a separate guild-only lifecycle helper.
  No initial nickname edit is created on join. Nicknames remain guild-only. Final
  DB checks cannot recall a Discord request already sent or coordinate multiple bot
  replicas; writer leases and queued administrative dispatch remain later work.
- `guild_settings` rows are **partial overrides** (field-level merge: guild →
  global → config terminal defaults from `bot.default_prefix` /
  `bot.retention_days`). Joining a server does **not** insert a settings row.
  `!prefix` / `!setname` / WebUI settings patch only the changed fields.
  WebUI/API `null` on a field clears that override (inherit parent); omitted
  fields are left unchanged (`de_settings_patch_field` — default Option nesting
  would treat JSON `null` as omitted). Empty prefix in the WebUI
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
- The historical retention field remains internal, with no live collection or purge
  task and no UI/API control. `bot.retention_days` / `bot.default_prefix` are synced
  into durable `meta.settings_terminal_defaults` on bot migrate-open (bot is
  authority) and read from that meta row on every settings resolve (not cached
  at process open) as the final fallback when neither guild nor global sets a
  field — shared by `get_settings` / config cache so bot and
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
src/config.rs           typed bot/API config loaders + shared env/DB selection
src/db.rs               local/remote libSQL pool + public data-layer facade
src/db/bootstrap.rs     migrations/startup fencing/versioning/settings heal
src/db/configuration.rs config CRUD + resolved snapshots
src/db/settings.rs      inherited settings reads/writes
src/db/nickname_jobs.rs durable Discord nickname reconciliation jobs
src/db/bot_actions.rs   inactive action queue storage
src/bot_actions.rs      typed action payloads and storage limits
src/db/moderation.rs    selected moderation records and statistics
src/db/tests.rs         database unit and regression tests
src/db_compat.rs        synchronous facade + remote deadlines/rollback
src/remote_https.rs     rustls 0.23 HTTPS connector for remote libSQL (hyper 0.14)
src/discord.rs          Discord REST helpers (nickname retries)
src/nickname_worker.rs  membership/request-checked nickname dispatch
crates/runtime/src/health.rs process-local GET/HEAD /health listener (bot; API bind until axum)
src/defaults.rs         seeded rules/commands/templates
src/types.rs            serde models (RuleDef/CommandDef/Behavior/PlanStep)
src/state.rs            BotState + compiled config cache + version poll
src/web/state.rs        ApiState + config read observation
src/web/oauth.rs        dashboard OAuth client and identity models
src/automod.rs          rule engine + action executor
src/handlers.rs         gateway events
src/commands/           engine, behaviors, plans
src/web/                axum API + OAuth (feature web; embed-ui serves frontend/ locally)
frontend/               admin SPA (Bunny Storage/CDN via private ops runners; local: embedded in API)
migrations/             0001_init through 0009_nickname_enrollments
Dockerfile.bot / Dockerfile.api / Dockerfile.conductor
docs/                   SETUP, UPGRADE, ARCHITECTURE, DEPLOY-BUNNY, …
```
