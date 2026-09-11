# Explicit case source association

The inactive guild-store library can associate one application operation and one Discord
audit entry with the same retained case. This is a storage contract for trusted producers;
it does not watch Discord, infer matches from timestamps/subjects, merge existing cases,
grant permission or enable current bot/API recording.

## Identity and conflicts

Each case starts with its original `CaseData.source`. Creation atomically registers that
source in an indexed mapping. `case_for_source` returns the canonical case ID and whether
its payload has been removed, without reading evidence/history. Missing sources return
`None`. Both outcomes still require the captured active membership and placement checks.

`link_case_source(scope, case_id, anchor, source)` requires an already-bound anchor that
identifies this exact case. Either known source can be the anchor on subsequent retries.
The new source is either a validated application operation ID or positive Discord audit ID.
The caller must establish that they identify the same action; the database cannot verify
Discord provenance. Subject/time similarity alone is not a supported correlation policy.

The model permits one ID of each kind per case. This is identity cardinality, not a
configurable retention or capacity ceiling: multiple Discord actions require separate cases.
There is no unbounded alias list. An identical association returns `Existing` without writes;
a new association returns `Linked`. A source already owned by a different case, an occupied
source kind, a missing/wrong anchor or a conflicting target ID fails. No source is reassigned.
Two independently recorded cases remain separate and produce a conflict; this slice does
not silently merge their original facts or histories.

All source operations use primary transactions with captured store, placement and membership
verification. Linking and case creation serialize with deletion and other associations.
An earlier lookup is only a snapshot; subsequent operations repeat their checks. Source
lookups use indexed keys and return at most one small identity record. Each case owns at
most two bounded keys, with no payload copies, cache, polling task or additional dependency.

## Recording and history

`record_case` now considers every registered source, including linked counterparts. Original
source plus identical original facts still returns `Existing`. For a live case, submitting
different facts under the linked counterpart fails: original observations stay immutable.
A producer should resolve the source and append permitted selected context through
[case history](CASE-HISTORY.md), preserving a stable history entry ID across retries. The
library does not infer a final outcome from a successful link or automatically append history.
Linking and appending are separate idempotent operations; a producer must survive a crash
between them without losing required follow-up work. Runtime ingestion/reconciliation and
its durable progress contract remain pending.

If one producer records a new case while another links that source elsewhere, only one
claim of the source can succeed. The loser receives a conflict and must reconcile through
an explicit trusted policy. A conflicting caller-supplied case ID is still rejected, even
when a source matches a removed record. No heuristic fallback creates a replacement case.

## Removal and cleanup

Removing a case erases its payload and owned history, retaining its source mappings as
minimal replay identities. `case_for_source` reports `removed=true`; `record_case` returns
`Removed` for either registered source without recreating content. This remains subject
to ordinary input validation and case-ID conflict checks.

A late explicit association to an already removed case is allowed using its known anchor.
It adds only the missing opaque counterpart, ensuring a later audit replay cannot resurrect
the removed content. It never reopens the case or permits history append. The returned
removed flag describes the link transaction's snapshot, not a later read authorization.
If removal races linking, both serialized orders preserve the removed case and its keys.
Invented/unlinked sources cannot be recognized automatically; this is not global replay
detection or proof that Discord polling has reconciled every event.

Closed membership denies all normal lookup/link operations immediately. `purge_closed_cases`
then deletes the parent and its source mappings/history atomically; failures roll back the
whole batch. The batch count measures parent cases, including their admitted children.
Source mappings contribute at most two rows per parent. Other guilds remain untouched.
No independent source deletion or unlink API exists. SQL guards reject replacement, updates,
orphan inserts and active-membership deletion without relying on FK/recursive-trigger PRAGMAs.
Administrators can still replace schema definitions; this is not a database privilege boundary.

Full departed-guild erasure/rejoin and backup removal remain separate lifecycle work.
These active replay markers do not prescribe a permanent departed-guild ledger. No external
file evidence or backup copy is created by this slice.

## Schema and validation

Guild migration0005 adds the source table and ownership triggers, backfilling original source
keys for existing live, removed and closed cases. It does not guess counterpart IDs or alter
case payload/history. `initialize` verifies versions1–4 and expected identity before upgrading
atomically to5. Fresh initialization applies1–5; `open` requires5. Older guild binaries reject5;
rollback requires matching pre-upgrade database/binary. Current core remains0010 and current
bot/API services do not use guild stores.

Local tests cover both association orders, immutable facts, concurrent retries and conflicting
records, write-free no-ops, late/deletion-racing associations, source replay and cleanup
isolation/rollback, raw SQL guards, scope validation and version4 backfill. Earlier version
upgrade regressions remain. Disposable libSQL exercises lost commit responses for source
creation/linking, removed-parent linking, removal and cleanup, plus cross-handle visibility.
No live Discord/Bunny proof, resource measurement, runtime permission enforcement or
complete tenant isolation is claimed.
