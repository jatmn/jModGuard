# Durable action storage (inactive)

The library has a queue for typed unban and log-channel requests. Neither service admits
these requests, dispatches them, schedules cleanup or exposes them through HTTP/the WebUI.
Existing unban and log-channel validation still run directly in the API using its bot
token. The nickname outbox remains a separate active path; see
[nickname membership checks](ENROLLMENTS.md#nickname-reconciliation).

## Storage contract

- Admission accepts positive guild/actor IDs and validated typed payloads. No SQL,
  executable code, arbitrary URLs or credentials are part of the action protocol.
  Snowflakes accept string or integer input and serialize as strings; an optional
  channel ID also accepts/serializes null for a clear. Claim tokens are never serialized.
- A request key is `<positive Unix seconds>_<nonce>` (at most 64 ASCII characters).
  Retrying the same guild/key/actor/payload returns the existing receipt. A conflicting
  actor or payload fails. A receipt belonging to another membership also fails; it is
  never rebound to a new join. Unknown remote commit outcomes require the **same** key.
- Primary transactions enforce a per-guild pending limit and one active action per
  target (user for unban, event for log configuration). Uncertain work occupies both
  limits within its membership. Reads and cancellation
  require the current membership; scoped helpers also check the caller's captured identity.
  These checks are not user authorization or tenant credential isolation.
- Claims are exclusive. Beginning consumes the claim once before any future dispatch;
  both begin and completion match the stored guild, actor, payload, expiry, claim and
  enrollment ID/join timestamp, and require that membership to remain active.
  Expired pending/claimed work cannot begin. Expired running work becomes uncertain and
  is never automatically retried. A known late result can resolve its original claim while that membership remains active.
- Completion commits its status and local unban record/audit or log-setting change in
  one transaction. This records a **trusted caller's reported result**, not proof that
  Discord performed an operation. SQL receipts cannot make a remote Discord action
  atomic or guarantee exactly-once delivery. A future worker must report outcomes honestly.
  Direct writes and queued completion share log-mapping/version semantics: a clear
  advances configuration versions/revisions only when a mapping is actually deleted.
  An absent-mapping queued clear still commits its completed action state atomically.
- Pending work can be cancelled within its guild. A direct log-channel upsert/delete
  supersedes active queued work for that guild/event and current membership atomically,
  so late completion cannot overwrite the newer setting. It cannot undo a remote effect already performed.
  Clearing an absent mapping still supersedes queued changes, but does not advance
  configuration versions or revisions when no mapping was deleted.
- Cleanup removes only a bounded batch of resolved operational history. Age uses the
  later of completion and request timestamp. It never purges unresolved uncertain work
  or retained moderation audits. Deletions and a monotonically increasing admission
  cutoff commit together; old keys remain invalid after pruning, restart, clock rollback
  or a later retention increase. Extant matching receipts can acknowledge retries only
  within their original current membership.

`bot_actions::Limits` supplies storage-only defaults: 32 active requests per guild,
300-second maximum age, seven-day resolved-history retention and 128-row cleanup batch.
They are library parameters with validation, **not runtime settings or active schedules**.
No queue maintenance task or new dependency is added. A future activation must expose
appropriate controls and run bounded housekeeping; this slice creates no runtime queue
traffic. Protocol forward clock tolerance is five minutes. Poll-time expiry processing
is capped at 64 rows per state group and recent history reads at 50 rows.

## Membership-bound storage

Migration 0008 binds every new action to an active `guild_enrollments` identity and join
timestamp. Retirement/replacement cancels pending/claimed work and marks running work
uncertain in the same transaction as the membership change. Matching/stale membership
observations remain write-free. A former membership's uncertain outcome remains retained
and does not block a new membership's target. Direct log writes do not cancel that old
uncertain record; erasure/reconciliation remain separate work.

Use `enqueue_enrolled_bot_action`, `enrolled_bot_action`, `recent_enrolled_bot_actions`
and `cancel_enrolled_bot_action` for future callers that have captured membership during
authorization. Each verifies guild, enrollment ID and join timestamp inside the primary
transaction before admitting/replaying, reading or cancelling. Changed identity returns
`EnrollmentChanged`; failed membership verification returns `EnrollmentUnverified` without
exposing database details. These values identify a scope; they grant no user permission.

The existing guild-ID helpers remain trusted internal operations: they resolve current
membership at the operation, without protecting an earlier authorization snapshot. They
must not replace captured-scope helpers in future dashboard routes. Status reads hide
old/unbound memberships. `bot_action_can_dispatch` rechecks running claim identity and
current membership at the primary; it does not perform Discord validation or a mutation.

Pre-0008 actions remain unbound. Migration cancels unstarted work, marks running work
uncertain, and keeps resolved/uncertain receipts; no old request is assigned to an existing
membership. Old keys cannot be resubmitted as new work. Start matched bot/API builds,
bot first; rollback needs matched pre-upgrade backup/binaries. This is an internal library
contract change with no current HTTP producer or worker.

## Activation prerequisites

Runtime consumers remain prohibited by the service-boundary check. A follow-up must
use the captured membership throughout authorization/admission, enforce fresh Discord
permissions and membership around admission/dispatch, and handle expiry, ambiguous results and late completions without replay or false success.
It must also provide appropriate UI outcomes, runtime limits and cleanup scheduling.
Do not remove the API bot token until all current API Discord operations have a complete
replacement. Public library helpers are trusted internal operations, not a security API.

Migrations 0006 and 0008 extend the recognized `jmodguard-core-1` baseline; start the matching bot
before the API. See [schema/setup and rollback boundaries](DATABASE-BASELINE.md).
Local SQLite tests and the optional disposable remote-libSQL smoke cover storage and
transport contracts. They do not validate live Discord/Bunny operations.
