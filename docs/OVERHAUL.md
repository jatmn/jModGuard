# Overhaul implementation status

The overhaul lands in independently reviewed changes. The service foundation is followed
by routine logging/data minimization. The existing bot/API and optional inactive conductor
remain independently buildable. A fresh schema baseline now removes the archive tables
and helpers. See [logging/data behavior](LOGGING-DATA.md) and the explicit
[database setup boundary](DATABASE-BASELINE.md).

## Foundation compatibility

- The foundation preserved the database and WebUI contracts. The logging wave below
  intentionally removed routine-history UI/API surfaces. The subsequent schema baseline
  now requires an empty database or the recognized `jmodguard-core-1` family.
- Bot/API binary names, Cargo feature flags and health/shutdown module paths remain
  available. Existing container definitions include the new workspace directories.
  Bare Cargo commands now select all workspace packages; production builds select a
  service explicitly, and CI uses `--workspace` to include all tests.
- Bot/API publishing tracks changes to the extracted runtime. Existing image tags and
  deployment approval flow remain in place; the conductor is not auto-published or
  required for startup.
- The conductor has no operational API and must not be deployed as an update/recovery
  service. Its addition does not establish production tenant isolation or resource usage.

The target has three Rust services (Discord bot, guild API and installation conductor)
and a static WebUI. Generic setup, releases and recovery tooling will live in this
repository; private installation credentials stay with the operator. This schema baseline
does not import old databases. The fuller tenant/case model remains a later change.

## Implemented: service foundation

- The repository is a Cargo workspace. `jmodguard` still contains the existing bot/API
  data/model/configuration library after the package split below; `jmodguard-runtime`
  contains shared shutdown and health utilities;
  `jmodguard-conductor` is an independently built control process.
- Existing bot/API entry points use the extracted utilities through the existing public
  module paths. Their startup, health and shutdown behavior remains covered by tests.
- The conductor has separate process configuration, a single-thread async runtime,
  stdout logging, liveness/version endpoints and a configurable shutdown grace.
  Operational readiness remains unavailable until the control system is configured
  and implemented. No update, backup, restore or login endpoint is enabled yet.
- CI checks that the conductor/runtime dependency trees do not include the application
  crate, bot/API service crates or Serenity. It tests the actual conductor process with
  local HTTP and process signals. Existing frontend and application checks remain.
- All three Docker build definitions understand the workspace. The conductor image is
  a development artifact; it is not included in automated release publication yet.

See [the conductor developer guide](CONDUCTOR.md) for commands and the current API.
This is application source, not a separate proof project. No prototype's fixture
schema, hardcoded test budgets, synthetic login or same-host recovery lock is being
presented as a completed production control system.

## Implemented: routine logging without SQL history

Routine message/member/user collection, roster backfill, tracking state and periodic
SQL message cleanup are removed. Basic edit/delete/join/leave reports use delivered
Discord fields, with no reconstructed transcript. Discord delivery is bounded by
configurable concurrency/deadline, suppresses mentions/previews and has no waiting backlog.
The Members view/endpoint and retention setting are removed; stats show moderation
records only. Selected moderation records and moderation-plan summaries remain durable.
The subsequent fresh baseline removes historical schema/helpers; unsupported existing
databases are refused without rewriting their application rows.
Deploy matching frontend/API artifacts and reload old pages that submit retention settings.
This does not implement new image detection, the case/evidence model or tenant routing.

## Implemented: fresh schema baseline

The initial migration omits chat, member, user and mute archive tables and writes the
schema-family marker atomically with the initial tables/version. Both processes reject
unmarked, foreign, invalid or newer schemas before changing bootstrap state. The bot
can resume initialization within this lineage; the API waits for completed bootstrap.
Warnings, bans, selected events, configuration and nickname reconciliation remain usable.
No old deployment conversion or data deletion is performed.

## Implemented: separate runtime state

Bot and API use distinct state types. The API no longer allocates the moderation engine,
compiled guild configuration cache, command cooldowns, log-delivery limits or slash-sync
state. The bot no longer allocates an OAuth HTTP client or dashboard display caches.
OAuth code now lives in the API package. API configuration reads remain direct database reads,
with primary-confirmed version observation and protection against concurrent local writes.

State separation introduced no schema/dependency change and does not establish measured
memory savings. Administrative activation below removes the API's bot token; the later
package split now confines each service's implementation and production dependencies. The optional
conductor remains inactive. See [runtime ownership](ARCHITECTURE.md#runtime-state-ownership).

## Implemented: typed process configuration

Runtime constructors now require `BotProcessConfig` or `ApiProcessConfig`. The bot has
no dashboard configuration field and ignores dashboard TOML values/environment overrides;
the API still loads and validates them. Common settings and environment/DB credential
precedence remain unchanged. DB-open options encode each process's existing migration
role; tracing consumes only logging settings.

The API now omits its bot token following action activation below. The private common
loader adapter and shared bot/log settings remain implementation scaffolding. This adds
no queue, schema or dependency change, and does not isolate OS environment/filesystem
secrets or database privileges. See [process configuration](ARCHITECTURE.md#process-configuration).

## Implemented: durable action storage

Typed unban/log-channel requests now have transactional deduplication, capacity/target
exclusion, exclusive claims, claim-bound local completion and bounded resolved-history
cleanup with a durable replay cutoff. Direct log-channel writes cancel superseded queued
settings in the same transaction. Migration 0006 extends `jmodguard-core-1`.

This initially landed as library groundwork; membership, dispatch and runtime integration
follow below. No dependency was added. See [the action contract](BOT-ACTIONS.md).

## Implemented: bot membership observations

The bot records its own per-guild membership separately from user permissions. Gateway
observations and bounded positive REST repair preserve reconnect identity and reject
older/retired joins. Repair reuses the Gateway HTTP client, stores no member roster and
has bot-only configurable page/deadline/pause controls, including a disable switch.
Migration 0007 adds only this observation table. See [membership behavior](ENROLLMENTS.md).

This observation slice did not activate queue or nickname/API enrollment fencing.
Subsequent nickname binding and administrative activation are described below.
No data erasure, new dependency or tenant-isolation claim is part of this wave.

## Implemented: membership-bound action storage

New administrative queue rows capture the active bot enrollment identity and join time.
Retirement/replacement invalidates pending/claimed work and preserves running work as
uncertain atomically. Admission, status, cancellation and claim completion require the
current membership; captured-scope entry points also reject changes since authorization.
Migration 0008 does not adopt old unbound jobs. Existing reviewed log-write semantics,
replay cutoff and no-write membership observations are preserved.

Storage binding landed before the fresh dispatch checks and runtime activation below.
Nickname enrollment binding is now implemented in its own path below. See [the queue contract](BOT-ACTIONS.md#membership-bound-storage).

## Implemented: nickname request and membership checks

The active nickname outbox captures a random request ID, nickname and observed membership
in the settings transaction. Its worker checks the primary request before and after a
bounded Discord own-member read. Stale attempt/completion results cannot update or remove
replacement work. Membership transitions invalidate nickname work in the same transaction;
matching observations resume only their own dormant request and preserve terminal failures.
Healthy observation no-ops still create no mutation receipt. The previous unscoped Gateway
nickname lifecycle calls are removed.

Migration 0009 leaves old jobs dormant and unbound. New joins do not replay them or issue
an initial nickname edit; save/re-sync after membership is observed. The existing WebUI
explains that recovery path. This uses the current HTTP client, batch/concurrency limits
and retry scheduling, with no dependency additions. It does not enable the administrative
queue, remove API credentials or establish multi-replica exclusion/full tenant isolation.
See [the lifecycle contract and limits](ENROLLMENTS.md#nickname-reconciliation).

## Implemented: captured configuration data operations

The data layer adds settings/copy and configuration CRUD variants that verify captured
membership within their primary transaction. Copies check both endpoints; writes targeting
global configuration from a guild page retain the original guild membership check. Empty
settings patches cannot bypass verification or create recovery receipts. Existing settings,
nickname, checksum, validation and version behavior shares the same implementation.

These operations share the existing mutations without a new schema or dependency. The
API/browser integration below now consumes them; bot configuration callers retain existing helpers.
See [captured configuration scopes](CONFIGURATION-SCOPES.md).

## Implemented: API configuration admission and browser membership

Guild API access now requires fresh Discord user permissions and primary active bot
membership. Configuration snapshots verify the captured membership with the data;
configuration writes verify it transactionally. The browser sends the loaded membership
and retains the selected source for copies. Old pages cannot silently edit a replacement
installation. Retry can restore drafts only within the same membership.

Deploy matching API/frontend builds and reload old pages. Global routes remain owner-only;
global targets through guild paths retain that page's membership. Administrative actions
now use the queue integration below. No schema migration, erasure or full tenant-isolation
guarantee was added by configuration admission. See
[the HTTP and browser contract](CONFIGURATION-SCOPES.md#http-and-browser-contract).

## Implemented: administrative dispatch library

The bot dispatch library consumes queued claims once and checks fresh bot membership and
actor authority before effects. It rechecks primary claim/membership before unban and at
completion; timeouts and ambiguous results never cause automatic replay. A local log-setting
change that expires while waiting for the primary writer is rejected, while a known remote
unban result can be recorded late. Existing atomic receipts, audit and version semantics remain.

The library and mock/remote-SQL tests landed before runtime activation. The worker denies
actively timed-out non-owner/non-administrator members and warns if a reported completion
cannot be persisted because the action was superseded or membership ended. It never resends.
See [the dispatch contract](BOT-ACTIONS.md#dispatch-library). This is not live-provider or
multi-replica certification.

## Implemented: administrative runtime and API credential separation

The API admits typed unban/log-channel actions after fresh permissions and captured page
membership checks, returning HTTP202 receipts. Status/cancellation retain those checks and
filter by current action authority. The frontend saves a request key and membership before
sending, retains it after an unknown response, and requires explicit clearing before a
separate new submission after expiry or membership change. The Actions tab refreshes on
request and permits pending cancellation; it never treats admission as completed moderation.

One bot task sequentially drains bounded batches using the existing HTTP client and real
Gateway identity. A second timer in that task prunes bounded resolved history; missed ticks
are skipped. Validated storage policy is shared by bot/API; worker limits and scheduling
are bot-only. The API removes its bot-token config field, startup requirement and client;
OAuth still handles login and fresh guild permissions. No schema, dependency or new client
is added. Deploy matching bot/API/frontend builds, bot first, and reload old pages.

Local HTTP/browser tests cover replay, stale membership, action-specific permissions,
status/cancellation and invalid/expired keys. An actual API process starts without a bot
token. Storage/worker tests use mock Discord and disposable loopback libSQL, not live
provider operations. One bot and one API remain the supported topology; database write
credentials are still shared. See [runtime controls](BOT-ACTIONS.md#runtime-controls).

## Implemented: independent bot/API packages

Bot and API source now live in `crates/bot` and `crates/api`; the root library contains
shared configuration/model/database code. The API production dependency graph excludes
Serenity and WebSockets; the bot excludes Axum and dashboard sessions. Runtime/conductor
boundaries remain. No external package version changes or new third-party packages are
introduced: shared config directly uses the existing url/http types, and API tests use
existing async-trait instead of Serenity's macro re-export.

Moved tests preserve worker, membership, browser/config and receipt behavior. Cross-service
DB tests live in a dev-only integration package; actual token-free API startup lives beside
its binary. The API's dev-only bot dependency keeps the HTTP-to-worker regression intact.
Only existing shared read-floor/resolve/SQL-param interfaces needed across crates are exposed;
claims and the full config loader adapter remain private. Database privileges are unchanged.

Buildkite and Docker now select packages explicitly. Publication already tracks all crates;
tests verify service-source changes still trigger image publication. Local API embedding uses
the root frontend; Bunny API builds without embedding. Bare Cargo commands include the
whole workspace; old root web-feature commands must switch to documented package commands.
Binary names, environment/TOML, schema, API/browser contracts and one-bot/one-API topology
are unchanged. No live provider or resource measurement is established by these builds.

## Implemented: membership-bound moderation reads

Audit events, ban history and statistics require fresh guild permissions and loaded page
membership, then verify that captured membership alongside data in one primary snapshot.
Empty reads cannot bypass the check; all statistics counts share that snapshot. Shared SQL
preserves filters, limits, ordering and wire values. No schema, dependency, cache or polling
is added. Reads create no mutation receipts or version changes. Matching API/frontend builds
are required. See [the contract and limits](MODERATION-READS.md).

## Implemented: individual configuration read snapshots

Rule, badword, command, template and log-channel GET reads now require loaded page membership
and verify it with data on primary. Guild/global layers share one snapshot; badwords retain
inheritance and log channels stay scope-only. Explicit global context remains owner-only.
Reads preserve the config-version floor without receipts/version changes, and use targeted
section/key queries instead of loading the full bundle. No schema, dependency, cache or
polling is added. Deploy matching API/frontend builds and reload old pages. See
[individual configuration reads](CONFIGURATION-SCOPES.md#individual-configuration-reads).

## Implemented: inactive guild-store catalog

The data layer can register an opaque shared/dedicated store identity and assign an initial
guild placement with a revision. Dedicated ownership is enforced by both transactional
helpers and SQL triggers without foreign-key PRAGMA dependence. Matching retries write no
receipts; actual writes retain lost-commit recovery. Conflicting registrations and placement
changes are refused. Migration0010 starts empty, preserving current records and runtime.
There are no credentials, provider resources, routing caches, extra pools or tasks. Matched
bot/API builds are required. See [the catalog contract](GUILD-STORES.md).

## Implemented: inactive separate guild-store identity

A separate `jmodguard-guild-1` schema and private-pool `GuildDataStore` wrapper verify
expected identity/ownership and install/check initial catalog placement revisions.
Foreign/core/unmarked schemas and conflicting bindings are refused. Matching retries
write nothing; initialization and binding recover lost commit responses on primary.
The wrapper reuses existing transport/pool/receipt code with no new dependency.
Core schema and current bot/API operation are unchanged. See [store contracts](GUILD-STORES.md).

These checks establish neither live routing nor distributed writer exclusion. Actual
membership/placement-scoped data operations must check scope in their own transaction;
restored copies can have identical identity stamps. CI uses a separate empty disposable
libSQL server for each schema's smoke test. No live provider behavior is established.

## Implemented: inactive captured tenant log-channel operations

The separate guild store now binds an initial captured membership and checks it alongside
store identity, placement and log mappings in one primary transaction. Revision checks
reject stale edits; reads/no-op edits write nothing. Closing the exact membership removes
its mappings and blocks later access/rebinding atomically. Version2 adds empty membership
and mapping tables to the verified guild-store lineage; core stays version10. Existing
shared event types bound mapping count, with no message/event payload collection or new
dependency/task. See [the contract and limits](TENANT-LOG-CHANNELS.md).

Current services still use core settings and the action queue. Directory/lifecycle
coordination, full erasure/rejoin, global policy, queue completion/version observation and
runtime authorization/routing remain pending. Closed identities are transitional storage
barriers, not the final purge policy. No live provider proof or distributed writer lease.

## Implemented: inactive selected case and text-evidence storage

Guild schema3 adds stable scoped case IDs, typed action/outcome facts and bounded selected
text evidence. Recording uses stable source keys to deduplicate retries; conflicting facts
fail. Payload removal retains minimal replay keys so the same source cannot recreate it.
After membership closure, bounded cleanup can remove records/markers while stale work stays
denied. Reads and mutations reuse captured primary scope and receipt recovery; no new
library, task or runtime recorder is enabled. Core stays10. See [case contracts](TENANT-CASES.md).

File evidence, richer proof, trusted runtime source matching, permissions, capacity monitoring
and backup-aware lifecycle remain before runtime adoption. History and explicit source
association are implemented below.
No general chat/event archive, automatic age expiry or full-erasure claim is introduced.

## Implemented: inactive append-only case history

Guild schema4 adds ordered notes, outcome/reason corrections and selected policy context
while preserving original case facts. Stable entry IDs make matching retries write-free;
conflicts fail. Explicit per-case count/byte admission shares the primary append transaction.
Single reads bound payload size, pages return references only. SQL triggers remove history
with parent payload removal or closed-case purge and roll back both on failure. No new
library/task/recorder is enabled. See [history contracts](CASE-HISTORY.md).

Trusted runtime source matching, permission/presentation, file evidence and complete
lifecycle/backup cleanup remain pending. Corrections are observations, not inferred Discord
success or automatic rewriting of effective state. Core remains10.

## Implemented: explicit case source association (wave 22)

Guild schema5 adds indexed application-operation/Discord-audit source mappings with original
source backfill. Trusted callers can link a counterpart using an existing anchor and resolve
canonical IDs without payload reads. Original facts/history remain immutable; conflicting
identities fail and no cases are merged. Removed parents retain both replay keys and can
accept a late opaque counterpart without reviving content. Closed-case cleanup atomically
removes owned mappings. See [source contracts and limits](CASE-SOURCES.md).

Current services/core0010 remain unchanged; no new dependency, worker or inference policy.
Runtime source matching and durable reconciliation, permissions, richer/file evidence and
full lifecycle/erasure/routing remain pending. Locally verified storage is not live Discord
correlation proof, a complete guild purge or a provider resource measurement.

## Next implementation slices

1. Extend the captured tenant log-channel pattern across the fresh data model/tenant
   routing: shared/dedicated guild placement,
   scoped case/evidence identities, selected durable audit records and bounded transient
   message context. Correct exact repeated-image matching in the actual event pipeline.
2. Add durable sessions, owner/guild authorization surfaces and effective settings with
   revision handling. Backend authority must enforce every UI permission.
3. Build release manifests/artifacts, the conductor control store and provider adapter,
   then authenticated updates and portable backup/restore using the validated contracts.
4. Complete guild/operator UI, backup scheduling/download/upload/restore, system metrics,
   setup and provider documentation. Replace the old private-ops workflow before launch.

Each slice must keep local tests runnable and record what is implemented versus pending.
Live Bunny/Discord testing and deployment are deferred. Local tests do not establish
provider durability, distributed writer exclusion, access policy or billed resource use.
Database credential separation and operational conductor features
remain pending. Logging changes are described above; they do not establish full guild erasure.
