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
source within the same write transaction; migrations and conductor moves use
separate conservative maintenance allowances and can refuse a large operation.

All supported application writes must use the checked paths. Raw administrative
SQL and unrelated database clients bypass this policy and are outside its
guarantee. Do not use them to get around a refusal. Do not run an emergency full
`VACUUM` against a nearly full production database. Plan supported maintenance
with enough space for its temporary output; keep the provider's normal quota.

## Owner inspection

Open **Administration → Database Usage** in the WebUI as the configured bot
owner. Ordinary administrators and View as… preview sessions cannot inspect
these routes. The page includes the core database and every cataloged guild
store, including unavailable or unconfigured stores. It does not return database
URLs, tokens, local paths, message contents or SQL error details.

The overview refreshes no more often than once per minute. Detailed attribution
is cached for five minutes. Exact category attribution uses `dbstat` when the
engine supplies it, separating snapshot tables, snapshot indexes, other tables,
other indexes, reusable pages and remaining allocation. Otherwise the page
labels the available snapshot payload estimate and unavailable physical
attribution; estimates are not added to exact totals. Measurements carry their
sample time. Failed probes preserve a clearly stale prior measurement instead
of turning unknown space into zero usage.

Capture and cleanup health come from bounded latest-value records written by
the bot and read by the API. An old heartbeat is stale, not proof that capture
is healthy. Check these states as well as the size percentage during periodic
inspection. A provider console's billed storage, replica, backup or journal
metrics may have a different scope from the engine's allocated main database.

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

See [message logging](LOGGING-DATA.md), [backup and restore](BACKUP-RESTORE.md)
and [guild moves](GUILD-MOVES.md) for the retained payload lifecycle.
