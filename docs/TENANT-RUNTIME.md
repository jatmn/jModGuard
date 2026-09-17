# Tenant runtime: stores, cases and live erasure

Bot and API processes now open a bounded `TenantRuntime` against the core directory
database. A local dual-process install creates one shared guild store beside the core
file (`jmodguard.db.guild.db`) and, on Unix, a private local evidence directory
(`jmodguard.db.evidence`). Remote core databases require an explicit shared
`[[guild_stores.stores]]` entry or the atomic `JMODGUARD_GUILD_STORE_URL` /
`JMODGUARD_GUILD_STORE_TOKEN` environment pair. Catalog rows store only opaque IDs and shared/dedicated
policy; URLs and tokens stay in process configuration.

This is not a guild move. Populated-guild shared/dedicated moves are conductor-owned;
see [guild moves](GUILD-MOVES.md). The conductor still has no tenant routing.

## Routing and handles

`TenantRuntime` registers configured stores, creates an empty matching store or reopens
the same identity, and caches a small number of handles (default 8). An empty configured
store `id` rediscovers the stamped store identity, then the matching catalog row; a new
id is minted only when both are absent. There is no
per-guild task or connection pool. A dedicated store is used only for its owner guild;
every other guild uses the shared store. Initial placement is assigned once and cannot
be changed by this runtime. When `guild_store_placements` already exists, activation
admits that catalog row even if a dedicated store remains configured for the guild;
config is only the hint used to mint the first placement. Activation checks the captured active core enrollment and
absence of an erasure job in the same primary transaction that admits placement.
A departure before admission prevents activation; a departure after admission sees
that placement and queues cleanup. The runtime guild lock orders subsequent store
binding against its erasure worker. Delayed activation cannot recreate an erased
membership or enqueue erasure of a replacement membership.

Membership checks run on real reads and writes. A closed or stale enrollment cannot
record, list, serve or delete another membership's cases. Offline stores fail closed
without mutating directory rows.

## Cases, history and evidence

Selected Discord moderation outcomes (ban, kick, warning, timeout, unban, timeout
removal) are recorded in the guild store with attempted/succeeded/failed/uncertain
outcomes. Core warn/ban mirror rows are still written only after a successful action so
existing surfaces keep working. Dry-run, log-only and image-spam test reports do not
create cases. Bot command, automod and queued WebUI action paths own these application
cases. Gateway ban/unban observations continue to maintain core audit mirrors, but do
not create a second tenant case for bot-owned actions: those paths register a bounded,
one-shot in-process suppression before dispatch. Unmarked manual/external gateway
actions still create a case. This is event ownership, not a guessed application/audit
source association; trusted durable Discord-audit association remains pending. Ordinary
messages are not archived in SQL. Selected operations capture their enrollment before
asynchronous effects and carry it through result recording; completion never adopts a
replacement enrollment.

The dashboard Cases tab pages through the current membership's records in opaque ID
order, displays all admitted history (up to 64 entries) and selected inline text, and
provides authorized private evidence downloads. Case removal clears retained private facts in the guild-store
journal immediately. It does not reverse a Discord action. The delete HTTP response reports
`removed` for that journal step; `cleanup_pending` is true when object-backend cleanup or
backup-reminder persistence could not finish in the same request. The administrator-only
Removed records view pages through retained removal identities (`cases?removed=true`)
and allows explicit cleanup retry, including after a reload. It never restores private
payloads. Repeating DELETE recovers the backup reminder from a verified removal tombstone;
reminder insertion is idempotent. There is no automatic active-guild retry worker. Server owners and
Administrators may delete. Delegated case-removal role IDs can be stored, but current
Discord OAuth (`identify guilds`) does not supply member roles, so those IDs are not applied
yet.

Local Unix evidence serving reads verified private objects after the journal confirms
they are available. Bunny serving is opt-in and proxies a GET after the same journal
check. Zone credentials never appear in API responses. If existing owned objects need
cleanup while the backend is disabled, they remain pending: `none` is not proof of
physical absence. Restore the owning backend and retry; do not change resource identity.

## Live erasure

Guild Delete with usable membership context retires the core observation and admits its
`guild_erasure_jobs` row in the same core transaction when a placement exists. A failed
queue write leaves retirement uncommitted so the confirmed departure can be retried. A bounded bot worker closes store membership, cancels in-flight
evidence, removes owned objects, purges cases, deletes remaining store identity,
records a `backup_cleanup_intents` row, wipes directory-owned configuration and core
moderation rows, and clears placement. Enrollment timestamp fencing stays so an older
join cannot be resurrected. There is no permanent departed-guild ledger. The conductor
backup worker sanitizes complete local archives against surviving intents and
acknowledges those rows after at least one complete archive is rewritten; it
does not import tenant routing. Live Bunny backup-copy
deletion remains later work. The durable job can resume its
core cleanup suffix after the store binding has already been erased: the exact store
identity and absence of the guild binding are checked without weakening normal data-scope
validation. If activation stopped after placement binding but before membership insertion,
the same exact-placement recovery can remove that empty binding. Configuration deletions advance the existing observation metadata atomically;
moderation-only cleanup does not bump configuration versions.

The worker advances a bounded guild-ID cursor before each erasure attempt and wraps
at the end of the queue. A failed guild stays queued, while later ticks can erase
other guilds with healthy storage. The cursor is process-local and restarts from the
beginning; durable jobs, rather than cursor state, preserve cleanup obligations.

A later rejoin is a new enrollment on a fresh placement after erasure completes. Late
events using the old enrollment cannot recreate erased facts. If a newer active
enrollment is observed before an older erasure job reaches directory cleanup, the
closed guild-store scope is still erased, but the stale job does not wipe directory
configuration now owned by the active membership.

## Fresh installation

Unfinished live provider checks stay deferred; public installation validation is
available on the conductor. Owner-selected updates and rollback are available on the conductor when an update
root is configured; see [UPDATES.md](UPDATES.md).
Signed Stable/Nightly discovery can list complete publications when a feed and
trust keys are configured; see [RELEASES.md](RELEASES.md). Local encrypted
backup/restore and guild-store moves are available on the conductor when their process
paths are configured; see [BACKUP-RESTORE.md](BACKUP-RESTORE.md) and
[GUILD-MOVES.md](GUILD-MOVES.md).
Image and text spam enforcement is implemented separately; see
[IMAGE-SPAM.md](IMAGE-SPAM.md). The bot captures proving Discord attachments for
selected image-spam enforcement through tenant runtime; the API serves authorized
journal-available objects and does not perform that Discord re-fetch. See [guild stores](GUILD-STORES.md), [cases](TENANT-CASES.md),
[evidence](CASE-EVIDENCE.md) and [enrollments](ENROLLMENTS.md).

## Continuation and retry boundaries

Core moderation mirror and event writers carry the operation's captured enrollment
through asynchronous effects and check it with each primary write. A delayed old
completion cannot restore deleted core payload or change a replacement enrollment's
mirror. Existing queued-action completion keeps its own transactional membership fence.
Discord calls already dispatched cannot be recalled by these database checks.

A newer positive join also admits cleanup of the displaced enrollment before replacing
its core identity. Subsequent departure preserves that oldest outstanding store owner.
A confirmed departure without placement settles directory-only cleanup and its backup
reminder atomically with retirement, including configuration observation metadata; it
does not create a store or infer a provider object's absence.

Gateway suppression retains individual local dispatch tokens. A definitive failure
releases only its own token; an echo consumes one outstanding token. The same bounded
30-second in-process lifetime applies; this is not durable audit correlation.

History note POSTs require a 32-hex `entry_id` retained for one note intent. The server
supplies the authenticated actor and first committed timestamp; retries with the same
actor/text return the existing entry. Changing the intent payload conflicts. A separate
identical note uses a new ID. The dashboard retains the intent while retrying an ambiguous
response and prevents simultaneous submission of the same form. Reloading or opening a
new form starts a new intent; clients must retain IDs when retrying across reloads.

Case-removal settings and tenant log mappings share the membership configuration
revision. Settings reads return that authoritative revision, including after log edits.

## Runtime recovery and producer coverage

Custom-command Timeout, Kick and Ban plan steps record the same selected application
outcomes as built-in moderation commands, including the actor and failed/uncertain
Discord result. Ban steps own their Gateway echo. Plans retain sequential execution,
continue after a step error, and keep the existing core summary; routine plan steps
still create no case. A queued unban that reports `already_unbanned` releases its echo
token because Discord made no mutation.

Cleanup pages include cancelled captures as well as settled deletion jobs. The existing
backend decides whether all writes have settled: local cleanup holds the resource lock;
an unresolved Bunny PUT remains blocked. A disabled backend reports pending cleanup.
Missing or empty guild databases with retained placements or erasure jobs are refused,
not recreated under the old identity. Restore the original store and retry; absence of
a newly created binding cannot prove that separately stored objects were removed.

Downloads recheck resource authority as well as journal availability and content hashes.
Local reads hold the stable resource lock and verify its marker; Bunny reads require the
original persisted binding and accepted transfer plus the configured resource marker.
Matching bytes do not authorize switching to an unmarked or backup resource.
