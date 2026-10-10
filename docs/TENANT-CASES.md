# Selected moderation cases in guild stores

The inactive guild-store library now records selected moderation facts and bounded text
evidence under captured store, placement and membership scope. Bot and API handlers now
record selected action outcomes and expose private case administration; see
[tenant runtime](TENANT-RUNTIME.md). No routine event/message archive is introduced.

## Identity and facts

`CaseId` is a portable opaque32-character lowercase hexadecimal identifier. Generate it once
and retain it across retries. Its parser and serde reject malformed values. Ownership is
always the guild and enrollment; the ID alone grants no access. Stored records must preserve
that ownership as well as IDs. No automatic cross-store import is implemented.

`CaseData` explicitly distinguishes ban, kick, formal warning, timeout, unban and timeout
removal, with attempted/succeeded/failed/uncertain outcome, subject, optional actor, original
millisecond timestamp and optional reason. Missing actor/reason is permitted; no actor or
successful outcome is fabricated. These are caller-supplied observations, not proof that a
Discord action occurred. The library never performs or reverses moderation actions.

A source is either a stable application operation ID or a positive Discord audit-entry ID.
The source key is unique within the captured guild membership. A retry with identical facts
returns the original case ID even if the caller generated a different ID. Changed facts or
reuse of an existing ID for another source fails. If both requested identities conflict with
two records, the operation fails. Callers must retain the original source and payload across
retries. [Explicit source association](CASE-SOURCES.md) can now bind an application ID and
Discord audit ID to one canonical case. The bot associates Ban/Unban/Kick it executed using
the in-process application identity from that dispatch; deferred pairs stay quarantined
for the same 30s/4096 bound so a later same-subject dispatch cannot reuse them.
Expired pending links stay application-only. The
storage layer never guesses a match or merges two existing cases.

Evidence is explicitly unavailable or a bounded nonempty list of selected text excerpts,
with channel/message/author IDs and original timestamps. Guild/membership ownership comes
from the case. No attachment URLs, arbitrary object keys, downloads or external evidence
references are accepted by `CaseData`. Separate [evidence ownership](CASE-EVIDENCE.md)
now journals selected attachment reservations and cleanup. The bot captures proving
Discord attachments for selected image-spam enforcement through [tenant runtime](TENANT-RUNTIME.md).
The API serves authorized
journal-available objects through [tenant runtime](TENANT-RUNTIME.md) and does not perform
that Discord re-fetch. Text evidence is a selected subset, not a completeness guarantee or general history.
Discord snowflake serialization reuses the existing string helpers.

## Operations and memory bounds

Every operation checks store identity, exact placement revision and captured membership in
the same primary transaction as data. Empty reads, missing removals and duplicate recording
cannot bypass this check. The raw pool remains private. These internal methods do not grant
view/add/delete permission, prove channel ownership or refresh authoritative Discord state.
Backend permissions and lifecycle coordination remain mandatory before service adoption.

| Operation | Contract |
|---|---|
| `record_case` | Create immutable facts, return `Existing` for identical-source/payload retries, or `Removed` for a deleted source. Never recreate a removed payload. |
| `case` | Read one live scoped record. Missing/deleted returns `None`; a record above the supplied byte/evidence budget fails without transferring an oversized SQL payload or erasing data. |
| `case_ids` | Bounded keyset page of live IDs only, in opaque ID order, not chronological order. No evidence payloads or offset scan. |
| `remove_case` | Atomically remove all retained payload, leaving only replay identities. Returns whether a live payload was removed; repeated/missing removal is a write-free false. |
| `purge_closed_cases` | Delete at most a supplied batch of payloads/replay markers after exact membership closure. Refuse an active scope; an empty batch writes nothing. |

`CaseLimits` is trusted installation policy: defaults are64KiB serialized record size,
32 selected excerpts and100 IDs/cleanup rows per request. All three are configurable through
library inputs; operator UI integration is pending. These are starting budgets, not lifetime
retention or a measured storage allowance. Input content is checked before serialization,
and the entire serialized record must fit the byte budget. Single-record reads check SQL
byte length before returning payload; pages never read payloads. Lowering budgets can make
existing records unreadable under that budget, but never silently deletes them. Raise the
budget or use explicit authorized deletion. There is no automatic age expiration or quota
eviction. Capacity monitoring, total tenant budgets and admission policy remain future work.

Real mutations use existing remote receipt recovery. Reads, identical retries, removed-key
replays and missing removals create no receipts. Cases do not change log/config/slash
revisions. Original payloads are immutable. [Append-only history](CASE-HISTORY.md) now records notes,
corrections and selected policy context without rewriting them. Explicit source association
is described in [case sources](CASE-SOURCES.md). Bot-owned Ban/Unban/Kick now bind a Discord
audit counterpart through in-process pending identity; presentation of both sources in the
dashboard remains pending.

## Removal and closure

Removal clears subject, actor, reason, outcome, timestamp and all selected excerpts together.
The remaining row contains guild/enrollment, case ID, source key and a deleted flag; owned
source mappings retain an explicitly linked counterpart. Retain these minimal markers while
the membership is active so replay of either registered source cannot restore erased content. A newly invented source key is not the same replay; future trusted
producers must preserve/correlate source identity. This is not global audit replay detection.
SQL guards reject updates except payload removal, INSERT OR REPLACE identity bypass and
physical deletion of replay keys while membership is active, without relying on foreign-key
or recursive-trigger settings. Database administrators can still replace schema definitions.

Closing membership immediately denies all normal case access/writes. Case cleanup is a
separate bounded operation; it can remove retained payloads and replay markers after closure
without admitting stale work. A parent with unsettled file-evidence ownership now blocks that
purge batch until capture settlement and object deletion are acknowledged. Other guilds are untouched. The closed membership identity
still blocks rebinding. Live guild erasure/rejoin, directory cleanup and evidence ownership
are coordinated by [tenant runtime](TENANT-RUNTIME.md). This case-storage library does not
create or manage backup copies. No permanent departed-guild ledger is the product target.

## Schema and validation

Guild migration0003 adds only `guild_cases` and its indexes/guards within `jmodguard-guild-1`.
`initialize` verifies the expected version1–5 family and identity before atomically extending
it to6, preserving placement, settings and cases. Version4 adds owned history; version5 adds
source mappings with original-key backfill. Version6 adds an [evidence ownership journal](CASE-EVIDENCE.md). Fresh initialization
applies all six steps.
`open` requires 9. Older guild-store binaries reject 9; rollback needs matching pre-upgrade
backups and binaries. Core is at migration 0018. Bot and API open these stores through
[tenant runtime](TENANT-RUNTIME.md).

Local regressions cover scoped persistence/paging, source conflicts/concurrent retries,
removed-source replay, payload/ID validation and budgets, SQL guards, removal rollback,
closed-scope denial, bounded cleanup and preservation through version2 upgrade. Disposable
libSQL tests cover lost responses for record/removal/purge and cross-handle visibility.
No live Bunny/Discord tests or measured RSS/billing is supplied. Runtime routing, selected
case recording, authorized serving and live guild erasure are described in
[tenant runtime](TENANT-RUNTIME.md).
