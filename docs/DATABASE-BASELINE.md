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
migrations 0001–0008. Do not add or edit the family marker on an old database to bypass
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
The queue is [inactive storage groundwork](BOT-ACTIONS.md), not an enabled service.

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
moderation data stay in place. Queue execution remains inactive. Deploy matched builds
with bot migration first; version7 binaries reject version8, so use the matched pre-upgrade
backup for rollback. See [membership-bound queue storage](BOT-ACTIONS.md#membership-bound-storage).
