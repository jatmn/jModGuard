# Append-only case history

The inactive guild-store library can append notes, corrections and selected policy context
to an existing case. It preserves the original `CaseData`; no runtime recorder, HTTP route,
permission grant, Discord request, file reference or background worker is added.

## Entries and order

`CaseHistoryData` carries an optional observed actor, positive millisecond timestamp and
one typed body:

- `Note`: nonblank text.
- `Correction`: an outcome and/or reason correction, plus a nonblank explanation. Reason
  can explicitly be cleared or set; omission means no reason correction.
- `Policy`: nonblank rule ID, revision and description selected by the producer. This is
  descriptive context, not a full configuration snapshot or proof the rule executed.

Actor IDs use the shared snowflake string serializer. The supplied actor/time/outcome are
observations, not authentication or verified Discord state. No outcome is inferred from a
successful database write. No attachment/text-evidence supplement is added by this history
format; selected evidence remains in the parent case until its richer ownership model lands.

An entry uses an opaque `CaseId` as its stable entry identifier, scoped to its parent case
and captured guild membership. Retain that ID and exact data across retries. Reusing it with
identical data returns its original sequence without writing; changed data conflicts. A
new ID means a separate entry, even for the same text. This is not source correlation or
content deduplication. It does not change the parent's source-key replay contract.

Primary transactions assign increasing per-case sequences. They describe insertion order,
not event chronology; producer timestamps may arrive out of order. Concurrent entries are
serialized. The original case stays unchanged, and this library does not automatically fold
corrections into a final effective outcome/reason. Future display/reconciliation must show
history and its provenance rather than silently rewrite the original facts.

## Operations and budgets

All operations verify expected store identity, placement revision and captured active
membership with the live parent case in the same primary transaction. Missing or removed
parents cannot accept entries; reads return `None` or an empty page. Wrong/closed membership
still fails, including empty reads. A case ID or entry ID alone never authorizes access.

| Operation | Contract |
|---|---|
| `append_case_history` | Append a new entry or return its existing ID/sequence for an identical retry. Real writes use existing mutation-receipt recovery. |
| `case_history_entry` | Read one scoped entry after checking SQL byte length against the supplied entry budget. |
| `case_history_ids` | Return a bounded sequence-keyset page of ID/sequence references; no history payloads are bulk-loaded. |

`CaseHistoryLimits` requires explicit maximum entry count and total serialized history
payload bytes per case. It reuses `CaseLimits` for individual record bytes and page size;
that type's evidence-item limit does not govern history text. No new installation defaults
are chosen here. Future operator settings must supply these limits. IDs/index overhead is
bounded by entry count separately from the payload-byte total.

Before a new append, the same transaction checks current count and aggregate bytes. A
conflict or budget failure leaves prior entries intact. Identical retries are handled before
aggregate admission, so reaching/lowering those aggregate limits does not create duplicate
writes. Individual entry validation/byte limits still apply to retries and reads. Lowering
budgets never silently deletes history or expires it by age. Aggregate limits govern new
admission; already retained individually readable entries remain readable. Reads are bounded
per record/page. Count/byte admission scans this case's existing history in SQL, without
loading its payloads into application memory; it is not a measured constant-time operation.

## Removal, cleanup and scope

History belongs to its parent. SQL triggers delete all its history when `remove_case` clears
the case payload, in that same transaction. A history deletion failure rolls back parent
removal. A racing append either lands before removal and is deleted, or sees a removed
parent and fails. The parent's minimal source replay marker survives; neither a new history
ID nor a retried old ID can append to it. There is no separate entry-edit/delete operation.

Closing membership denies history access immediately. `purge_closed_cases` also deletes
each purged parent's history atomically. An [unsettled evidence object](CASE-EVIDENCE.md)
blocks parent purge until cleanup is acknowledged. Its batch count is parent cases, not total child
rows or bytes; cleanup work includes the admitted history for those cases. Per-case history
budgets bound normal admitted growth, but lowering them does not shrink existing cleanup
work. An interrupted failed transaction leaves the parent and its history together.

Guards prevent history updates, cross-scope/orphan insertions and INSERT OR REPLACE bypass,
and refuse direct deletion while the parent is live and its membership active. They work
without foreign-key or recursive-trigger settings. Database administrators can still replace
schema; this is not a database privilege boundary. Full guild erasure/rejoin, directory and
backup cleanup, external evidence ownership and distributed writer exclusion remain later.

## Schema and validation

Guild migration0004 adds only the history table, constraints and parent/child triggers.
Version5 additionally tracks [explicit case sources](CASE-SOURCES.md). `initialize` verifies
and extends matching version1–5 stores to6, preserving cases, history, replay markers and
settings. Open now requires 8; see [tenant runtime](TENANT-RUNTIME.md).

Local tests cover original-fact preservation, scope isolation, sequence pages, concurrent
retries, conflicts, count/byte/read limits, removal/append races, child-deletion rollback,
closed cleanup, SQL guards, input validation and version3 upgrade. Disposable libSQL covers
lost append/removal/purge responses, cross-handle reads and duplicate retries.

Backend note/correction/view permissions, trusted runtime source matching, richer proof/evidence,
capacity monitoring and runtime case presentation remain pending. Local tests establish no
live Bunny/Discord behavior, resource measurements or complete tenant-isolation guarantee.
