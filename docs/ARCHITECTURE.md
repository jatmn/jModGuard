# Architecture

jModGuard runs as **two processes** that share one database:

| Process | Role |
|---------|------|
| `jmodguard-bot` | Discord gateway, automod, commands, migrations (`migrate=true`) |
| `jmodguard-api` | OAuth WebUI + JSON API (`migrate=false`; requires feature `web`) |

Neither process calls the other. The **database is the source of truth**.

```
jmodguard-bot  ──writes──►  SQLite / libSQL  ◄──writes──  jmodguard-api
       │                           │                           │
       └──── polls config_version ─┴── both processes poll ────┘
```

For a single-host install, both processes open the same local SQLite file. For
distributed deployment, both connect to one remote libSQL database. Bunny
Database is the documented first-class remote provider; its environment
mapping and generic protocol path have automated coverage, while production
TLS/authentication is verified during deployment acceptance. Local SQLite
uses WAL mode, so its file must not be placed on a network filesystem or opened
by processes on different hosts.

## Package boundaries

- `jmodguard` is the shared configuration/model/database library. It has neither the
  bot nor dashboard stack. Root config uses the existing `url` and `http` types directly.
- `jmodguard-bot` owns Discord Gateway/REST, caches, moderation, commands and workers.
  Build it with `cargo build --locked -p jmodguard-bot`.
- `jmodguard-api` owns HTTP/OAuth, sessions and dashboard state. Its production graph
  contains no Serenity or WebSocket stack. Default `embed-ui` embeds root `frontend/`;
  Bunny builds `cargo build --locked -p jmodguard-api --no-default-features` for JSON/OAuth.
- Runtime retains its independent dependency graph. Integration tests explicitly
  join service libraries; the API's dev-only bot dependency retains HTTP-to-worker tests.
- Bare workspace commands include every package. Select production builds with `-p`;
  the former root `web` feature no longer exists. Binary names, config and wire contracts
  are unchanged. Dependency separation does not provide SQL privilege or tenant isolation.

## Runtime state ownership

The processes own separate state types in their respective service packages:

- `BotState` owns compiled guild configuration, automod working state, command cooldowns,
  Discord log-delivery limits and slash-command synchronization. It does not construct
  dashboard OAuth clients or user/guild display caches. Nickname reconciliation reuses
  the Gateway client's REST handle and caches bot identity in its worker; it does not
  allocate a second REST client or fetch identity when no nickname jobs are pending.
- `ApiState` owns the OAuth client, dashboard identity/display caches and a serialized
  database-version observation. Configuration endpoints already read the database
  directly; the API no longer constructs the bot's compiled configuration cache,
  moderation engine, cooldown maps, log-delivery semaphore or slash-sync state.

Unban and log-channel changes use [queued administrative actions](BOT-ACTIONS.md). The
API returns receipts; the bot validates and dispatches with its existing HTTP client.
`ApiState` has no bot-token client. One bot task polls and cleans bounded batches;
independent bot/API packages now enforce production dependency boundaries. External
versions are unchanged; resource savings are not measured.

## Process configuration

`BotState` accepts `BotProcessConfig`; `ApiState` accepts `ApiProcessConfig`. The bot
configuration has no `web` field. Its loader ignores dashboard fields in TOML and does
not apply web/OAuth environment overrides. Malformed dashboard field values therefore
cannot block the bot, although the shared file must still have valid TOML syntax.
The API loader continues to parse and validate dashboard configuration, including
credentialed CORS and public HTTPS origin checks, before opening its listener.

The file path (`JMODGUARD_CONFIG` or `config.toml`), environment names, environment-over-file
precedence, atomic remote DB credential selection and common bot/log settings remain.
API configuration has no Discord bot-token field and ignores `[discord]`,
`[action_worker]` and their environment overrides. This narrows retained typed
configuration; it is not an OS secret boundary. A process can still read environment
variables or the raw contents of a shared file made available to it. Assign OAuth
secrets only to the API and the bot token only to the bot in a deployment that needs
process-level secret separation. Shared action storage limits and bot-only scheduling
controls are documented in [BOT-ACTIONS.md](BOT-ACTIONS.md#runtime-controls).

The process config builds DB options without a caller-supplied migration flag: bot
options initialize the database, API options wait for completed initialization. This
encodes startup ownership, not DB privilege isolation. The `jmodguard-core-1` schema,
shared database access and existing bootstrap/defaults behavior are unchanged.

## Bot membership observations

The bot's [membership observer](ENROLLMENTS.md) records Gateway joins/departures with
known context and runs bounded, configurable positive REST repair. It keeps one SQL row
per guild and one pagination cursor, reuses the existing HTTP client, and does no
per-message work. API configuration has no enrollment-repair fields. Observation state
scopes queued actions, nickname reconciliation and API configuration access.
Action dispatch additionally checks fresh bot membership and actor permissions.

## Config cache invalidation

Config writers bump a durable `meta.config_version` row and replace its
`meta.config_revision` token **inside the DB layer**
(same transaction as the write). Runtime moderation writers (events,
warns, bans) do **not** bump it.

Both processes poll `bot.config_poll_secs` (default 5). The bot invalidates its compiled
guild cache; the API updates its database read observation without a compiled cache.
Local config writes continue to advance the DB read floor immediately. Command registry writers
also bump `meta.slash_registry_version` and replace its revision token; the bot polls that independently each
tick and re-syncs Discord slash commands only when it advances (not on every
settings/rules/template write). `BotState::new` primes both baselines and
`ApiState::new` primes its config observation from the primary. Neither skips its first
observation. API primary reconciliation checks the local write generation so an older
snapshot cannot replace the read floor after a concurrent API mutation.

Expect up to one poll interval of lag locally after a cross-process config
write; remote deployments can additionally wait for provider replica convergence.
Slash-command resync reads its registry version and command rows in one
transaction and retries when that snapshot has not reached the target version.
If a lower counter is observed, jModGuard confirms it through a deferred primary
snapshot before rebaselining caches. `BEGIN DEFERRED` is not a read-only statement,
so remote libSQL still routes it to the primary, and the snapshot does not take the
writer lock. This recovers from Bunny's documented failover-loss window without
accepting a stale replica as authoritative and without queueing ahead of bot writes.
Non-reusable revision tokens also distinguish counters that
return to the same numeric value after rollback (the ABA case). Mutating read-modify-write paths also use `IMMEDIATE`
transactions so they begin on the primary and serialize with other writers. Web, API,
and status reads use the deferred snapshot or a read-only transaction instead, so they
cannot take that lock. API writes keep a short busy and operation timeout and fail
retriably rather than waiting out a bot write.

Nickname changes commit a durable outbox row in the settings transaction, capturing the
nickname, random request ID and active membership. The bot drains primary-routed due work,
checks the exact request, reads its own current Discord member record with a deadline, and
checks the request again before editing. Retry, failure and acknowledgment match immutable
request fields, attempt/generation and membership. Transient failures keep bounded backoff;
permanent failures remain operator-visible. Membership changes invalidate work atomically.
Only a confirmed matching membership can resume its own dormant request. New joins and
migrated unbound jobs require a fresh save or Re-sync; no automatic initial nickname edit
is issued. See [nickname reconciliation](ENROLLMENTS.md#nickname-reconciliation).

These checks cannot recall an already-sent Discord edit or provide exclusive bot-replica
ownership. Configuration API scope is implemented below; writer coordination remains
separate work. Use one bot worker for this implementation.

Captured configuration data operations now retain the original guild membership through
the primary write transaction, including both sides of a copy and the guild context of a
global edit. Empty scoped settings patches verify before reading. These library operations
are used by the API after fresh user permission and primary membership admission; see
[configuration scopes](CONFIGURATION-SCOPES.md) for page/source tokens and stale-edit behavior.
The configuration bundle, individual configuration GET reads and
[moderation reads](MODERATION-READS.md) verify membership alongside their data. Configuration
layers share a primary snapshot and preserve the process read floor; queued actions use
captured membership. The schema is unchanged.

## Guild-store catalog foundation

[Guild-store catalog storage](GUILD-STORES.md) now records opaque identities, immutable
shared/dedicated ownership and initial placements. It is not connected to runtime routing;
current bot/API data still uses one configured database. No provisioning, moves, extra
connections, caches or polling are enabled. The later directory database split must preserve
these identities while adding credential resolution and transactional tenant operations.

The inactive `GuildDataStore` library opens a separate `jmodguard-guild-1` database and
verifies its identity/ownership against the catalog. It can bind/check initial guild
revisions, with the raw pool private and the existing transport/recovery reused. Bot/API
still creates no such handles. Checks are snapshots, not writer leases or later-operation
authorization; cloned stores can retain identical stamps. Actual data access
must combine checks with tenant operations and coordinated writer exclusion.

[Captured tenant log-channel operations](TENANT-LOG-CHANNELS.md) now demonstrate that
same-transaction boundary in the separate store: exact membership and placement accompany
reads/edits, revision checks reject stale edits, and closure removes mappings and denies
later access. Directory log delivery still uses core configuration. Live store routing
and guild erasure are described in [tenant runtime](TENANT-RUNTIME.md).

Selected [case storage](TENANT-CASES.md) now reuses captured tenant scope for immutable
facts, bounded text evidence, source-key deduplication, payload removal and closed-scope
batch cleanup. Bot and API now record selected outcomes and serve authorized case/evidence
administration; see [tenant runtime](TENANT-RUNTIME.md). The bot captures proving
Discord attachments for selected image-spam enforcement through that runtime; the API
serves authorized journal-available objects and does not perform that Discord re-fetch.

## Migrations

The [fresh database baseline](DATABASE-BASELINE.md) remains `jmodguard-core-1`,
migrations 0001–0016. Migration 0006 adds action storage and its replay floor.
Migration 0007 adds bot membership observations. Migration 0008 binds queue
storage and captured queue scope to those observations. Migration 0009 binds nickname
requests without adopting earlier work. Migration 0010 adds the guild-store catalog.
Migration 0011 adds live-erasure jobs and the historical `backup_cleanup_intents` table,
which no longer receives application writes. Migration 0012 adds persisted guild-write
fences; existing fences remain enforced for database safety. Migration 0013 stores per-event
log exclusions. Migration 0014 stores known ban display names. Migration 0015
stores component-update coordination. Migration 0016 adds the per-bot shared
[member-sync request gate](MEMBERS.md), preserving minimum request intervals and
rate-limit delays across guilds and worker restarts. Bot/API startup
opens [tenant runtime](TENANT-RUNTIME.md) against that catalog. Unban/log-channel WebUI
operations now use API admission and bot dispatch.

- Bot applies migrations, atomically seeds globals plus their config/command
  versions, heals settings, then publishes a
  bootstrap-complete marker.
- API opens with `migrate=false`. Until the schema, terminal-defaults meta, and
  bootstrap-complete marker exist—or while SQLite is transiently locked by bot
  startup—it retries for about 30 seconds locally. On a remote database it also
  retries transient connect/timeout errors for about six minutes. Credential
  and other non-retryable open failures still fail immediately.
- Always start (or migrate with) the bot before the API on a new or upgraded DB.

## WebUI auth (summary)

- Discord OAuth2; sessions in `MemoryStore` (single API process; cookie
  `jmodguard_session`; lost on API restart).
- Guild admins need Discord `MANAGE_GUILD`.
- Global edits (`guild_id=0` or body `scope: "global"`) require `bot.owner_id`.
  That check uses the session user id and does not fetch the guild list.
- Guild-scoped authorization always re-fetches Discord guild membership
  (`authorize_manage_guild`); the short guild-list cache is display-only
  (`/api/me`) and may serve the last good list while Discord is rate-limiting.

## Source layout

```
src/lib.rs                 shared library
crates/bot/src/main.rs
crates/api/src/main.rs
crates/bot/src/state.rs               BotState, compiled config cache and slash-version polling
crates/api/src/web/state.rs           ApiState and config-version observation
crates/api/src/web/oauth.rs           dashboard OAuth client and identity models
crates/runtime/src/health.rs  GET/HEAD /health liveness (bot listener; API bind until axum)
frontend/                  admin SPA (Bunny CDN; local embed-ui; daisyUI/Headless loaded at runtime, not vendored)
migrations/                0001_init through 0016_member_sync_pacing
Dockerfile.bot
Dockerfile.api
```

## Related docs

- [SETUP.md](SETUP.md) — first install
- [UPGRADE.md](UPGRADE.md) — moving to a new version
- [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md) — Bunny CDN admin UI + private containers
- [CONFIGURATION.md](CONFIGURATION.md) — config.toml and backups
