# Bot membership observations

The bot now records its own observed Discord membership in each guild. Migration 0007
adds `guild_enrollments` to `jmodguard-core-1`: one row per observed guild containing a
random identity, the bot's join timestamp in nanoseconds, and active/inactive state.
No member roster, message history, or per-user activity is collected by this feature.

Observations bind administrative actions and the nickname outbox.
API guild access now checks them after fresh Discord user permissions; configuration
reads/writes retain captured membership. They are not a complete authorization or erasure
boundary. Administrative and nickname dispatch each add fresh Discord verification.

## Gateway observations

The bot subscribes to the nonprivileged `GUILDS` intent in addition to its existing
intents. Guild Create supplies a join timestamp. A matching active join keeps the same
identity. Healthy repeats write no row or mutation receipt; a matching observation may
resume a dormant nickname request bound to that exact membership. A newer join replaces it;
older observations cannot replace a newer one. A retired join cannot be reactivated by
an observation carrying that same timestamp.

Guild Delete records retirement only when it represents a departure and Serenity supplies
matching cached guild context with a usable join timestamp. Outage/unavailable events,
missing context and unsupported IDs/timestamps do not retire a membership. Failures are
logged; slash synchronization remains independent. Membership transitions invalidate
nickname work atomically. There is no separate guild-only nickname departure/reactivation
call that could affect a newer membership after stale or missing event context.

The stored observation is not proof of current membership forever. Missed or contextless
removals remain unresolved; cached delete context and reordered gateway/REST observations
need live validation before relying on them for access revocation. This table must not
be used alone to grant access, auto-purge data or authorize side effects.

## Bounded positive repair

A bot-only background task reuses the existing Serenity HTTP client and its bot identity
from the Gateway cache. It waits for a real bot identity instead of using the SDK's
pre-READY placeholder. Each step lists one bounded guild page and queries only the bot's
own member record for each returned guild, sequentially. It records positive join
observations using the same monotonic primary transaction as gateway observations.

The task retains only a pagination cursor and next-run time between steps. A matching
observation with no same-membership dormant nickname request writes no row or mutation
receipt. Resuming a dormant request changes its generation/state with a recovery receipt,
without changing membership identity or configuration version. There is no per-message work, new HTTP client, library or member cache.

| Bot `[enrollment]` setting | Default | Environment override |
|---|---:|---|
| `enabled` | `true` | `JMODGUARD_ENROLLMENT_REPAIR_ENABLED` (`true`/`false`) |
| `poll_secs` | 15 | `JMODGUARD_ENROLLMENT_POLL_SECS` |
| `batch_size` | 16 | `JMODGUARD_ENROLLMENT_BATCH_SIZE` |
| `sweep_pause_secs` | 3600 | `JMODGUARD_ENROLLMENT_SWEEP_PAUSE_SECS` |
| `retry_secs` | 60 | `JMODGUARD_ENROLLMENT_RETRY_SECS` |
| `request_timeout_secs` | 10 | `JMODGUARD_ENROLLMENT_REQUEST_TIMEOUT_SECS` |

Environment values override TOML. The API ignores this section and these environment
variables. Intervals must be positive, pages 1–200 guilds, and each REST deadline 1–300
seconds. Disabling repair stops the periodic task; Gateway observations still operate.
These are bootstrap controls; admin UI controls remain future work.

Polls during the scheduled pause do no SQL/HTTP work. A complete sweep waits one hour
by default; page steps wait at least the poll interval. Listing/DB failures keep the
cursor and back off; failed individual member reads are revisited next sweep. An empty
page, an omitted guild, or failed member request never causes retirement. Every REST
request has its own deadline; a whole page can take multiple such deadlines plus DB time.
Only one step runs at a time, missed ticks are skipped, and shutdown cancels the wait/step.

At defaults, each page makes at most 17 sequential REST requests (one list, 16 own-member
reads). Actual rate depends on guild count, timeouts, pagination and configured delays.
Resource/billing savings are not measured. Existing Gateway cache policy is unchanged;
this does not establish the final minimal bot memory footprint.

## Queue binding

Migration 0008 adds explicit enrollment ID/join timestamp to new administrative actions.
Retirement/replacement invalidates unstarted claims and makes running outcomes uncertain
in the same transaction. Queue admission, reads, cancellation and local completion are
membership-aware. Captured-scope helpers reject a membership change between authorization
and the queue transaction. Direct log configuration changes preserve unresolved history
from older memberships. See [the queue storage contract](BOT-ACTIONS.md).

The API admits captured actions and the bot dispatches them with fresh membership and
actor permission checks. This layer cannot recall a Discord request or establish full tenant isolation. Current membership observations still need live validation/reconciliation and
must not alone authorize an effect or erase data.

## Nickname reconciliation

Migration 0009 extends the existing one-row-per-guild nickname outbox. A settings save or
explicit Re-sync captures a random request ID, effective nickname and active enrollment in
the same transaction. No membership means a dormant unbound request. Pending reads join the
active membership and return the captured payload; they do not recalculate it at dispatch.

The bot checks the exact primary request, then fetches only its own Discord member record
with the existing REST deadline. A mismatched join leaves the job dormant; an unavailable
or invalid response schedules bounded retries without retiring the guild. After a matching
response it checks the primary request again before editing. Retry/failure/acknowledgment
compare request ID, enrollment, join, nickname, generation and attempt. Deleting a completed
row and reusing generation1 therefore cannot let an old completion delete a new request.

Retirement/replacement makes existing work dormant atomically. Neither a new join nor
migration adopts old/unbound work or creates an initial nickname edit. Confirm the bot is
present, then save its nickname or use **Re-sync Discord nickname** to submit fresh work.
A reconnect/positive repair may resume only a dormant request in that same membership;
terminal errors still need operator correction and a fresh request. Settings are retained.

No new task, HTTP client, cache or library is added. The existing worker loads up to25 jobs
and runs up to4 concurrently. Each dispatch adds one own-member read; stale jobs skip HTTP.
Existing retry/deadline limits remain bootstrap implementation limits, not measured capacity.
The checks cannot recall a Discord request after it is sent or guarantee atomicity between
SQL and Discord. Run one bot worker; exclusive replica ownership remains unimplemented.
This does not certify tenant isolation or automatic recovery.

## Captured configuration storage

The database now provides captured-scope settings, copy, rule, command, template and badword
operations. They verify the original membership inside the same primary transaction;
copies check both ends and global writes from a guild context retain that context's check.
Empty settings patches also verify before reading, without writing receipts or versions.
The API now uses these helpers after fresh permission and active membership admission.
Configuration snapshots return a membership token required on edits; copies retain both
page and selected-source membership. See [the HTTP contract](CONFIGURATION-SCOPES.md#http-and-browser-contract).
Bot configuration callers retain their existing helpers. No schema change is introduced.

## Schema and remaining work

Start matched bot/API builds with the bot first. Migration 0007 adds an empty table;
it does not infer membership from existing guild settings or old queue jobs. The current
recognized lineage extends through migration0010. Version9 binaries reject version10;
rollback needs matching binaries and the pre-upgrade database backup.
No previous deployment is imported and no guild data is purged.

[Moderation event, ban and statistics reads](MODERATION-READS.md) now retain captured
membership through their primary snapshot and require the loaded page token.
Individual configuration GET reads now retain that same page membership with guild/global
layers in one primary snapshot; explicit global context stays owner-only. Case/evidence isolation,
shared/dedicated routing, complete guild erasure and missed-removal reconciliation remain
separate work. Inactive membership rows are transitional lifecycle metadata awaiting that
purge design. Local tests do not certify tenant isolation or Discord/Bunny behavior.

## Sources and validation

Discord documents the join timestamp on [Guild Create](https://docs.discord.com/developers/events/gateway-events#guild-create)
and distinguishes outages from removals in [Guild Delete](https://docs.discord.com/developers/events/gateway-events#guild-delete).
Repair uses [paginated current-user guilds](https://docs.discord.com/developers/resources/user#get-current-user-guilds)
and [one guild member](https://docs.discord.com/developers/resources/guild#get-guild-member),
not the member-list endpoint. The implementation uses the pinned Serenity 0.12.5 API.

Local tests cover concurrent/reordered observations, reconnect/reopen, rollback, no-write
matching observations, scoped reads, gateway missing/outage context, startup identity,
pagination bounds, retry/pause, member deadlines and non-destructive omissions. The optional
disposable libSQL test covers lost commit responses and bot/API reads. Live Gateway/REST
join-timestamp consistency, provider durability and resource measurements remain deferred.
