# Explicit workflow states

State belongs to the existing production owner. Independent dimensions stay
separate: holding authority does not mean accepting new work, and a queued
operation does not mean its remote effect is known. Storage encodings and wire
strings remain compatible; these types do not add a state-machine framework.

The inventory identifiers below refer to the source audit of main
`ba1245fe43f15fffd81ebdaf0ab3ce58a293d077`. The table records the implemented families; the complete inventory is still
in progress.

| Family | Production owner | States and guarded transitions |
|---|---|---|
| SM01: process lifecycle | `crates/runtime/src/lib.rs` | Readiness is `Unavailable` or `Ready`, independently of liveness. Shutdown moves from `Running` to `Requested` and stays there even with no subscribers. Launcher child-ready files remain a separate bootstrap proof. |
| SM02: active bot | `crates/bot/src/activation.rs` | Lease ownership is `Waiting` or `Held`; its horizon is `Unbounded` or `ConfirmedUntil`. Gateway readiness, intake (`Accepting`/`Draining`), recording (`Open`/`CutOff`), and recovery (`Idle`/`Running`) are separate dimensions. A failed/expired lease closes authority; draining closes intake while existing work may settle until cutoff. Late renewal cannot reopen recording. |
| SM07: pooled connections | `src/db.rs` and `src/db_compat.rs` | Checkout owns a `Reusable` connection. Failed/tainted operations mark it `Retiring`; drop consumes either into `Returned`, returning only reusable connections to the idle pool. Shared connection health moves only from `Healthy` to `Tainted`. The caller watch moves from `Waiting` to `Abandoned` without releasing the blocking worker's pool permit early. Remote checkout still resets the prior cursor baton and captures the current authority fences. |
| SM08: transactions | `src/db_compat.rs` | Write state is `Snapshot`, `Unprepared`, or `Prepared`; snapshots cannot promote. Preparation retains restore checks and receipt admission. Handle ownership moves from `Active` to `Completed` exactly once after commit or cleanup. Shared stream observations distinguish `Open`, `ClientInterrupted`, `ProviderClosed`, and `InterruptedAndClosed`; neither observation erases the other. Client interruption permits bounded runtime-owned release, while provider closure suppresses rollback. Lost COMMIT responses still reconcile a primary receipt as `Present` or `Absent`; inability to observe it remains indeterminate. |
| SM11: slash registry | `crates/bot/src/state.rs` | A target starts with `FirstPass` and `NeedsInvalidation`. Once admitted it becomes `Invalidated`; failures become `RetryFailed`. Missing cached guilds defer the whole retry set. Only a successful finish for the still-current target acknowledges its version. |
| SM13: authenticated frame replay | `src/control.rs` | Nonce admission distinguishes `Malformed`, `Expired`, `TooFarAhead`, `Replay`, and `Accepted`. Accepted nonces enter the bounded retained set; expiry and oldest-entry eviction remove them. Logical request identity remains separate. |
| SM29: nickname reconciliation | `src/db/nickname_jobs.rs` and `crates/bot/src/nickname_worker.rs` | Scheduling is `Pending`, `Dormant`, or `Terminal`. Confirmation is independently `NotRequired` or `Required` by the retained outcome-unknown marker; dormant work can still require confirmation. An absent marker does not prove a remote outcome. Only `Dormant`/`Terminal` are stop transitions; successful acknowledgement removes the exact request. All transitions retain membership, payload, generation and attempt checks. Unknown stored scheduling values remain readable but cannot dispatch. |
| SM78: CPU sampling | `crates/runtime/src/cgroup.rs` | `AwaitingBaseline` becomes `Tracking` after one observation. Missing counters reset the baseline; decreasing counters establish a new baseline. A zero elapsed interval preserves the previous sample. Only a positive elapsed interval with a nondecreasing counter yields a percentage. |

The process shutdown transition fixes a lost-signal case: Tokio watch `send`
does not retain a value when there are no receivers. The latch uses
`send_replace` so a later subscriber still observes `Requested`. Regression
tests exercise both a gap without waiters and simultaneous/current/future
waiters through the production latch, without sending OS signals to the test
runner.

Existing tests continue to exercise lease renewal and expiry, cutoff races,
slash retry subsets, nickname membership replacement and uncertain edits,
frame replay protection, and CPU baseline resets. The nickname compatibility
test reads both the standalone status and bundled configuration snapshot from
SQLite and verifies the existing JSON strings. These fixtures do not prove
live Discord, Bunny or remote-database behavior.

Database preparation and receipt identity remain separate: local writes check
the restore fence without remote receipts, and the batched remote path prepares
receipt SQL before the caller executes it in the same transaction. Preparation
is not proof of durable acceptance. Only a confirmed commit or the existing
receipt reconciliation publishes its outcome; an unresolved commit still
advances the conservative configuration read floor. Capacity and writer checks
remain at their existing transaction boundaries.

Database regression coverage exercises real SQLite snapshot promotion and
rollback, pooled connection retirement, and loopback Hrana cancellation,
provider-closed streams, prepared statements, and connection replacement. The
separate disposable libSQL gate covers receipt recovery after a lost commit
response. Loopback Hrana models provider behavior and is not a live Bunny test.
