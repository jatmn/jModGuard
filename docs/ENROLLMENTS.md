# Bot membership observations

The bot now records its own observed Discord membership in each guild. Migration 0007
adds `guild_enrollments` to `jmodguard-core-1`: one row per observed guild containing a
random identity, the bot's join timestamp in nanoseconds, and active/inactive state.
No member roster, message history, or per-user activity is collected by this feature.

This is an observation foundation, not an authorization or erasure boundary. Existing
API permissions, direct Discord actions, settings and nickname reconciliation continue
to operate independently of it. The administrative action queue remains inactive and
unbound. No queue/nickname payload adopts a membership just because this row exists.

## Gateway observations

The bot subscribes to the nonprivileged `GUILDS` intent in addition to its existing
intents. Guild Create supplies a join timestamp. A matching active join keeps the same
identity without writing another row or mutation receipt. A newer join replaces it;
older observations cannot replace a newer one. A retired join cannot be reactivated by
an observation carrying that same timestamp.

Guild Delete records retirement only when it represents a departure and Serenity supplies
matching cached guild context with a usable join timestamp. Outage/unavailable events,
missing context and unsupported IDs/timestamps do not retire a membership. Failures are
logged and do not block the existing slash/nickname lifecycle paths permanently. Those
paths keep their own existing lifecycle behavior; this wave does not fence their effects.

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
observation writes no membership/config/nickname row or mutation receipt. No configuration
version is bumped. There is no per-message work, new HTTP client, library or member cache.

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

## Schema and remaining work

Start matched bot/API builds with the bot first. Migration 0007 adds an empty table;
it does not infer membership from existing guild settings or old queue jobs. Versions
1–6 in the recognized lineage can advance normally. Version-6 binaries reject version7
on startup; rollback needs matching binaries and the pre-upgrade database backup.
No previous deployment is imported and no guild data is purged.

Queue/nickname enrollment binding, fresh user permission and bot membership checks around
side effects, captured API transaction scope, stale browser context, case/evidence isolation,
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
