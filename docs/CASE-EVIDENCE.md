# Private evidence ownership journal

The inactive guild-store library records selected attachment ownership and lifecycle state in
SQL. It does not fetch Discord attachments, run background workers, or perform provider HTTP
by itself. The bot captures proving Discord attachments for selected image-spam enforcement
through [tenant runtime](TENANT-RUNTIME.md); that path reserves journal rows and streams bytes
through the opt-in [local transfer backend](LOCAL-EVIDENCE.md) or [Bunny adapter](BUNNY-EVIDENCE.md).
The API serves authorized journal-available objects and does not perform that Discord re-fetch.
Backup-copy
cleanup remains later work.

## Reservation and identity

`reserve_case_evidence` requires a live parent, captured active membership/placement, a stable
evidence ID and an `EvidenceRequest`: nonsecret storage alias, expected bytes and selected
channel/message/attachment IDs. No source URL, filename, token or arbitrary object key is
stored. Producers must independently verify attachment ownership and permission.

Keys are derived as `evidence/{guild}/{enrollment}/{case}/{evidence}`, using validated IDs.
They remain stable across guild-store moves. Trusted installation configuration must bind
the alias to a stable, allowlisted private evidence resource, never the backups-only zone.
It cannot silently reassign that alias while objects remain. This library neither establishes
resource privacy nor loads provider credentials. These internal types are not HTTP wire models.

Evidence IDs are unique per guild enrollment. Matching retries return `created=false`;
changed parent, storage alias, size or retained origin conflicts. After origin is cleared,
a cancelled identity stays cancelled. No retry reopens it. Reserve before external writes;
neither a job nor `created=true` is a worker lease or an object-access grant.

## Capture and removal ordering

| State | Journal meaning |
|---|---|
| `Capturing` | This reserved key may still receive writes, even after cancellation. |
| `Available` | A trusted producer reported matching stored size and a SHA-256 digest. Actual verification belongs to the adapter; reads still require live parent/membership. |
| `CleanupPending` | Capture settled; this key requires verified deletion/absence, including after failed uploads. |
| `Erased` | A trusted worker acknowledged object absence. This is not backup-copy deletion or full guild erasure. |

Before `settle_evidence_capture(Stored/Discarded)`, the uploader/reconciler must establish that
no writes can still arrive. A timeout, restart or early 404 is insufficient. Stored size must
match the reservation and SHA-256 must be lowercase hexadecimal. This layer checks shape and
equality; it never hashes/inspects files. Identical available completions are write-free;
conflicting completions fail. Cleanup/erased completion retries cannot revive content.

Case removal atomically clears origin/digest metadata and cancels capture. Available objects
become cleanup-pending. Capturing objects remain capturing/cancelled until settlement, then
become cleanup-pending even after a late successful upload. This prevents early deletion
acknowledgment followed by an orphaned late write. Failed evidence cancellation rolls back
parent payload/history removal. Normal reads hide removed/capturing/cleanup/erased objects.

`confirm_evidence_deleted` only accepts cleanup-pending work or an erased no-op retry. It
journals a trusted worker's verified result; it performs no external deletion. Capturing and
available objects cannot be acknowledged away. There is no independent evidence unlink API.
Interrupted capture remains inspectable and blocks cleanup. No lease expiry, automatic upload
retry or guessed settlement is provided. Future workers must coordinate exclusive writes and
reconcile uncertain uploads; unresolved work must remain blocked and visible to the operator.

## Closure, cleanup and limits

Closure immediately denies normal reads/reservations without scanning every attachment.
`cancel_closed_evidence` prepares cleanup and clears metadata in indexed, bounded batches,
refusing active scopes. Capturing objects await settlement; available objects become pending
deletion. Before preparation, available rows may retain internal state/metadata while access
is denied. Returned states describe journal state, not permission to serve content.

`evidence_jobs` returns keyset pages for capture reconciliation or deletion, including closed
memberships. It returns fixed-size identities/status, not origin/digest/file content. Closed
capture jobs are marked cancelled even before preparation. These are snapshots, not claims.
Workers must begin later sweeps at the start so newly transitioned IDs below old cursors are
not missed. Settlement and cleanup intentionally accept exact closed scope: they are internal
lifecycle methods, never guild-facing authorization.

`purge_closed_cases` refuses parents with any non-erased evidence. After acknowledgment it
purges parent/history/source/evidence rows atomically. A blocked parent can abort that case
batch; finish object cleanup before retrying. Other guilds survive. SQL guards protect
identity, transitions, replacement and deletion without FK/recursive-trigger PRAGMAs; they
do not attest provider behavior or resist administrators replacing schema. No permanent
departed-guild ledger is prescribed.

`EvidenceLimits` has no defaults. Trusted callers supply per-file bytes, per-case item/byte
limits and page/batch size. Admission shares the reservation transaction and includes pending,
failed and erased reservations until parent purge, preventing identity churn from bypassing
the bound. Lower budgets never evict retained data; per-request validation still applies to
retries. Larger limits increase SQL/cleanup work and imply no measured capacity. No file bytes
or unbounded metadata blobs enter SQL. No age expiry, dependency, cache or task is added.

## Schema and validation

Guild0006 adds the journal, three indexes and ownership/transition triggers. Fresh setup applies
1–7; verified versions1–6 upgrade atomically without inventing attachments or PUT attempts.
Open requires 8 after the erasure/settings extension. Rollback needs matching pre-upgrade
database/binary. Core is at migration 0012. The bot captures proving attachments through
tenant runtime as described above; the API does not perform that Discord re-fetch.
Backup-copy cleanup remains later work.

Local tests cover concurrent reservations, retries/conflicts, budgets, late uploads, removal
races, scope denial, bounded closed cleanup/pages, SQL guards, cancellation/purge rollback
and version5 upgrade. A local file fixture simulates a late write before settlement/deletion;
it is distinct from the real local backend linked above. Disposable libSQL injects lost responses for reservation,
settlement, cancellation preparation, acknowledgment and parent cleanup. No live provider
proof, serving permission, backup-copy erasure, resource measurement or full tenant-isolation
guarantee is supplied.
