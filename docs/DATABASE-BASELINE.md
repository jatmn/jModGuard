# Fresh database baseline

This overhaul starts a new database lineage, `jmodguard-core-1`. Point it at an
empty database for a new installation. Databases created by the previous application
or unpublished development baselines with a different marker are unsupported. This release does not
copy, convert, erase or adopt them. Recreate development fixtures explicitly.

The first migration writes `meta.schema_family` in the same transaction as its initial
tables and `user_version`. An interrupted uncommitted initialization leaves no partial
marker. Bot startup can resume subsequent schema/bootstrap steps in the recognized
family. API startup continues to wait for the bot to initialize an empty database and
publish the completed bootstrap marker.

Both processes check the family on an authoritative snapshot. Foreign/unmarked schemas,
invalid initialized versions and schemas newer than the running binary fail startup
without clearing readiness or rewriting application rows. A schema-family/version
mismatch is non-retryable. Version numbers describe steps within this lineage; they do
not authorize migration from an unrelated database with the same numeric version.
This is a compatibility guard, not a credential/tenant security boundary or a complete
schema-integrity validator.

New schemas omit the retired `messages`, `members`, `users` and unused `mutes` tables.
Their archive, roster, user-profile and retention data helpers are removed from the
shared library. Ordinary event reporting and the detector's working state do not need
SQL history. Warnings, bans, selected events, nickname jobs and mutation receipts remain
in the new schema; this does not import records from an unsupported database.

The application is still being overhauled. The full case/evidence model, membership-scoped tenant
data, shared/dedicated routing, scoped database privileges, erasure and recovery
contracts are not implemented by this baseline. Existing configuration/default healing,
legacy global-channel schema scaffolding and the unused internal retention field await
later cleanup; they do not provide old-environment import support.

Local regressions check schema contents, reopen with retained moderation records,
unsupported-database preservation, newer/invalid version rejection, empty API startup
and atomic rollback of the first schema/marker. The optional remote libSQL smoke now
checks the absence of archive tables on a disposable local server. This does not establish
Bunny behavior. No Bunny resource was created,
modified or used to establish provider-specific behavior.
## Setup and recovery boundary

Use a new local SQLite path or an empty remote libSQL database, with matching bot/API
binaries. Start the bot to initialize it, then the API. The current baseline contains
migrations 0001–0019. Do not add or edit the family marker on an old database to bypass
the guard; the version number alone cannot establish compatibility.

Keep a backup with the binaries/images that support its family and version. A backup from an unsupported
lineage requires its matching release; it cannot be restored into this baseline as
an upgrade. This is a schema boundary, not an automatic backup or restore implementation.

## Action storage extension

Migration 0006 adds an initially empty durable action queue and replay-floor metadata
within `jmodguard-core-1`. Bot startup can extend version 5 while retaining configuration
and moderation records; the API waits for the updated bootstrap marker. Deploy matched
bot/API builds. Older binaries that support only version 5 reject version 6 on startup;
rollback requires the matching pre-upgrade database backup, not a version-marker edit.
This does not add a production backup implementation or an old-lineage migration.
This storage foundation is now used by [queued administrative actions](BOT-ACTIONS.md);
runtime activation adds no further schema change.

## Membership observation extension

Migration 0007 adds an empty `guild_enrollments` table for the bot's own membership
observations. It does not infer a join from old settings/jobs or bind queue/nickname work.
The bot initializes it; the matching API waits for completed bootstrap. Older binaries
supporting only version6 reject version7. Preserve matched pre-upgrade backups for rollback.
No data erasure or authorization boundary is activated. See [membership observations](ENROLLMENTS.md).

## Action membership extension

Migration 0008 binds new queue work to observed membership without recreating the table
introduced in 0007. Earlier actions are never adopted: unstarted work becomes cancelled,
running work uncertain, and resolved/uncertain receipts remain. All existing settings and
moderation data stay in place. Runtime activation is documented separately. Deploy matched builds
with bot migration first; version7 binaries reject version8, so use the matched pre-upgrade
backup for rollback. See [membership-bound queue storage](BOT-ACTIONS.md#membership-bound-storage).

## Nickname membership extension

Migration 0009 captures request identity, membership and immutable nickname on new work.
Existing nickname rows become dormant and remain unbound, even if an active membership
already exists. They require a fresh settings save or Re-sync after membership observation;
startup and rejoin never adopt them. Settings/moderation records remain intact. Deploy
matched bot/API builds, bot first. Version8 binaries reject version9; rollback requires
matching pre-upgrade backup and binaries. No live upgrade/restore automation is supplied.

## Guild-store catalog extension

Migration0010 adds empty `guild_store_catalog` and `guild_store_placements` tables with
SQL constraints/triggers for valid shared/dedicated ownership. It does not infer placements
from existing configuration, membership or data and does not move any records. Current
bot/API operation continues against the configured database. Deploy matched builds, bot
first; version10 binaries reject version11. Rollback requires matching pre-upgrade backup
and binaries. See [catalog scope and limits](GUILD-STORES.md).

Migration 0011 adds live `guild_erasure_jobs` and the historical `backup_cleanup_intents`
table. The latter has no application producer or consumer; its schema is retained for
compatibility. These tables are not a departed-guild membership ledger.

Migration 0012 adds `guild_write_fences`. Bot/API operations still enforce these
fences; retain the table and existing rows when upgrading.
Migration 0013 adds per-guild, per-event `log_filters` for ignored user/bot IDs
and source channel IDs. Start the bot before the API so the new table exists.
Migration 0014 adds optional banned-user and moderator display names to retained
ban rows. Existing rows keep their exact IDs and have no fabricated names.
Migration 0015 adds component-update coordination and managed-WebUI asset metadata.
Core migration 0016 persists a per-bot shared member-sync request gate so
rate-limit delays and the minimum request interval survive worker restart
across guilds ([MEMBERS.md](MEMBERS.md)).

## Separate guild-store family

The `GuildDataStore` library initializes/verifies `jmodguard-guild-1`, version16,
using `migrations/guild/0001_identity.sql` through `0015_local_write_fence.sql`. Version 16
adds optional immutable ban attempt tags to case payloads without changing tables or
rewriting legacy cases. Earlier binaries must not read these payloads. This separate
database holds identity, placement bindings, receipts, membership/log-channel
settings, selected cases, history, sources, evidence ownership, Bunny transfer
progress, tenant settings, bounded latest-message snapshots and the bounded
[member directory](MEMBERS.md). Initialize verifies and upgrades a matching earlier
store atomically; open requires 14. See [tenant runtime](TENANT-RUNTIME.md). Core is at
migration 0019. Each opener refuses the other family. No records are moved by startup.
Guild migration 0009 adds bounded latest-message snapshots with fixed 24-hour expiry
and store-level capture health. Guild migration 0010 adds the member directory,
stays, case projection, sync state and immutable initial-seed completion.
Guild migration 0011 tracks coverage-gap generations and last-successful-run
totals, and updates member-state observation constraints.
Guild migration 0012 adds nullable per-phase committed counters and the measured
member-page interval. Existing version10 aggregate progress cannot reconstruct
earlier ban, check, or audit counts, so those historical phase counts remain
unknown after upgrade. Initial completion, run cursors and retries are preserved.
Guild migration 0013 preserves exact, dated bot-executor audit actions while their
local case-source association remains unresolved.
Guild migration 0014 stops case-retention triggers from bumping import page
`directory_revision` when only ban-history metadata changes.
See [store initialization and recovery limits](GUILD-STORES.md#separate-store-schema-and-handle).

Selected case payload removal, replay-key cleanup and live guild erasure are described in
[tenant case storage](TENANT-CASES.md) and [tenant runtime](TENANT-RUNTIME.md).

Guild migration 0009 starts with no retained message payloads. It adds snapshot
expiry/identity constraints and bounded store-health metadata. Deploy matching
bot/API builds; old guild-store binaries reject version9. Rollback requires a
matching pre-upgrade backup and binaries. See [application database budgets](DATABASE-CAPACITY.md).

Core migration 0017 adds ordered guild event/ban page indexes and a state/expiry
index for the bot-action poll. Core migration 0018 deletes surplus active ban
rows for the same guild and user, keeping the row that already has an actor or
a real reason, otherwise the earliest id. Inactive history stays. A unique index
then rejects a second active row. Upgrade the bot and API together through the
existing paired schema protocol; their exact core runtime schema is now 19.
The guild schema is 15. Migration 0015 adds `guild_local_write_fence` so a member-sync status read can see a fence inside the guild transaction, and `guild_writer_epoch` so a leased guild commit must match the holder's generation before it lands. Core fences remain in the other database. A newer holder raises that epoch in the guild store before it acts. The core writer lock is not held across the guild commit.
