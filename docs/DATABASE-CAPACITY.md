# Database usage and application budgets

jModGuard manages its own write admission. It does not lower Bunny's quota or
set SQLite `max_page_count`. The decimal boundary is **1,000,000,000 bytes for
each physical database**, including its non-message tables and indexes.

| Application threshold | Bytes | Behavior |
| --- | ---: | --- |
| Rolling snapshots | 400,000,000 | Expire or evict oldest snapshots; other data takes priority. |
| Pressure | 700,000,000 | Accelerate bounded snapshot cleanup. |
| Capture pause | 800,000,000 | Refuse new or growing snapshots. |
| Ordinary growth stop | 900,000,000 | Refuse application writes whose conservative allowance does not fit. |
| Provider boundary | 1,000,000,000 | Never use provider rejection as normal admission. |

Allocated size is `page_count * page_size` on an authoritative primary
transaction. Reusable pages remain part of allocation; deleting rows does not
necessarily shrink a database. After a capacity pause, capture requires a fresh
allocated-size reading below 700 MB before resuming. Closing the browser does
not stop cleanup. Moderation records, configuration and selected evidence are
never automatically deleted to create room for message logs.

Application writers acquire an immediate transaction, verify their scope and
reserve a conservative growth allowance before mutation. That allowance covers
record bytes, indexes and B-tree working space. Commit receipts have their own
allowance. A final measurement before commit rejects an exceeded allowance.
Unknown measurements, overflow and unclassified write paths fail closed.
If a measurement detects growth beyond the declared allowance, that process
pauses further growth for the affected database runtime, including replacement
pooled connections. Reads and bounded reclamation remain available. Diagnose
and correct the write bound before restarting the process; restarting clears
this latch. Other processes still apply their own primary admission checks.
Cached owner-page measurements cannot authorize a write. Copies measure their
source within the same write transaction; migrations use
separate conservative maintenance allowances and can refuse a large operation.

If an already initialized bot database reopens above 900 MB with the current
schema and persisted terminal defaults intact, startup defers seed/heal and
exact-default template upgrades. New snapshot capture stays paused while owner
inspection and verified bounded cleanup remain available. After reclamation
brings allocation below the growth stop, restart the bot to complete deferred
bootstrap work before treating capture as ready. Incomplete schemas and missing
defaults still refuse startup. The API's rolled-back write permission probe
continues to reject a read-only token without growing the database.

All supported application writes must use the checked paths. Raw administrative
SQL and unrelated database clients bypass this policy and are outside its
guarantee. Do not use them to get around a refusal. Do not run an emergency full
`VACUUM` against a nearly full production database. Plan supported maintenance
with enough space for its temporary output; keep the provider's normal quota.

## Active-bot lease admission

Initial claims and expired-lease takeovers intentionally retain ordinary growth
admission. They can insert lease/generation metadata and, on remote libSQL,
commit-recovery receipts. Operational authority does not justify bypassing the
storage budget or the pause raised by an underestimated write bound. A refused
claim returns an error without changing the persisted lease/generation or arming
the claimant; the bot cannot become active until a claim succeeds. Expiry alone
does not give a replacement permission to act. Startup fails on this admission
error; repeated restarts cannot solve exhausted capacity.

An already-armed owner's dedicated renewal CAS updates its existing lease without
creating a receipt or reserving ordinary growth. It can therefore keep authority
while growth is paused or receipt headroom is exhausted, but still checks owner,
unexpired lease, writer generation and the restore fence. Ordinary writes remain
protected. Renewal without an armed matching generation falls back to the claim
path and receives the same admission checks. The ordinary receipt-bearing lease
release can also fail under pressure; another owner must still wait for expiry
and pass its own claim admission.

For an underestimated-bound pause, diagnose and correct the offending bound,
then restart the affected process to clear its runtime latch. For exhausted
capacity, restore enough measured allocated-size headroom for the complete claim,
including remote receipt allowances, using supported maintenance with sufficient
working space. Restarting alone does not reduce allocation, and deleting rows
may only create reusable pages. A still-active owner can perform permitted
bounded reclamation. If no bot can acquire authority, bot-owned WebUI cleanup is
not a recovery route: arrange supported operator maintenance before retrying
startup. Do not bypass the admission checks with raw SQL or raise the provider
quota as a substitute for application admission.

## Owner inspection

Open **Administration → Database Usage** in the WebUI as the configured bot
owner. Ordinary administrators and View as… preview sessions cannot inspect
these routes. The page includes the core database and every cataloged guild
store, including unavailable or unconfigured stores. It does not return database
URLs, tokens, local paths, message contents or SQL error details.

The overview refreshes no more often than once per minute. Detailed attribution
is cached for five minutes. The API admits at most two store probes concurrently,
waits up to eight seconds for each response and limits a fleet refresh to 25
seconds. A timed-out blocking database call may finish later; it keeps its probe
slot until completion, so later probes can report unavailable while it drains.
A detail request opens only the selected cataloged store. Exact category
attribution uses `dbstat` when the engine supplies it, separating snapshots,
moderation/evidence metadata, configuration, operational tables, indexes,
reusable pages and system/unattributed remainder. Categories reconcile to one
physical allocation sample. If `dbstat` is unavailable, physical categories
remain unavailable; the encoded snapshot payload estimate is labeled separately.
Snapshot counts and logical bytes come from bounded singleton accounting, and
the oldest retained timestamp uses the expiry index. The page does not scan
message bodies for each refresh. Failed probes preserve a clearly stale prior
measurement instead of turning unknown space into zero usage.

Capture and cleanup health come from bounded latest-value records written by
the bot and read by the API. The page shows the capture reason, per-store loss counter, epoch, heartbeat
age, 24-hour maximum retention, oldest retained time, cleanup status and counters
with their reset time. An old or absent heartbeat is unknown, not proof of
healthy capture. The API process reports its own growth guard and deferred
startup state; another process's live guard is explicitly unknown. The latest
primary sample is display evidence, not write-admission authority. Provider
reported bytes and external Bunny Storage evidence file bytes remain separate
unavailable fields until a verified measurement source exists. A provider
console's billed storage, replica, backup or journal metrics may have a
different scope from the engine's allocated main database.

## Workload and rollout evidence

The ignored storage measurement test runs 60 seconds of input at each of 10 and
100 events/sec, then large records, eviction/reuse and simulated 24-hour expiry:

```bash
cargo test --locked -p jmodguard --lib \
  db::guild_data::tests::snapshots::measure_sustained_snapshot_workload_and_cleanup_reuse \
  -- --ignored --exact --nocapture
```

It prints allocation, retained/missing/refused counts, operation latency and
test-client resource samples without message bodies. For a **fresh disposable
localhost libSQL server**, set `JMODGUARD_SNAPSHOT_MEASURE_URL` and
`JMODGUARD_SNAPSHOT_MEASURE_TOKEN`; this harness refuses non-loopback endpoints.
Its serial calls may fall behind the offered input rate. It does not measure
the bot's Gateway queue, Discord delivery or production Bunny capacity.

Member count and online count do not determine message volume. For a server
with 10,000 members and 2,000 online, measure actual creates, edits, deletes,
admission refusals, cleanup throughput and database latency. At an illustrative
2–8 KiB retained footprint per message, 100,000 messages in 24 hours would need
roughly 205–819 MB before competing data. The 400 MB rolling budget therefore
shortens retention under load; 24 hours is a maximum age, not a promised window.

Deploy matching bot/API binaries with guild schema 9. Keep capture disabled
during migration and controlled canary setup. Validate page measurements,
concurrent writers, cleanup and refusal using a disposable libSQL/Bunny database
and a small application test budget with the server quota unchanged. Never fill
production to test the boundary. Local tests and scenario arithmetic do not
prove Bunny-specific quota accounting or production throughput. Live provider
and controlled Discord checks remain deployment validation, requiring an
authorized isolated environment.

See [message logging](LOGGING-DATA.md) for the retained payload lifecycle.
