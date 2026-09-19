# Queued administrative actions

The API queues typed unban and log-channel requests and returns HTTP 202 with a receipt.
The bot executes them using its existing Discord client. The WebUI reports pending work
and provides an Actions tab for manual status refresh and pending cancellation. Acceptance
is not completion. The API needs OAuth credentials but no bot token.
The nickname outbox remains a separate active path; see
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
  atomic or guarantee exactly-once delivery. The dispatch library reports outcomes as described below.
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

`bot_actions::Limits`, configured through `[actions]` in both services, supplies defaults: 32 active requests per guild,
300-second maximum age, seven-day resolved-history retention and 128-row cleanup batch.
The bot runs one sequential task with poll and cleanup timers, skipping missed ticks.
Polling defaults to five seconds and cleanup to one hour. No new client or dependency is
added; the worker waits for a real Gateway bot identity before draining. Protocol forward clock tolerance is five minutes. Poll-time expiry processing
is capped at 64 rows per state group and recent history reads at 50 rows.

## Membership-bound storage

Migration 0008 binds every new action to an active `guild_enrollments` identity and join
timestamp. Retirement/replacement cancels pending/claimed work and marks running work
uncertain in the same transaction as the membership change. Matching/stale membership
observations remain write-free. A former membership's uncertain outcome remains retained
and does not block a new membership's target. Direct log writes do not cancel that old
uncertain record; erasure/reconciliation remain separate work.

Use `enqueue_enrolled_bot_action`, `enrolled_bot_action`, `recent_enrolled_bot_actions`
and `cancel_enrolled_bot_action` for API callers that have captured membership during
authorization. Each verifies guild, enrollment ID and join timestamp inside the primary
transaction before admitting/replaying, reading or cancelling. Changed identity returns
`EnrollmentChanged`; failed membership verification returns `EnrollmentUnverified` without
exposing database details. These values identify a scope; they grant no user permission.

The existing guild-ID helpers remain trusted internal operations: they resolve current
membership at the operation, without protecting an earlier authorization snapshot. They
must not replace captured-scope helpers in dashboard routes. Status reads hide
old/unbound memberships. `bot_action_dispatch` rechecks running claim identity,
current membership and guild-move fencing at the primary, preserving whether dispatch
was blocked by enrollment replacement or a move. It does not perform Discord validation
or a mutation. Claim selection excludes fenced guilds; expiry of overdue `pending`/`claimed`
work and `running`→`uncertain` still run when `expires_at` has passed, even if no other
guild has claimable work. Local log-channel completion checks the move fence again in the same
transaction as the setting write so a fence raised during Discord channel validation
still fails closed. Unban completion preserves the Discord result but skips its local
ban-mirror and event writes if a move fence was raised after dispatch.

Pre-0008 actions remain unbound. Migration cancels unstarted work, marks running work
uncertain, and keeps resolved/uncertain receipts; no old request is assigned to an existing
membership. Old keys cannot be resubmitted as new work. Start matched bot/API builds,
bot first; rollback needs matched pre-upgrade backup/binaries. Runtime activation adds no schema migration; the current lineage remains steps 0001–0012.

## Dispatch library

`action_worker` provides the bot-side executor and sequential batch drain, scheduled by
`jmodguard-bot`. The API uses only captured admission/read/cancel helpers. The
service-boundary guard rejects unscoped API queue access and bot-token dispatch in the API.

The executor first checks the bot's own join timestamp through REST. When that positive
observation proves that a gateway rejoin was missed, it advances the enrolled membership
and atomically cancels the old, still-unconsumed claim; later requests can bind to the
repaired membership instead of repeating `bot_membership_changed`. Missing or unreadable
REST timestamps remain unavailable failures, not proof of a changed membership.

After that check, the executor consumes the immutable claim on the primary before any
dispatch-capable operation. A copied/stale/modified claim cannot begin again. It then
fetches the acting user's member record and guild roles. Unban requires BAN_MEMBERS; log
configuration requires MANAGE_GUILD. Guild owner and administrator
permissions are honored; installation-owner status grants no dispatch bypass. An active
member timeout denies either action for non-owner, non-administrator actors, even when
their roles retain the required permission; authorization resumes after expiry. No OAuth,
member roster, message cache, new client or additional library is needed. Callers supply
the bot's existing HTTP client and real cached bot identity; placeholder identity is refused.

Immediately before unban, a primary check verifies the running claim and active membership
again. Channel changes verify a text-based channel belongs to that guild. Clear requires no
channel lookup. Final SQL completion rechecks the claim/membership and preserves direct
configuration supersession. Failed validation produces a failed receipt without dispatch.

The configured timeout bounds validation and the Discord mutation. A timeout before invoking
unban is failed; after invocation it is uncertain. Discord's specific Unknown Ban response
is an already-unbanned success, other explicit client rejections are failed (except 408),
and transport/server/timeout ambiguity is uncertain. The worker does not automatically
resend failed or uncertain actions. The underlying SDK's HTTP/rate-limit behavior remains;
this is not a guarantee of exactly one wire request. Once an unban reaches the Discord
mutation boundary, the worker also records one tenant case using the durable action ID as
its application source, preserving the captured enrollment, actor and reason. Succeeded
and uncertain unban cases that may still receive a bot-executor audit entry remember that
identity in-process so it can be linked; `already_unbanned` records the case but does not
register audit pending identity because Discord made no mutation. Unmarked manual/external
unbans still do not. Before
dispatch it registers a bounded one-shot suppression so the matching gateway echo does
not create another case; unmarked manual/external unbans still do. A definitive Discord
rejection releases the marker immediately, while an uncertain result retains it for the
bounded gateway window.

`finish_dispatched_bot_action` shares the existing atomic receipt/effect/audit transaction.
For a successful local log-setting result, it checks expiry after acquiring the primary
writer lock. Expired local work becomes failed without changing mappings or configuration
versions. Confirmed remote unban results may be recorded late in their original membership.
The existing trusted `finish_bot_action` data-layer helper retains its earlier contract;
worker integrations must use the deadline-aware variant.

If receipt persistence fails after Discord was called, local effects roll back together;
the worker reports the error and never resends the action. If completion is rejected
because the action was superseded or its membership ended, the worker warns with the
action ID, reported state/outcome and whether the remote mutation started. It preserves
the recorded cancellation or uncertainty and does not retry the action. Aborting a worker
after begin leaves consumed work running; later expiry processing marks it uncertain. No restart or
claim timeout reclaims it. This favors avoiding duplicate effects over automatic completion.
A missed departure, a permission change after the REST check, or a request already sent
cannot be made atomic with SQL. Replica writer coordination and live Discord validation
remain prerequisites for broader guarantees.

`WorkerLimits` is a trusted library input separate from storage `Limits`: defaults are
four sequential actions per drain and 15 seconds per validation/Discord operation block.
Validation accepts 1–64 actions and 1–120 seconds and runs before consuming a claim.
These are implementation defaults/bounds, not measured capacity or active deployment
requirements. Bot-only `[action_worker]` configuration exposes these limits and timers.
Claim and receipt SQL retain the existing database operation deadlines outside that block.

Local worker tests cover duplicate/modified claims, cancellation/expiry, permission and
membership changes, missed-rejoin repair, failed verification, current-role authorization,
channel supersession, request timeouts, abandoned dispatch, receipt rollback, batch bounds
and late local effects.
The disposable remote-libSQL smoke exercises lost completion responses without resending
and deadline-aware log completion. Mock outcomes do not validate live Discord behavior.

## HTTP, browser and deployment contract

Unban (`POST .../unban`), set log channel (`PUT .../logchannels/{event}`) and clear log channel
(`DELETE .../logchannels/{event}`) require a JSON `request_id` and the loaded page's
`x-jmodguard-enrollment` header. Routes are under `/api/guilds/{guild_id}`. Actor identity
comes from the session, never the body. Every request rechecks Discord permissions.
`GET .../actions` and `DELETE .../actions/{id}` also require the page membership; status
returns up to the latest 50 receipts filtered to the caller's current action permissions.
Only pending work can be cancelled. Global guild-0 paths do not admit actions.

The browser saves the request key, payload identity and membership in session storage
before sending. A lost response retains that key across retries/reloads in the same tab.
It clears it only after a valid receipt or explicit user confirmation. Old-membership,
expired or invalid keys require explicit clearing and a separate reviewed submission;
there is no automatic retry with a new key. Denied session storage prevents submission.
The Actions tab uses manual refresh, with no background polling. Refresh log configuration
or bans after completion to see the applied result. “Outcome unknown” requires inspection;
submitting fresh work can repeat an effect already performed.

Deploy matching bot, API and frontend builds, bot first, and reload old browser pages.
Older clients without request IDs/page headers are rejected. Use one bot and one API;
shared sessions, distributed worker ownership and complete tenant isolation remain future
work. Both services still require database write access. Public library helpers are trusted
internal operations, not a security API. Assign the bot token only to the bot and OAuth
credentials only to the API; ignoring a shared file's fields does not hide its raw contents.

## Runtime controls

Environment overrides TOML. `[actions]` is shared; configure the same storage policy in
both processes. `[action_worker]` is bot-only and ignored by the API. UI controls remain
future work. All timers must be positive; protocol bounds remain validated in code.

| Section / field | Default | Environment override |
|---|---:|---|
| `actions.pending_per_guild` | 32 | `JMODGUARD_ACTION_PENDING_PER_GUILD` |
| `actions.max_age_secs` | 300 | `JMODGUARD_ACTION_MAX_AGE_SECS` |
| `actions.history_retention_secs` | 604800 | `JMODGUARD_ACTION_HISTORY_RETENTION_SECS` |
| `actions.cleanup_batch_size` | 128 | `JMODGUARD_ACTION_CLEANUP_BATCH_SIZE` |
| `action_worker.poll_secs` | 5 | `JMODGUARD_ACTION_POLL_SECS` |
| `action_worker.cleanup_interval_secs` | 3600 | `JMODGUARD_ACTION_CLEANUP_INTERVAL_SECS` |
| `action_worker.batch_size` | 4 | `JMODGUARD_ACTION_BATCH_SIZE` |
| `action_worker.request_timeout_secs` | 15 | `JMODGUARD_ACTION_REQUEST_TIMEOUT_SECS` |

Migrations 0006 and 0008 extend the recognized `jmodguard-core-1` baseline; start the matching bot
before the API. See [schema/setup and rollback boundaries](DATABASE-BASELINE.md).
Local SQLite tests and the optional disposable remote-libSQL smoke cover storage and
transport contracts. They do not validate live Discord/Bunny operations.
