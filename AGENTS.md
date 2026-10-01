# jModGuard — agent notes

Contributor-oriented build and design invariants for the public jModGuard repo.
Deploy topology for Bunny: `docs/DEPLOY-BUNNY.md`. Public install: `docs/INSTALL.md`.
Upgrades: `docs/UPGRADE.md`.

## Incremental overhaul

The first compatible foundation is integrated here. See `docs/OVERHAUL.md` for scope.
The workspace has independent `crates/bot` and `crates/api` services, shared root
`jmodguard` data/configuration and `crates/runtime` process primitives. Do not import the root data crate, Serenity or bot/API state
into runtime. Root data must not depend on either service; production bot and
API dependency graphs must remain independent. Cross-service tests live in
`crates/integration-tests`; API dev-dependencies also exercise the HTTP-to-worker handoff.
The existing bot/API state/config invariants below describe current code; replace them deliberately as their overhaul
slice is implemented, with corresponding regression coverage.

Owner Administration uses the existing WebUI and API session.
Guild dashboard View as… is a read-only preview
tab on the API. No live provider testing or deployment is part of the current local
implementation.
Bot/API configuration remains available. The current baseline requires a fresh database
or the recognized `jmodguard-core-1` family; see `docs/DATABASE-BASELINE.md`. Preserve
family/version checks before bootstrap writes. Never adopt an unmarked old database.

The old general message archive and member/user activity collection are retired.
The separate rolling guild-store snapshot records only eligible human messages for
configured edit/delete logs and expires at most 24 hours after original creation. The bounded member directory is a separate
current-membership and retained-ban feature; see `docs/MEMBERS.md`. It never
records general activity. See `docs/LOGGING-DATA.md`. Historical archive tables
and helpers remain removed. Basic reports use the last retained observation,
with explicit gaps and bounded delivery; they never reconstruct Discord history.
Message-edit reports require Discord's `edited_timestamp`;
embed/GIF/unfurl/reply/flag refreshes and the bot's own messages are not edits. Each
report is that one event. Leave logs may use bounded join timestamps already received
with messages. The separate paced directory importer may list the roster; basic
report delivery never does. Ban/unban display may use bounded recent audit
lookup inside report admission; a gateway ban may use the same bounded lookup
under the report concurrency limit after saving its row. Neither establishes
durable case-source identity.
General Serenity message caching stays disabled. Keep selected moderation
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
checks. Only the bot config retains a Discord bot token; the API retains OAuth credentials.
Common bot/log settings and DB privileges remain transitional. Production dependencies
are separated by package; the API has no Serenity/Gateway/cache dependency.
Process `db_open_options()` fixes the migration role: bot true, API false. Tracing takes
only `LogConfig`. See `docs/ARCHITECTURE.md`; this is not an OS secret boundary.

The API admits typed administrative actions and the bot dispatches them; see
`docs/BOT-ACTIONS.md`. Require fresh permissions, page membership and stable request keys. Keep claim identity checks,
monotonic replay fencing, bounded cleanup and direct log-config supersession atomic.
Queue history is operational metadata, separate from retained moderation audit records.
The service-boundary guard rejects unscoped API queue access and API bot-token dispatch. `action_worker` consumes claims once, rechecks fresh bot
membership and actor permissions, and distinguishes failed validation from uncertain
mutation outcomes. Reuse the bot HTTP client; no additional client/cache. One bot task polls and cleans
bounded batches, with skipped missed ticks. Worker limits/timers are bot-only config. Worker completion must use finish_dispatched_bot_action:
local log changes check expiry inside the primary transaction; known remote unban results
may be recorded late. Failed receipts or shutdown must never automatically resend Discord.
Dashboard cancel may clear unstarted (`pending`/`claimed`) work and stuck log-channel
`running`/`uncertain` rows; it must not treat a running unban as never dispatched.

The bot records its own membership observations; see `docs/ENROLLMENTS.md`. They now
bind queue storage, the nickname outbox and API configuration scope.
Positive repair must stay bounded, use the existing bot HTTP/identity, never fetch a
member roster or infer removal from omissions/errors. The separate Members sync
worker has an explicit paced roster contract; enrollment repair does not use it.
Missing/outage Gateway context
cannot retire an observation. Healthy matching joins write no rows/receipts; resuming a
dormant request in that exact membership is a mutation with recovery receipts. No
configuration version changes on observations.
Only BotProcessConfig loads enrollment repair controls; API ignores their TOML/env values.

Queue membership changes invalidate old work atomically. Preserve delayed receipt creation
for matching/stale observation no-ops. Dashboard queue callers must use captured
`*_enrolled_*` scope helpers, not guild-ID helpers that resolve membership at execution.
Keep old uncertain outcomes separate from newer membership config writes. Admission is
not success: the browser retains keys after lost responses and exposes receipt states.

The API uses captured configuration operations; see `docs/CONFIGURATION-SCOPES.md`.
Require fresh Discord permission then primary active membership before guild access.
Loaded configuration supplies a membership token; require it for guild configuration edits
and separately for a selected copy source. Never silently refresh/rebind an old browser edit.
Keep request/target and copy source/destination checks in the mutation transaction; scope
objects confer no user/owner permission. Global targets through guild paths retain the guild
check; explicit guild-0 routes remain independently owner-only. Preserve settings/read floors,
nickname/checksum/version and validate-before-replace semantics. Empty captured settings
patches verify using a primary read without mutation receipts. Unban/log-channel actions
use the queue. Moderation event/ban/statistics GET routes also require loaded page membership
and captured primary reads; see `docs/MODERATION-READS.md`. Keep the API guard against
unscoped moderation helpers. These reads create no receipts or version changes; the primary
writer lock spans verification and data. Individual configuration GET reads also require page
membership and captured primary snapshots, preserving the config-version read floor. Keep
guild/global layers together, badword inheritance and guild-only log-channel mappings; explicit
global context remains owner-only. The WebUI/config-parts bundle verifies membership then
reads those collections in one SQL statement: Bunny Database interactive transactions cannot
survive a per-table Hrana round-trip loop. Targeted rule/command/template reads must not load the
entire configuration bundle. Complete tenant isolation and replica coordination remain separate work.

The guild-store catalog and `TenantRuntime` now route live isolated tenant data for the
bot and API; see `docs/TENANT-RUNTIME.md` and `docs/GUILD-STORES.md`. Catalog rows remain
nonsecret. Credentials stay in process configuration. Never run core migrations/defaults on a guild
store or adopt a foreign/unmarked schema. `jmodguard-guild-1` is at version 14. Identity,
placement and membership checks share the data transaction. A successful snapshot check
never authorizes a later write.
Stable/Nightly image publication uses Release Please plus scheduled Buildkite Nightlies;
see `docs/RELEASES.md`. Nightly IDs never enter the Release Please baseline and
must not move `latest`.
Library contracts for membership, cases, history, sources and evidence remain as
documented in their TENANT/CASE pages. Runtime routing, selected-case recording,
authorized serving and live guild erasure are described in `docs/TENANT-RUNTIME.md`.
Identity/placement/membership checks must share the data transaction, including empty/no-op
paths. Keep per-membership edit revisions and closure atomic with mappings; never rebind or
reopen a closed scope automatically. Initial binding is trusted lifecycle work, not
permission checking. A successful snapshot check never authorizes a later write. Cloned
backups can retain the same identity/revision. Persisted core `guild_write_fences` must continue blocking writes and erasure after an upgrade. Bunny credential hot-reload remains pending.

Version3 adds inactive selected cases/text evidence; see `docs/TENANT-CASES.md`. Reuse
captured scope checks with data; preserve immutable source/payload retry semantics, bounded
record/page inputs and payload-free replay markers after explicit removal. Closed-scope
cleanup must stay bounded and refuse active memberships. Never infer success, fetch arbitrary
evidence URLs, expire cases by age or expose removal without its backend permission. Selected
moderation outcomes now also write guild-store cases; core warn/ban mirrors remain.

Version4 adds owned append-only case history; see `docs/CASE-HISTORY.md`. Preserve original
facts, stable entry retries and per-case sequence order. Count/byte admission must share the
append transaction; reads page IDs and bound single payloads. Parent removal/closed purge
must remove owned history atomically without foreign-key PRAGMA reliance. Budgets are trusted
explicit inputs; no new runtime defaults, permissions or automatic effective-outcome fold.

Version5 adds explicit application/audit source associations; see `docs/CASE-SOURCES.md`.
Require an already-bound anchor and reject conflicts; never infer matches or merge cases.
Preserve original facts and both source replay keys after removal, including late explicit
links to removed parents. Closed cleanup removes owned keys atomically. Bot-owned
Ban/Unban/Kick now link Discord audit entries from the same process dispatch using that
application identity. Unmarked gateway cases and foreign audit actions other than Ban/Unban stay
application-only; foreign Ban/Unban audit entries use their exact Discord audit ID as source.
Deferred matched pairs stay quarantined for that same 30s/4096 window so a later
same-subject dispatch cannot reuse those halves; expired pending links stay application-only.

Version6 adds the inactive evidence ownership journal; see `docs/CASE-EVIDENCE.md`.
Reserve before object writes, preserve stable identity/byte admission, and never acknowledge
capture settlement while a write can still arrive. Removal cancels/hides metadata immediately;
closed cancellation is batched. Parent purge refuses unacknowledged evidence. Jobs are snapshots,
not leases or provider proof. Authorized serving of available objects is now
implemented; the bot captures proving Discord attachments for selected image-spam
enforcement through tenant runtime. The API serves authorized journal-available objects
and does not perform that Discord re-fetch.
Private evidence and operator-managed backup resources must remain separate.

The opt-in Unix local evidence backend uses real bounded streaming and durable receipts;
see `docs/LOCAL-EVIDENCE.md`. Hold its stable OS resource lock through I/O and SQL settlement,
even if the async waiter is cancelled. Reload exact journal identity, never accept raw object
keys or retry an existing reservation's source. Keep the evidence root private/dedicated;
only verified file absence can acknowledge cleanup. Local advisory locks require cooperating
writers and trusted parents; they do not establish remote PUT completion or restore fencing.
Bot and API open this backend through tenant runtime for authorized serving of available
objects. The bot captures proving Discord attachments for selected image-spam enforcement
through tenant runtime; the API does not perform that Discord re-fetch.

The opt-in Bunny evidence adapter uses guild schema7 single-use attempts and durable201
acceptance before readback/settlement; see `docs/BUNNY-EVIDENCE.md`. Preserve immutable scoped
binding, checksum scrubbing and blocked unresolved PUTs. Never use timeout/early404/matching
bytes as acceptance, retry a PUT, follow redirects or accept arbitrary endpoints. Resource
markers do not prove zone privacy; operator checks, provider resolution and runtime authority
remain before activation. Core is at migration 0016. Local HTTP/SQL fixtures
must not be reported as live Bunny proof or measured resource usage.

The attachment and repeated-text rules now use bounded Gateway exact-image and
exact-trimmed-text paths, in test or delete-and-ban mode; see `docs/IMAGE-SPAM.md`.
Never restore the count-only trigger or route matches into the generic punishment
executor. Require an exact common distinct-hash set or exact trimmed text across
channels, preserve original observation time and invalidation/replay guards, and
keep work/metadata bounds with no background task queue. Test reports are Discord
logs only, with no SQL cases. Enforcement rechecks membership/exemptions, deletes
matched messages and bans with truthful separate outcomes, and records selected
case text (hashes/message IDs/trimmed excerpts). The bot captures proving Discord
attachments after a recorded image-spam ban case using the Hasher CDN contract and the
configured evidence backend; the API does not perform that Discord re-fetch. Test mode
and `none` skip it. Failures do not undo delete or ban. Process image budgets belong
only to bot config.
Owner-selected image updates use the existing WebUI/API; see `docs/BUNNY-UPDATES.md`.
Public installation requires no private ops repository. AWS is EC2 plus SQLite or
libSQL, not RDS. See `docs/INSTALL.md`.

## Commands

Local Linux preflight is `bash scripts/ci-preflight.sh` with no `--only`.
Buildkite runs the same gates in selected groups; see `docs/BUILDKITE.md`.
Do not add GitHub Actions CI workflows. Allowed workflow files are the
owner-command Pullfrog workflow (`docs/PULLFROG.md`) and the Socket Security
manifest scan (`.github/workflows/socket.yml`). Socket Security runs on GitHub
Actions; it is not CI, image publication, or a Buildkite job. Set the
repository Actions secret `SOCKET_SECURITY_API_KEY` to a Socket API key before
relying on the scan. A missing secret skips the scan and does not fail the job, and the key is not stored in git.
The scan uploads discovered Cargo and npm manifests outside ignored paths to Socket, including workspace and member `Cargo.toml` files, `Cargo.lock`, and `tools/eslint/package.json` and its lockfile. It may comment on a pull request with the GitHub Actions token.
A new blocking alert can fail this check only when Socket compares a pull request change that touches a supported manifest. Push and full-report scans do not fail for alerts. Socket API errors do not fail this check. Buildkite remains the merge gate.
After enabling the key, let a scan run on `main` before relying on pull request comparisons; without a main baseline, Socket may treat existing dependencies as newly added.

```bash
bash scripts/ci-preflight.sh
# For a PR whose base is not main:
# bash scripts/ci-preflight.sh --base origin/<base-branch>
# Optional local git hooks (same full Buildkite test script; not GitHub Actions).
# Worktree-local core.hooksPath (.githooks); does not chain previous hook dirs:
# bash scripts/install-git-hooks.sh
# bash scripts/install-git-hooks.sh --base origin/<base-branch>
```

That script runs lockfile verification, `cargo fmt --check`,
`bash scripts/dylint-test.sh` and `bash scripts/dylint.sh` (the Trail of Bits
general lints pinned in `dylint.toml`),
whitespace `git diff --check`, the publisher/release contract tests
(`bash .buildkite/publish-image-test.sh`), workspace and
no-default-features API tests, Clippy `-D warnings` on `--all-targets` (plus
API `--no-default-features --all-targets`), service boundaries (`bash scripts/check-service-boundaries.sh`), public-install acceptance
(`bash scripts/public-install-acceptance-test.sh`), the Dependabot config contract
(`bash scripts/dependabot-config-test.sh`), host ESLint for the browser pages
(`bash scripts/require-host-eslint-node.sh`, then `npm ci --ignore-scripts --prefix tools/eslint`,
then `npm audit --prefix tools/eslint --audit-level=high`,
then `bash tools/eslint-web-selftest.sh`; Node `^20.19.0`, `^22.13.0`, or `>=24` per
`tools/eslint/package.json` `engines`),
frontend Node tests,
independent `jmodguard-bot` / `jmodguard-api` builds, then `dual_process_smoke`
with `--include-ignored` (that ignored test launches those binaries; the
workspace test runs only the in-process smoke), the API build without default features,
`cargo deny check bans licenses sources`, and
`cargo audit` (fail-closed when `JMODGUARD_AUDIT_FAIL_CLOSED=1`; pull requests
and ordinary main pushes ignore reported CVE advisories but still fail when
`cargo audit` cannot run). `npm audit --prefix tools/eslint --audit-level=high`
always fails the `frontend` group on a high or critical advisory and on an
advisory-endpoint failure. There is no fail-open switch and no ignore list;
an unfixable transitive advisory is handled by pinning or dropping that package. GitHub-hosted Dependabot Updates dynamic workflows
are not CI; do not add `.github/workflows` for them. Keep **Dependabot on self-hosted runners** disabled. This repository has no GitHub Actions self-hosted
runners; otherwise jobs queue on a `dependabot` label and GitHub cancels
them after 24 hours. Do not register leftover Proxmox GHA runners or label
any runner `dependabot`.
Remote libSQL tests stay in the Buildkite `remote-database` job.

```bash
# or: ./scripts/smoke-dual-process.sh
```

Local dual-process:

```bash
cargo run -p jmodguard-bot   # migrates DB; Discord gateway
cargo run -p jmodguard-api                         # waits ~30s locally / ~6m remote for bot migrate, then serves HTTP
```

## Test quality and regression evidence

Before implementing a fix, identify the accepted behavior, a credible failure,
and an independent expected result. Extend existing cases where useful. For
regressions, demonstrate failure before the fix and success afterward when feasible;
confirm the failure reaches the intended assertion rather than unrelated setup.

Use the smallest reliable test boundary for that failure, adding broader evidence
when the risk crosses components. State which dependencies and processes are real:
two service states in one process do not prove executable startup; a Node VM with a
simulated DOM does not prove browser behavior; local SQLite or HTTP fixtures do not
prove remote libSQL, Discord, or Bunny acceptance. Exercise the actual production
owner rather than having a mock supply the effect the test claims to verify.

Preserve useful unit, integration, static-boundary, and smoke tests. Before removing
or consolidating coverage, identify the exact remaining cases, assertions, and CI
routes, or explain which requirement is obsolete. Shared or parameterized cases can
protect several entry points; neither test counts nor an E2E-only policy is the goal.
Follow the existing preflight and relevant separate remote-database gates above;
report unrun checks and simulated boundaries explicitly. These principles do not
authorize live provider calls or require a Codex skill to contribute.

## Version pins (verified)

- Dylint **6.1.0** — `dylint.toml` pins selected `examples/general` lints at tag `v6.1.0`. `scripts/dylint.sh` installs matching `cargo-dylint` and `dylint-link` from source, then runs `cargo check --all-targets` for the affected packages selected by Buildkite (the full workspace for local preflight and global changes) under **nightly-2026-08-20** (the lint libraries' toolchain at that tag). Clippy, `cargo test`, and image builds still use **1.96.0** from `rust-toolchain.toml`.
- Rust **1.96.0** — `rust-toolchain.toml`, the Buildkite agents, and
  `Dockerfile.{bot,api}` (`rust:1.96-bookworm` linux/amd64 digest)
  must stay aligned. `Cargo.toml` `rust-version = "1.96"`. Do not float the agent toolchain on `stable`. CI and image publishing use
  Buildkite; see `docs/BUILDKITE.md`. Do not add GitHub Actions CI workflows.
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
- Select service packages explicitly: `-p jmodguard-bot` or `-p jmodguard-api`.
  API default `embed-ui` includes the root `frontend/` for local use; API
  `--no-default-features` builds JSON/OAuth for Bunny. There is no root `web` feature.
  Keep embedding paths relative to the new API source. Bot never needs dashboard features.
  Bare Cargo commands select all workspace packages; production builds must use `-p`.
- Sessions use `MemoryStore` intentionally for a **single** API process; lost on
  restart. Cookie name is `jmodguard_session` (Magic Container sticky
  sessions must use that name). Shared session store is a future
  multi-replica concern. Discord
  OAuth CSRF (`state`) and `return_to` are browser cookies so a restart
  between `/login` and `/oauth/callback` does not fail the handshake.
  Those cookies are host-only on `web.base_url`; a split-origin SPA still
  sends the user through that hostname for login and callback.
  Successful `/oauth/callback` rotates the session id. API JSON responses
  set `Cache-Control: private, no-store`, `CDN-Cache-Control: no-store`,
  and `Vary: Cookie`. The SPA bootstraps with `POST /api/me` so a CDN that
  cached `GET /api/me` cannot keep a signed-in workspace without a live
  session. It loads guild configuration with `POST /api/guilds/{id}/config`
  so a cached GET settings document cannot populate a writable UI. It reads
  bot-action receipts with `POST /api/guilds/{id}/actions` and one log-channel
  mapping with `POST /api/guilds/{id}/logchannels/{event}` so a cached GET
  cannot leave a finished log-channel save marked queued. Dashboard
  counts, the audit log, bans, case lists, a case, and its evidence list
  use the same POST read. Case history stays GET because POST on that
  path adds a note. Discord
  snowflake fields on the WebUI (log channels, rule/command allowlists,
  case-removal roles, plan role steps) stay decimal strings — never JS
  `Number`. Bunny API-path edge rules must also **Override Cache Time = 0**
  (plus Bypass Perma-Cache); Bypass Perma-Cache alone is not enough.
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
  replicas; writer leases remain later work.
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
crates/bot/src/main.rs
crates/api/src/main.rs
src/config.rs           typed bot/API config loaders + shared env/DB selection
src/db.rs               local/remote libSQL pool + public data-layer facade
src/db/bootstrap.rs     migrations/startup fencing/versioning/settings heal
src/db/configuration.rs config CRUD + resolved snapshots
src/db/settings.rs      inherited settings reads/writes
src/db/nickname_jobs.rs durable Discord nickname reconciliation jobs
src/db/bot_actions.rs   action queue storage
src/bot_actions.rs      typed action payloads and storage limits
crates/bot/src/action_worker.rs    bot dispatch library (worker parameters in bot_actions)
src/db/moderation.rs    selected moderation records and statistics
src/db/tests.rs         database unit and regression tests
src/db_compat.rs        synchronous facade + remote deadlines/rollback
src/remote_https.rs     rustls 0.23 HTTPS connector for remote libSQL (hyper 0.14)
crates/bot/src/discord.rs          Discord REST helpers (nickname retries)
crates/bot/src/nickname_worker.rs  membership/request-checked nickname dispatch
crates/runtime/src/health.rs process-local GET/HEAD /health listener (bot; API bind until axum)
src/defaults.rs         seeded rules/commands/templates
src/types.rs            serde models (RuleDef/CommandDef/Behavior/PlanStep)
crates/bot/src/state.rs            BotState + compiled config cache + version poll
crates/api/src/web/state.rs        ApiState + config read observation
crates/api/src/web/oauth.rs        dashboard OAuth client and identity models
crates/bot/src/automod.rs          rule engine + action executor
crates/bot/src/handlers.rs         gateway events
crates/bot/src/commands/           engine, behaviors, plans
crates/api/src/web/                axum API + OAuth (API embed-ui serves root frontend/ locally)
frontend/               admin SPA (Bunny Storage/CDN from public artifacts; local: embedded in API; daisyUI/Headless from runtime CDN, not git)
migrations/             0001_init through 0016_member_sync_pacing
Dockerfile.bot / Dockerfile.api
docs/                   SETUP, INSTALL, DEPLOY-BUNNY, DEPLOY-AWS, COST, UPGRADE, ARCHITECTURE, …
```
