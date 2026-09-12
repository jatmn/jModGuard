# Tenant runtime: stores, cases and live erasure

Bot and API processes now open a bounded `TenantRuntime` against the core directory
database. A local dual-process install creates one shared guild store beside the core
file (`jmodguard.db.guild.db`) and, on Unix, a private local evidence directory
(`jmodguard.db.evidence`). Remote core databases require an explicit shared
`[[guild_stores.stores]]` entry. Catalog rows store only opaque IDs and shared/dedicated
policy; URLs and tokens stay in process configuration.

This is not a guild move. Populated-guild shared/dedicated moves remain a later feature
and are not exposed. The conductor still has no tenant routing.

## Routing and handles

`TenantRuntime` registers configured stores, creates an empty matching store or reopens
the same identity, and caches a small number of handles (default 8). There is no
per-guild task or connection pool. A dedicated store is used only for its owner guild;
every other guild uses the shared store. Initial placement is assigned once and cannot
be changed by this runtime.

Membership checks run on real reads and writes. A closed or stale enrollment cannot
record, list, serve or delete another membership's cases. Offline stores fail closed
without mutating directory rows.

## Cases, history and evidence

Selected Discord moderation outcomes (ban, kick, warning, timeout, unban, timeout
removal) are recorded in the guild store with attempted/succeeded/failed/uncertain
outcomes. Core warn/ban mirror rows are still written only after a successful action so
existing surfaces keep working. Dry-run, log-only and image-spam test reports do not
create cases. Ordinary messages are not archived in SQL.

The dashboard Cases tab lists the current membership's records, notes, and authorized
private evidence downloads. Case removal clears retained private facts. It does not
reverse a Discord action. Server owners and Administrators may delete. Delegated
case-removal role IDs can be stored, but current Discord OAuth (`identify guilds`) does
not supply member roles, so those IDs are not applied yet.

Local Unix evidence serving reads verified private objects after the journal confirms
they are available. Bunny serving is opt-in and proxies a GET after the same journal
check. Zone credentials never appear in API responses.

## Live erasure

Guild Delete with usable membership context retires the core observation and queues a
`guild_erasure_jobs` row. A bounded bot worker closes store membership, cancels in-flight
evidence, removes owned objects, purges cases, deletes remaining store identity,
records a `backup_cleanup_intents` row, wipes directory-owned configuration and core
moderation rows, and clears placement. Enrollment timestamp fencing stays so an older
join cannot be resurrected. There is no permanent departed-guild ledger. Backup-copy
cleanup is not implemented; the intent exists so a later backup feature can finish
controlled-copy deletion without reconstructing live data.

A later rejoin is a new enrollment on a fresh placement after erasure completes. Late
events using the old enrollment cannot recreate erased facts.

## Fresh installation

Unfinished services stay inactive: the conductor has no operational API, image-spam
enforcement remains log-only test mode, and no move UI/API is registered. See
[guild stores](GUILD-STORES.md), [cases](TENANT-CASES.md), [evidence](CASE-EVIDENCE.md)
and [enrollments](ENROLLMENTS.md).
