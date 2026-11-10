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
| SM03: launcher writer authority | `src/db/update_control.rs` | Arming makes local authority `Unconfirmed`; a timely successful acquisition confirms it until the original request's TTL deadline. CAS distinguishes `Renewed`, `Missing`, and `Contended`; only a missing row permits the existing fenced acquisition fallback. Renewal distinguishes an ordinary confirmation from `Reacquired`. The captured prior horizon and post-response expiry checks still reject late success. |
| SM04: Gateway startup and sessions | `crates/bot/src/gateway_budget.rs` and `crates/bot/src/state.rs` | Admission is `Ready` or `Wait` under the existing budget/spacing rules. Startup ends as `IdentifyRecorded` or `Shutdown`; recording is not session readiness. A shard's first READY is `First`; later READY for that shard is `Replaced`, advancing coverage generation and invalidating member-sync scope. Other shards' initial READY does not report replacement. |
| SM05: maintenance recovery | `crates/bot/src/recovery.rs` | Persisted recovery is `Complete` or `Queued`; only acknowledgement of the matching first step removes it and records its done marker in the existing fenced transaction. Retry progress is independently `Ready` or `Retrying`; successful steps reset the counter, while failures retain the existing capped backoff. Lease loss stops the pass. |
| SM06: database bootstrap | `src/db.rs`, `src/db/bootstrap.rs`, and `src/db/guild_data.rs` | The primary core observation is `Fresh`, `Behind`, or `Current`; unsupported lineage/version still returns the existing typed error before writes. Bootstrap work moves from `Required` to `Complete` or `DeferredAtCapacity`. API fencing is `Required` or `PreserveReady`, and readiness is published only after the pool exists. Guild layout is `Empty`, `Older`, `Current`, or `Unsupported`; layout never replaces family/ownership verification. Matching heal generation and checksum select `Current`; otherwise healing remains `Required`. |
| SM07: pooled connections | `src/db.rs` and `src/db_compat.rs` | Checkout owns a `Reusable` connection. Failed/tainted operations mark it `Retiring`; drop consumes either into `Returned`, returning only reusable connections to the idle pool. Shared connection health moves only from `Healthy` to `Tainted`. The caller watch moves from `Waiting` to `Abandoned` without releasing the blocking worker's pool permit early. Remote checkout still resets the prior cursor baton and captures the current authority fences. |
| SM08: transactions | `src/db_compat.rs` | Write state is `Snapshot`, `Unprepared`, or `Prepared`; snapshots cannot promote. Preparation retains restore checks and receipt admission. Handle ownership moves from `Active` to `Completed` exactly once after commit or cleanup. Shared stream observations distinguish `Open`, `ClientInterrupted`, `ProviderClosed`, and `InterruptedAndClosed`; neither observation erases the other. Client interruption permits bounded runtime-owned release, while provider closure suppresses rollback. Lost COMMIT responses still reconcile a primary receipt as `Present` or `Absent`; inability to observe it remains indeterminate. |
| SM09: database growth admission | `src/db_compat.rs` | A sampled reservation has an `Unselected` limit until admission selects a nonzero limit; growth and reclaim policies cannot mix. `CapacityCheck` and `CapacityHealth` independently track required precommit measurement and poisoning. The shared runtime `GrowthGuard` moves from `Open` to `Paused` after an underestimated bound. Original primary measurements, allowances, overflow checks, reclaim bounds and atomic ordering remain in force. |
| SM10: configuration freshness | `crates/bot/src/state.rs` and `crates/api/src/web/state.rs` | Cache loads start on `Replica` and switch to `Primary` after an insufficient floor/revision or a publication race. Primary lineage changes still invalidate before retry. API `ConfigObservation` keeps the accepted version and revision together; only primary confirmation with a matching local write generation replaces it. Cache publication, TTL and invalidation guards retain their existing locks and bounds. |
| SM11: slash registry | `crates/bot/src/state.rs` | A target starts with `FirstPass` and `NeedsInvalidation`. Once admitted it becomes `Invalidated`; failures become `RetryFailed`. Missing cached guilds defer the whole retry set. Only a successful finish for the still-current target acknowledges its version. |
| SM12: private control results | `crates/bot/src/control_server.rs` | `RequestLookup` distinguishes `Ready`, `Pending`, `Foreign`, and `Unknown` for both mutation replay and lookup-only recovery. Only mutation admission can reserve `Pending` and enqueue new work. Lookup-only reads retain their eviction-first behavior; mutation replay retains its prior lookup ordering. Queue rejection forgets the reservation; worker completion retains the existing result/forget rules. A process-local result is not durable effect proof. |
| SM13: authenticated frame replay | `src/control.rs` | Nonce admission distinguishes `Malformed`, `Expired`, `TooFarAhead`, `Replay`, and `Accepted`. Accepted nonces enter the bounded retained set; expiry and oldest-entry eviction remove them. Logical request identity remains separate. |
| SM14: tenant handles and backend readiness | `src/tenant.rs` | Handle lookup is `Missing` or `Cached`; per-store locks still serialize opening, identity/epoch checks precede publication, and the existing bounded cache eviction remains. Evidence is explicitly unavailable because it is `Disabled`, `WaitingForIdentity`, or unsupported on the platform; local/Bunny handles remain separate ready variants. Only the existing read-only local identity wait starts the retry path. Epoch retry admission distinguishes `Schedule` from `AlreadyScheduled`; its generation marker does not claim a task is still running. |
| SM15: core enrollment | `src/db/enrollments.rs` | Observations are `Present` or `Departed`. The stored identity selects `Ignore`, `Confirm`, or `Replace`: older observations and an equal departed timestamp cannot resurrect membership. Healthy confirmation remains write-free except for an existing dormant nickname request; replacement admits old-scope erasure and invalidates old work in the same transaction. |
| SM16: positive enrollment repair | `crates/bot/src/enrollment_repair.rs` | The sweep cursor is `Beginning` or `After`; a bounded page is `More` or `Complete`. Only a successfully processed page advances the cursor. Completion resets it and applies the existing sweep pause. Retry admission remains independent; omissions and errors never retire membership. |
| SM17: tenant membership scope | `src/db/guild_stores.rs` and `src/db/guild_data/tenant_logs.rs` | Placement admission is `Current`, `StaleEnrollment`, or `ErasurePending`. Stored membership is `Active` or `Closed`; each operation explicitly requires active membership or includes closed membership. Captured identity, placement and edit revision are checked in the data transaction. Closing removes log mappings atomically and cannot reopen or adopt a new enrollment. Existing tuple adapters preserve callers' contracts. |
| SM18: departed guild erasure | `src/tenant.rs` and `src/db/guild_data/erasure.rs` | `ErasurePhase` names close, evidence cancellation, evidence cleanup, case purge, scope removal, directory cleanup, placement removal, and job completion. Bounded operations repeat their stage while data remains. `ErasureProgress` distinguishes `Remaining` from `Complete`; only verified empty bindings skip to directory cleanup. Stages are process-local: restart rechecks the durable job and actual stored prerequisites. The fairness cursor advances before I/O; newer active directory data and unavailable evidence remain protected by their existing guards. |
| SM29: nickname reconciliation | `src/db/nickname_jobs.rs` and `crates/bot/src/nickname_worker.rs` | Scheduling is `Pending`, `Dormant`, or `Terminal`. Confirmation is independently `NotRequired` or `Required` by the retained outcome-unknown marker; dormant work can still require confirmation. An absent marker does not prove a remote outcome. Only `Dormant`/`Terminal` are stop transitions; successful acknowledgement removes the exact request. All transitions retain membership, payload, generation and attempt checks. Unknown stored scheduling values remain readable but cannot dispatch. |
| SM75: REST retry and cancellation | `crates/bot/src/discord.rs` | Retry admission is `Open` or `Stopped`, independently of the `Ready`/`Until` deadline and failure count. Transient failures keep the existing capped jitter; they never reopen a stopped owner. Caller-owned resets and the existing `DiscordCall::Finished`/`Cancelled` distinction remain unchanged. Cancellation or timeout never grants permission to resend a mutation. |
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

Authority/Gateway/recovery/cache coverage retains the existing launcher expiry,
late-renewal and missing-row recovery tests, Gateway budget
tests, interrupted maintenance recovery, and configuration rollback/publication
race tests. Recovery markers keep their newline encoding. The refactor changes
no launcher behavior or required capability revision.

Storage/control/tenant coverage retains core and guild schema rejection and
migration tests, API bootstrap fencing, settings-heal idempotence, bounded growth
and reclaim tests, control result eviction, tenant cache bounds, read-only local
evidence identity recovery, and refusal to treat missing owned resources as
erasure proof. Core schema state carries the version already read under the
primary transaction; migration selection no longer repeats that same query.

Enrollment and erasure coverage exercises reordered observations, write-free
confirmation, atomic invalidation, partial sweep failures, stale edit revisions,
closed-scope refusal, interrupted activation, restart after core cleanup failure,
cancelled capture settlement, missing backend/store refusal, and protection of a
newer active enrollment. Existing REST tests exercise backoff and unauthorized
stops; these local fixtures do not establish live Discord behavior.
