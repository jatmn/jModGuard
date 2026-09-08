# Durable action storage (inactive)

The library has a queue for typed unban and log-channel requests. Neither service admits
these requests, dispatches them, schedules cleanup or exposes them through HTTP/the WebUI.
Existing unban and log-channel validation still run directly in the API using its bot
token. The nickname outbox is separate and continues to operate as before.

## Storage contract

- Admission accepts positive guild/actor IDs and validated typed payloads. No SQL,
  executable code, arbitrary URLs or credentials are part of the action protocol.
  Snowflakes accept string or integer input and serialize as strings; an optional
  channel ID also accepts/serializes null for a clear. Claim tokens are never serialized.
- A request key is `<positive Unix seconds>_<nonce>` (at most 64 ASCII characters).
  Retrying the same guild/key/actor/payload returns the existing receipt. A conflicting
  actor or payload fails. Unknown remote commit outcomes require the **same** key.
- Primary transactions enforce a per-guild pending limit and one active action per
  target (user for unban, event for log configuration). Uncertain work occupies both
  limits. Read and cancellation helpers use exact guild scope; this is not caller
  authorization or tenant credential isolation.
- Claims are exclusive. Beginning consumes the claim once before any future dispatch;
  both begin and completion match the stored guild, actor, payload, expiry and claim.
  Expired pending/claimed work cannot begin. Expired running work becomes uncertain and
  is never automatically retried. A known late result can resolve its original claim.
- Completion commits its status and local unban record/audit or log-setting change in
  one transaction. This records a **trusted caller's reported result**, not proof that
  Discord performed an operation. SQL receipts cannot make a remote Discord action
  atomic or guarantee exactly-once delivery. A future worker must report outcomes honestly.
  Direct writes and queued completion share log-mapping/version semantics: a clear
  advances configuration versions/revisions only when a mapping is actually deleted.
  An absent-mapping queued clear still commits its completed action state atomically.
- Pending work can be cancelled within its guild. A direct log-channel upsert/delete
  supersedes active queued work for that guild/event atomically, so late completion
  cannot overwrite the newer setting. It cannot undo a remote effect already performed.
  Clearing an absent mapping still supersedes queued changes, but does not advance
  configuration versions or revisions when no mapping was deleted.
- Cleanup removes only a bounded batch of resolved operational history. Age uses the
  later of completion and request timestamp. It never purges unresolved uncertain work
  or retained moderation audits. Deletions and a monotonically increasing admission
  cutoff commit together; old keys remain invalid after pruning, restart, clock rollback
  or a later retention increase. Extant matching receipts can still acknowledge retries.

`bot_actions::Limits` supplies storage-only defaults: 32 active requests per guild,
300-second maximum age, seven-day resolved-history retention and 128-row cleanup batch.
They are library parameters with validation, **not runtime settings or active schedules**.
No queue maintenance task or new dependency is added. A future activation must expose
appropriate controls and run bounded housekeeping; this slice creates no runtime queue
traffic. Protocol forward clock tolerance is five minutes. Poll-time expiry processing
is capped at 64 rows per state group and recent history reads at 50 rows.

## Activation prerequisites

Runtime consumers remain prohibited by the service-boundary check. A follow-up must
complete enrollment identities and departure/rejoin fencing, capture the authorized guild
scope, enforce fresh Discord permissions and membership around admission/dispatch, and
handle expiry, ambiguous results and late completions without replay or false success.
It must also provide appropriate UI outcomes, runtime limits and cleanup scheduling.
Do not remove the API bot token until all current API Discord operations have a complete
replacement. Public library helpers are trusted internal operations, not a security API.

Migration 0006 extends the recognized `jmodguard-core-1` baseline; start the matching bot
before the API. See [schema/setup and rollback boundaries](DATABASE-BASELINE.md).
Local SQLite tests and the optional disposable remote-libSQL smoke cover storage and
transport contracts. They do not validate live Discord/Bunny operations.
