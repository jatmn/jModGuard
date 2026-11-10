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
| SM19: member presence and retention | `src/db/guild_data/member_directory.rs` | `MemberPresence`, `BanPresence`, and `MemberRetention` keep current membership, current ban, and retained ban history independent. Stay transitions are `Keep`, `Rejoin`, or `Replace`; observed and batched ingestion retain their distinct timestamp/provenance guards. A retained ban can leave an absent member's stay pending for a later dated departure. Trusted versus checked departures and observed versus uncertain ban order remain explicit inputs. SQL/JSON boolean encodings remain unchanged. |
| SM20: pending ban attempts and recovery | `crates/bot/src/cases.rs`, `crates/bot/src/member_directory.rs`, and their guild-data owners | Attempt recording is `Unconfirmed` or `Confirmed`; marker lookup is `Missing` or `Awaiting`. Repair disposition stays independent from the remote outcome. Exact-tag recovery distinguishes `Missing`, `Unique`, and `Ambiguous` and settles only a unique case. Original payload, enrollment, newer-unban guards, and uncertainty handling remain unchanged. |
| SM21: member import phase/status | `src/db/guild_data/member_directory.rs` | `SyncState` names queued, running, waiting, paused, attention, and complete scheduling; `SyncRunKind` keeps initial import and reconciliation distinct. Phase advancement stays on `MemberSyncPhase`, while `ImportCompletion` distinguishes importing, waiting for local history, and complete. Controls retain saved-state and expired-lease guards. Unknown stored status strings remain readable without acquiring work. |
| SM22: seed admission and coverage gaps | `src/db/guild_data/member_directory.rs`, `crates/bot/src/member_sync.rs`, and `crates/bot/src/state.rs` | Seed admission is `Available` or `Consumed`; the seed itself is `Missing`, `Importing`, or `Complete(timestamp)`. READY observations are `First`, `Unchanged`, or `Replaced` and are acknowledged only after status/gap persistence. Pending gaps carry an explicit member/ban verification kind; delivery remains pending after a failure. Exact generation checks still prevent an older acknowledgement or reconciliation from clearing newer uncertainty. |
| SM23: audit recovery and association | `src/db/guild_data/member_directory.rs` | Recovery scheduling remains independent from initial completion. A committed audit page selects `Continue`, `StartUnbans`, `Restart`, or `Complete`, retaining cursor, lease, generation, and follow-up-request checks. Audit association is `Linked`, `AwaitingOwnSource`, or `Foreign`; an own audit without an exact source remains an immutable unresolved fact. SQL transactions and the existing exact-link consumption remain unchanged. |
| SM24: targeted checks and cleanup | `src/db/guild_data/member_directory.rs` | A saved check's revision, reason, request time, and stay identity must all match. Settlement is `Apply`, `DismissSuperseded`, or `Stale`; only the original ambiguous departure may be dismissed after its row changes. Cleanup history is `Pending` or `Ready`, selecting a member check, a history wait, or a ban check. Final deletion still rechecks membership, ban/retention, revision/deadline, coverage, and pending work in the transaction. |
| SM25: request pacing and worker scheduling | `src/db/member_sync_pace.rs` and `crates/bot/src/member_sync.rs` | A request slot keeps its deadline, lease, and generation separate; admission is `Ready`, `Paced`, or `Leased`. Reservation is `Granted`, `Busy`, or `Unavailable`; only a granted claim permits dispatch. Worker turns distinguish import and cleanup, with separate fair cursors and explicit scan work/wait states. Unknown commit outcomes retain the conservative lease/deadline and never authorize an immediate HTTP retry. |
| SM26: member case-index backfill | `src/db/guild_data/member_directory.rs` | Backfill progress is `Pending` or `Complete`, independent of its retained cursor. Projection writes and progress commit together. Empty or terminal pages complete the existing import/history prerequisites; captured gap generations still prevent clearing newer uncertainty. Public completion booleans remain adapters. |
| SM27: rejoin-note delivery | `crates/bot/src/logging.rs` and `crates/bot/src/member_rejoin.rs` | Admission distinguishes `Ready`, `InFlight`, and recently `Delivered`. Delivery outcome is `Delivered` or `Undelivered`; the attempt guard moves from `Active` to `Finished`, and dropping an active guard records undelivered work. Existing enrichment `Ready`/`NoNote`/`Retry`, durable exact-stay obligations, separate bounded cache indexes, expiry, and in-flight eviction protection remain intact. |
| SM28: administrative action queue | `src/db/bot_actions.rs` and `crates/bot/src/action_worker.rs` | Saved queue states are named while public strings and unknown-value reads remain compatible. Completion distinguishes trusted reports from deadline-aware dispatch results. Live membership is matching or mismatched; mutation progress moves from `Validating` to `Started` at the existing boundary. Failed validation remains separate from an uncertain started mutation. Claim consumption, exact identity checks, cancellation, expiry, supersession, and atomic receipt/effect SQL retain their guards. |
| SM29: nickname reconciliation | `src/db/nickname_jobs.rs` and `crates/bot/src/nickname_worker.rs` | Scheduling is `Pending`, `Dormant`, or `Terminal`. Confirmation is independently `NotRequired` or `Required` by the retained outcome-unknown marker; dormant work can still require confirmation. An absent marker does not prove a remote outcome. Only `Dormant`/`Terminal` are stop transitions; successful acknowledgement removes the exact request. All transitions retain membership, payload, generation and attempt checks. Unknown stored scheduling values remain readable but cannot dispatch. |
| SM31: timed-ban expiry | `src/db/moderation.rs` and `crates/bot/src/action_worker.rs` | Deadline matching is `Current` or `Stale`; only a confirmed consumption constructs `ConsumedExpiry` for dispatch. Existing final `Current`/`Superseded`/`Blocked` checks remain. Retry selection follows the primary search, readiness is `Waiting` or `Due`, and a scan distinguishes `Incomplete` from `Exhausted` before forgetting idle retry entries. Consuming a deadline never proves an unban or permits restoring it after an uncertain outcome. |
| SM32: case settlement and removal | `src/db/guild_data/cases.rs` | Payloads are `Live` or `Removed`; absence remains distinct. Removal is terminal and retains replay identities. Settlement accepts only `FinalCaseOutcome` and changes only an `Attempted` case; already-final outcomes remain unchanged. SQL immutability, captured membership, and atomic child cleanup remain authoritative. |
| SM33: case history and source ownership | `src/db/guild_data/case_history.rs` and `case_sources.rs` | History requires a `Live` parent and distinguishes exact-payload replay from note intent, preserving the first committed timestamp. Source slots are available or occupied; an existing association is complete or requires staged-audit/retention repair. Late links to removed parents retain identity only. Sequence/budget checks, source conflicts, and receipt-free completed retries stay in their original transactions. |
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

Member-directory coverage retains production-path tests for late and ambiguous
departures, retained history across unban/rejoin, bounded page ingestion,
revision/stay-fenced target checks, history-gated cleanup, case-index backfill,
and rejoin delivery cancellation, retry, expiry, and capacity bounds. The
refactor does not change migration SQL or add Discord requests.

Import/recovery coverage retains existing tests for one-time seed admission,
completed-control no-ops, stalled pagination, captured-claim rejection, audit
follow-up requests, missing metadata, delayed backfill and newer gaps,
own-audit association races, and retry after failed gap persistence. Test
representation changes retain the same behavioral assertions and deadlines.

Ban recovery and pacing coverage retains existing tests for stalled attempt
recording, exact-tag recovery and unmatched rejection, newer unbans, shared request
leases, stale claim completion, rate-limit extension, and idle-scan deadlines.
The refactor preserves the same SQL, request order, and test assertions.

Queue and expiry coverage retains existing production-path tests for duplicate
claims, supersession, membership replacement, timeout before/after dispatch,
late completion, receipt failure, expiry consumption, newer-ban protection,
and bounded fair retry scheduling. Public status assertions remain unchanged.

Case coverage retains existing tests for one-way attempt settlement, immutable facts,
removed-key replay, concurrent history append/removal, note-intent retries,
source-link conflicts, late links, staged audit consumption, retention repair,
and atomic cleanup rollback. No migration, API shape, assertion, or deadline changes.
