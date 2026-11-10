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
| SM30: moderation mirrors and observation ordering | `src/db/moderation.rs` | A ban capture is unknown, current, or changed; only a current capture permits deadline binding. Mirror reason distinguishes the exact Gateway placeholder from other text/absence. Keep/replace decisions retain operation, audit and observation-order guards; historical deliveries insert inactive rows. Audit matching selects exactly one original or retained observation; a split replaces only an older active row of the same origin. Mirror mutation is independent of delivery acknowledgement, so an existing result may still commit. Scoped unban and warning-clear SQL retain their existing guards. Mirrors never prove Discord state or merge case identities. |
| SM31: timed-ban expiry | `src/db/moderation.rs` and `crates/bot/src/action_worker.rs` | Deadline matching is `Current` or `Stale`; only a confirmed consumption constructs `ConsumedExpiry` for dispatch. Existing final `Current`/`Superseded`/`Blocked` checks remain. Retry selection follows the primary search, readiness is `Waiting` or `Due`, and a scan distinguishes `Incomplete` from `Exhausted` before forgetting idle retry entries. Consuming a deadline never proves an unban or permits restoring it after an uncertain outcome. |
| SM32: case settlement and removal | `src/db/guild_data/cases.rs` | Payloads are `Live` or `Removed`; absence remains distinct. Removal is terminal and retains replay identities. Settlement accepts only `FinalCaseOutcome` and changes only an `Attempted` case; already-final outcomes remain unchanged. SQL immutability, captured membership, and atomic child cleanup remain authoritative. |
| SM33: case history and source ownership | `src/db/guild_data/case_history.rs` and `case_sources.rs` | History requires a `Live` parent and distinguishes exact-payload replay from note intent, preserving the first committed timestamp. Source slots are available or occupied; an existing association is complete or requires staged-audit/retention repair. Late links to removed parents retain identity only. Sequence/budget checks, source conflicts, and receipt-free completed retries stay in their original transactions. |
| SM34: application/audit rendezvous | `crates/bot/src/state.rs` and `cases.rs` | A matched pair records whether its consumed half came from the audit or application map. `DeferredAuditLink` owns a quarantined pair and its existing expiry. Matching consumes the exact half before linking; deferral finalizes it before deduplication, and duplicate deferral does not extend its lifetime. Independent bounded maps, oldest-match selection, source conflicts and exact-token finalization remain unchanged. |
| SM35: Gateway echo and observation ownership | `crates/bot/src/state.rs` | Captured echoes are `Unowned`, `First`, or `Repeated`, with operation identity independent of those states. Each dispatch's shared suppression moves from `Active` to `Released`; even a consumed capture remains revocable by that dispatch. Observation registration moves once from `Registered` to `Released`, preserving exact peer removal and completion/drop retry behavior. Unban clears remembered echoes while pending newer tokens survive. |
| SM36: manual audit attribution retry | `crates/bot/src/state.rs` | Audit validity is `Live` or `Invalidated`, independent of expiry and retry ownership. An `Initial` pass can claim the existing atomic retry slot after a transient failure and become `Retrying`; the owned guard releases that slot on completion or cancellation. Original Acquire/Release/CAS ordering, observation/window rechecks, backoff, deadlines and error classification remain intact. |
| SM37: durable Gateway delivery inbox | `src/db/gateway_ban_deliveries.rs` and `crates/bot/src/gateway_ban_persistence.rs` | `RetainedDelivery` becomes `SavedDelivery` only after a confirmed canonical save; recovery constructs saved ownership from durable rows. Core projection is `Pending` or `Applied`, independently of `Pending`/`Complete` case acknowledgement. Named recovery rows preserve round-robin order; snapshots never bypass primary scope, fence, audit-proof or applied-result rechecks. Callback cancellation retains the detached owner, and persistence retries never resend Discord. |
| SM38: evidence journal cancellation and settlement | `src/db/guild_data/case_evidence.rs` and its local/Bunny consumers | `EvidenceCancellation` is `NotCancelled` or `Cancelled`, independently of the existing journal phase. A closed-membership snapshot can add cancellation but never clear it. `CaptureSettlement` publishes a verified stored result only after active membership, no cancellation and a live parent pass the original guards; all other settled captures require cleanup. Cancellation never proves writes stopped, and deletion/purge still require the existing settlement and absence proofs. |
| SM39: local evidence resource and settlement | `src/evidence_transfer.rs` | `ResourceOpenMode` distinguishes initialization from reopening an existing identity. Only a successful OS lock creates `LockedResource`, retained at the original worker and serving boundaries. Parent traversal moves from `Present` to `Missing`, skipping subsequent parent-directory checks. `VerifiedCapture` is produced only after the existing hash and durability checks; recovery distinguishes `Unverified` from `Verified`. Cancellation skips verification, and missing/corrupt captures settle to cleanup before any verification error is returned. Existing reservations never reread the source; deletion acknowledgement still follows verified absence. |
| SM40: Bunny single-use upload acceptance | `src/db/guild_data/bunny_transfer.rs` and `src/bunny_evidence.rs` | `BunnyAcceptance` is `Unresolved` or `Accepted`, independently of checksum retention and journal phase. Only the original completed-source/HTTP201 path records durable acceptance. Fresh binding and acceptance checks produce `AcceptedTransfer` before recovery, serving or cleanup can proceed. Cancellation may scrub the checksum but never resolves an uncertain PUT; timeout, early 404 and matching bytes cannot authorize another upload or deletion. Existing SQL identity and single-attempt guards remain authoritative. |
| SM41: generic automod escalation | `crates/bot/src/automod.rs` | `RuleRetention` distinguishes retained and expired buckets; observation updates the original event time and strike count together. Each threshold is `Available` or `Reserved`, independently of action success. Reservation still precedes highest-severity selection, and explicit release retains the original caller contract. |
| SM42: exact repeated-text reports | `crates/bot/src/text_spam.rs` | Admission keeps its existing unchanged/admitted/matched/rejected states. Report cooldown moves from `ReadyAt` to `ReservedUntil`; only a retryable completion with the same token and deadline releases it. Matching, expiry, edit timestamps, watermarks and enforcement proof checks retain their original boundaries. |
| SM43: exact-image analysis and reporting | `crates/bot/src/image_spam.rs` | Existing admission, analysis-gate, completion and retry states remain. `WindowOwner` distinguishes a match reservation from an incomplete notice, independently of its deadline; parked authors preserve both. A verified match supersedes an incomplete notice. Own-delete results are `Unconfirmed` or `Succeeded`, independently of the Gateway echo and proof lifetime. |
| SM44: attachment hash handoff | `crates/bot/src/attachment_burst.rs` | Staged identities are `AwaitingHash` or `FallbackRecorded`; exact generation checks still gate fallback and hash promotion. Handoff ownership is `Inert`, `Staged`, `HandedToJob`, or `ResolvedIncomplete`. Dropping staged ownership resolves incomplete work, while the existing job guard resolves remaining identities on drop. Metadata fallback never becomes exact-image proof. |
| SM45: attachment review ownership | `crates/bot/src/attachment_burst.rs` | Atomic removal from the pending review map produces `ClaimedReview`, carrying its original review ID and captured membership. The existing failure paths consume that claim to restore the same review; successful release/ban consumes it. Timeout results, review delivery, final decisions and expiry remain independent. |
| SM46: spam effect progress | `crates/bot/src/spam_actions.rs` | Each delete and the author's ban independently move from `Ready` to `InFlight` to `Completed`. Unsuccessful settlement releases an in-flight reservation; a late failure cannot erase completed success. Claims and settlements retain the existing locks, expiry refresh, missing-entry no-ops and retry authority. |
| SM47: automod delete correlation | `crates/bot/src/automod_delete_marks.rs` | REST confirmation (`Pending`/`Confirmed`), Gateway receipt (`Unseen`/`Observed`) and report progress (`Pending`/`Settled`) remain independent for both retained attempts and bounded overflow. Existing sibling ordering, bulk ownership and replay guards decide when attribution settles; none of these observations substitutes for another. |
| SM48: bulk delete-report ownership | `crates/bot/src/automod_delete_marks.rs` | Bulk delivery is `InFlight`, `Delivered`, or `Ambiguous`; failed sends remove the row and may release a deferred report. `DeferredPlainReport` distinguishes no request from a request with an optional snapshot. Automod settlement remains a separate payload with replay precedence. Delivered/ambiguous outcomes discard replay; REST attempts independently govern the ambiguous suppression window. |
| SM49: log delivery and deduplication | `crates/bot/src/logging.rs` | Existing admission permits and delivery outcomes remain explicit. Audit attribution moves from `Unresolved` to `Claimed` under generation and receipt guards. Edit identity is `Current`, `PreviouslyReported`, or `New`; timestamp-less refreshes never enter the state. Snippet origin advances from `Unmarked` to `Webhook`, preserving existing expiry and consumption. |
| SM50: snapshot ingress | `crates/bot/src/snapshot_capture.rs` | Byte admission is `Reserved` or `Refused`; the receiver moves once from `Ready` to `Started`. Deletion knowledge is `Unseen`, `Exact`, or `Possible`; either remembered deletion blocks capture. `CoverageEpoch` advances on the existing loss paths. Queue bounds, tombstone-before-enqueue ordering, Bloom rotation and physical-store loss accounting are unchanged. |
| SM51: rolling snapshots | `src/db/guild_data/snapshots.rs` | `FieldSupply` moves from `Inherited` to `Supplied` only when the incoming field is present; a distinct edit resets that provenance, while equal-timestamp conflicts keep the first value. `SnapshotContinuity` is `Continuous` or `Gap` under the current epoch. Public flags and SQL retain their encoding; existing outcomes, health states, expiry and transactional admission remain authoritative. |
| SM52: command cooldown | `crates/bot/src/state.rs` and `crates/bot/src/commands/mod.rs` | `CommandCooldowns` returns `Allowed` or `CoolingDown` from the shared guild/user/command timestamp. Admission records time before execution and survives command failure. Zero-duration bypass, elapsed-time boundary and the existing read/insert sequencing remain unchanged. |
| SM53: slash response lifecycle | `crates/bot/src/commands/mod.rs` | Defer completion selects `Initial` or `DeferredPublic`; existing `Public`/`Ephemeral`/`Withdraw` policy then selects the response operation. `Confirmation` distinguishes needed replies from an already-visible effect. Deferred visibility stays public, withdrawal keeps its delete-to-edit fallback, and all existing writer-cutoff races remain. |
| SM54: OAuth handshake and session rotation | `crates/api/src/web/mod.rs` | `OAuthHandshake` issues the existing browser state and allowed return target. Only `ValidatedOAuthCallback` supplies an exchange code after the cookie/query checks. `RotatedLoginSession` is produced only after successful ID rotation and consumed by credential storage. Callback cookies still clear on every response; old coordinators retire before rotation, and logout retains its retirement/flush ordering. |
| SM55: session refresh and retirement | `crates/api/src/web/mod.rs` and `state.rs` | A coordinator moves from `Active` to `Retired` with its first retirement time. Payload clearing, tombstones and Arc/lock guards preserve the single refresh owner. `RefreshHold` keeps the deadline separate from `Unavailable`/`RateLimited` cause; successful authorization releases the hold/backoff, while transient errors never retire the session. |
| SM56: authorization freshness and shared lookup | `crates/api/src/web/mod.rs` and `oauth.rs` | `GuildSnapshotAge` distinguishes `Fresh`, `RateLimitFallback` and `Expired` under the original three/five-second limits. `DisplayIdentitySource` controls cache renewal independently. Shared lookup publishes `Pending`/`Settled` updates; its guard is `Running`/`Finished`, with unchanged cancellation settlement and watch-send behavior. Display caches never become mutation authority. |
| SM57: OAuth cooldown persistence | `crates/api/src/web/oauth.rs` and existing `discord_retry.rs` codec | Existing finite/process-lifetime/body-pending cooldown states remain. The gate owns the guarded body hold. `RetryDeadlineSnapshot` binds the durable map to its generation; persistence is `Recorded` or `WaitingForWriter`, while a changed generation retains the existing bounded retry. Route/global bounds, extension rules, restart encoding and session refresh holds remain separate. |
| SM58: preview capability | `crates/api/src/web/preview.rs` and `frontend/app.js` | Presented preview state is `Absent`, `Active`, or `Expired`; expired credentials return the existing refusal before session fallback. Grant lifetime controls pruning and scheduled removal. The tab keeps an `absent`/`captured` capability independently of browser storage and an `unchecked`/`unavailable` display state. Fragment persistence, explicit exit, read-only masks and fresh owner permission checks are unchanged. |
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

Moderation mirror coverage retains existing tests for command/Gateway races,
redelivery, literal placeholder reasons, delayed historical bans, audit ambiguity,
retained observation splits, newer unbans and exact capture/deadline binding.
Query ordering, SQL predicates, observation windows and public results remain unchanged.

Gateway ownership coverage retains existing tests for matched-half consumption,
deferred quarantine, suppression release before and after capture, overlapping
attempts, observation cancellation, audit invalidation, and single-owner retries.
The refactor preserves shared-flag identity and atomic ordering; private test
finalization calls and one expired-entry fixture adapt to the types without
changing their assertions.

Durable inbox coverage retains cancellation during initial lookup/save, restart
and partial-case recovery, canonical audit replay, lost commit responses, and
out-of-order ban/unban tests. Only the private save fixture adapts to the saved
owner type; SQL, serialized payloads, assertions and retry timing remain unchanged.

Evidence journal coverage retains late-write cancellation, rollback, reservation
replay, cleanup, closed-scope purge and local/Bunny settlement tests. The two
cancellation assertions now use the enum without changing their expected result.
SQL transitions, provider acceptance proofs and resource-lock ordering are unchanged.

Local transfer coverage retains real streaming and receipt recovery, failed SQL
settlement and reopen, corruption, private-path guards, and cancellation while a
blocking writer owns the OS lock. Existing separate-process exclusion and cleanup
assertions are unchanged; no test helper or deadline adaptation is required.

Bunny transfer coverage retains late writes after timeout, lost acceptance,
readback and SQL recovery without another PUT, checksum scrubbing, and deletion
absence checks. The remote lost-commit assertion now names `Unresolved`; its
expected outcome and all other assertions remain unchanged.

Detection and delete-correlation coverage retains the existing exact-match,
replacement, stale completion, parked watermark, incomplete-notice, own-delete
proof, handoff drop, review claim/restore, membership replacement and overlapping
REST/Gateway tests. Private fixture access follows the new types; expected
outcomes, timeouts, SQL, wire formats and remote-effect retry rules are unchanged.

Logging and snapshot coverage retains bulk ambiguity, deferred replay precedence,
expiry/capacity, edit identity, partial fields, audit consumption, tombstone overflow,
queue saturation, coverage gaps, durable snapshot expiry and capacity tests.
Command coverage retains shared dispatch, visible-effect suppression and cutoff
fixtures. Timestamp-less edit states were unreachable behind the existing guard;
removing them also removes fingerprints that could not affect a reported edit.
Private fixture adapters preserve assertions; public payloads and SQL are unchanged.

Authentication coverage retains restart-safe CSRF cookies, failed rotation,
serialized refresh, temporary holds, stale-handle retirement and delayed-save
fences. Existing three/five-second authorization, display fallback, cancelled
lookup, delayed 429 body and persisted cooldown cases remain unchanged. Preview
coverage includes API write refusal and browser-VM storage failures, reload and
explicit exit. Private session fixtures use named states without changing their
behavioral assertions; these local boundaries do not prove live Discord or browser behavior.
