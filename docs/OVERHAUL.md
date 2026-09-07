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
  Bare Cargo commands still select the application; conductor builds use an explicit
  package selection, and CI uses `--workspace` to include all tests.
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
  implementation; `jmodguard-runtime` contains shared shutdown and health utilities;
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

## Next implementation slices

1. Separate bot/API state and credentials. Replace API bot-token operations with typed,
   durable bot actions, preserving current guild permission checks and explicit outcomes.
   Complete bot/API crate boundaries without pulling gateway/cache state into the API.
2. Implement the fresh data model and tenant routing: shared/dedicated guild placement,
   scoped case/evidence identities, selected durable audit records and bounded transient
   message context. Correct exact repeated-image matching in the actual event pipeline.
3. Add durable sessions, owner/guild authorization surfaces and effective settings with
   revision handling. Backend authority must enforce every UI permission.
4. Build release manifests/artifacts, the conductor control store and provider adapter,
   then authenticated updates and portable backup/restore using the validated contracts.
5. Complete guild/operator UI, backup scheduling/download/upload/restore, system metrics,
   setup and provider documentation. Replace the old private-ops workflow before launch.

Each slice must keep local tests runnable and record what is implemented versus pending.
Live Bunny/Discord testing and deployment are deferred. Local tests do not establish
provider durability, distributed writer exclusion, access policy or billed resource use.
Database credential separation, API bot-token removal and operational conductor features
remain pending. Logging changes are described above; they do not establish full guild erasure.
