# Configuration

There are two layers: the static `config.toml` (tokens, bindings) and the
database (everything server-related, editable at runtime).

The bot and API load `BotProcessConfig` and `ApiProcessConfig` respectively. Existing
file/env names remain valid. The bot ignores `[web]` fields and web/OAuth env overrides;
the API continues to validate them. The shared file must have valid TOML syntax. Only
the bot loads its bot token; the API uses OAuth and queues administrative actions. See [process configuration](ARCHITECTURE.md#process-configuration).

## config.toml

| Key | Default | Description |
|---|---|---|
| `discord.token` | — | Bot token (env: `JMODGUARD_DISCORD_TOKEN`). Required only by bot; ignored by API |
| `db.path` | `jmodguard.db` | Local SQLite file (env: `JMODGUARD_DB_PATH`) |
| `db.url` | — | Remote libSQL URL; empty = local path. Environment overrides are accepted only as a complete URL/token pair. |
| `db.token` | — | Remote DB access token; required with `db.url`. Complete `JMODGUARD_DB_URL`/`JMODGUARD_DB_TOKEN` takes precedence over the complete Bunny-injected pair. |
| `db.pool_size` | `8` | reusable connections and maximum concurrent database operations in each process (minimum 2) (env: `JMODGUARD_DB_POOL_SIZE`) |
| `guild_stores.max_handles` | `8` | cached guild-store handles per process (2–32). A local core also creates `{db.path}.guild.db`. Remote cores need an explicit shared `[[guild_stores.stores]]`. Empty store `id` values rediscover stamped/catalog identity. |
| `evidence.backend` | `local` | private evidence backend: `local`, `bunny`, or `none` (env: `JMODGUARD_EVIDENCE_BACKEND`) |
| `web.bind` | `127.0.0.1:8080` | API/WebUI listen address (env: `JMODGUARD_WEB_BIND`) |
| `web.base_url` | `http://localhost:8080` | public URL used for OAuth redirect (env: `JMODGUARD_WEB_BASE_URL`) |
| `web.oauth_client_id` | — | Discord OAuth client ID (env: `JMODGUARD_OAUTH_CLIENT_ID`) |
| `web.oauth_client_secret` | — | Discord OAuth client secret (env: `JMODGUARD_OAUTH_CLIENT_SECRET`) |
| `web.cors_origins` | `[]` | Extra SPA origins only for a split-origin UI (env: `JMODGUARD_CORS_ORIGINS`). Empty = same-origin. Bunny CDN proxies `/api` onto the UI hostname, so this stays empty. |
| `web.hosting_estimate` | Cost estimate and cgroup declaration off | API-only TOML table for local Bot Resources estimates (env: `JMODGUARD_HOSTING_ESTIMATE`). See [hosting estimates](#hosting-estimates). |
| `bot.default_prefix` | `!` | seeded into the global settings row on first boot; synced to durable `meta.settings_terminal_defaults` on bot migrate-open (final fallback when global/guild omit the field) (env: `JMODGUARD_DEFAULT_PREFIX`) |
| `bot.owner_id` | `0` | owner user ID (owner commands + global WebUI config) (env: `JMODGUARD_OWNER_ID`) |
| `bot.status` | `moderating` | presence text (env: `JMODGUARD_BOT_STATUS`) |
| `bot.about_webui_url` | — | public Web UI URL that `!about` links to (env: `JMODGUARD_ABOUT_WEBUI_URL`). Only an `https` DNS name is shown, as its origin; IP addresses, `localhost`, hosts containing a Discord ID (17+ digits), and local, internal or reserved names (such as `.local`, `.internal`, `.lan`, `.home.arpa`, `.test`, `.invalid`) are left out. The bot cannot tell whether a name resolves only on a private network (split DNS), so set a name your users can open |
| `bot.about_instance` | — | optional one-line note about your instance, shown under the upstream project link in `!about` (env: `JMODGUARD_ABOUT_INSTANCE`; up to 200 characters) |
| `bot.config_poll_secs` | `5` | how often each process polls durable `config_version` (env: `JMODGUARD_CONFIG_POLL_SECS`) |
| `log.level` | `info` | tracing level (env: `JMODGUARD_LOG_LEVEL`) |
| `log.directory` | `logs` | rolling log directory (env: `JMODGUARD_LOG_DIRECTORY`; empty = console only). Bot writes `jmodguard-bot.log`; API writes `jmodguard-api.log`. Container images default this env var to empty for platform logging |
| `log.group_automod_cleanup_deletes` | `true` | Bot only. Group eligible automod cleanup deletes across channels into one summary (env: `JMODGUARD_LOG_GROUP_AUTOMOD_CLEANUP_DELETES`) |
| `log.automod_cleanup_settle_secs` | `10` | Bot only. Require 5–10 seconds of quiet before sending a grouped cleanup summary (env: `JMODGUARD_LOG_AUTOMOD_CLEANUP_SETTLE_SECS`) |

Use `info` for production. `JMODGUARD_LOG_LEVEL` overrides `log.level` and
is the only environment override for tracing; `RUST_LOG` is ignored. The
configured level applies to application targets, with no automatic debug override.
Sensitive dependencies are capped at `warn`, with Serenity gateway logs capped at
`error`; more restrictive configured levels still apply. Only a single level is
accepted, not target directives. See [diagnostics setup](SETUP.md#11-troubleshooting).
Per-message and per-guild diagnostics require `debug`.
Enable it only briefly: diagnostic metadata can reveal per-user activity even
though the per-message line omits author IDs and message text. Restrict log access.

### Hosting estimates

The API loads the `[web.hosting_estimate]` table on startup. A nonblank
`JMODGUARD_HOSTING_ESTIMATE` replaces the entire table with a JSON object
using the same fields. Omitted fields use the defaults below.

| Key in `web.hosting_estimate` | Type | Default | Description |
|---|---|---|---|
| `enabled` | Boolean | `false` | Show hourly and 30-day USD estimates on the owner-only Bot Resources page. Local memory blocks are shown when the readings qualify, regardless of this setting. |
| `assume_disjoint_cgroups` | Boolean | `false` | Declare that the sampled Bot and API cgroups are disjoint. Enable only after independently verifying separation. Applies when both scopes are identified but their visible ancestry is inconclusive. Shared or overlapping scopes and missing identities still prevent combined estimates. |
| `memory_usd_per_block_hour` | Number | `0.0003125` | USD per local 64 MiB memory block per hour. |
| `cpu_usd_per_core_hour` | Number | `0.02` | USD per CPU core-hour used. |

Rates must be finite and nonnegative. Combined estimates require fresh,
usable readings for the relevant metric. The local memory model rounds up
to 64 MiB blocks with a one-block minimum. The 30-day estimate assumes
current usage continues for 720 hours. Both cost estimates exclude host
overhead and egress. They use local readings and make no calls to billing providers.
See [Bot Resources](WEBUI.md#tabs).

### Bunny provider cost estimates

Set these optional variables in the **API process** environment. Use stats-only
keys restricted to the listed resources. Example values are placeholders.

| Variable | Placeholder | Purpose |
|---|---|---|
| `JMODGUARD_BUNNY_MC_STATS_KEY` | `<mc-stats-key>` | Key with `MagicContainers.Statistics` |
| `JMODGUARD_BUNNY_MC_APP_ID` | `<mc-app-id>` | Magic Containers app ID, with its original case |
| `JMODGUARD_BUNNY_DATA_STATS_KEY` | `<data-stats-key>` | Key with `Database.Statistics`, `StorageZone.Statistics`, or both |
| `JMODGUARD_BUNNY_DB_CORE_ID` | `<CORE-DB-ID>` | Core database ID, as issued by Bunny |
| `JMODGUARD_BUNNY_DB_GUILD_ID` | `<GUILD-DB-ID>` | Guild database ID, as issued by Bunny |
| `JMODGUARD_BUNNY_STORAGE_ZONE_ID` | `<storage-zone-id>` | Storage zone ID |

Set a matching key and ID for each service you want to see. Either database
ID can be used by itself. Copy uppercase database IDs as issued. The API
does not trim or change ID case. The Bunny panel appears only when the API can
start a collector for at least one valid key-and-ID pair. Scoped keys cannot
read CDN statistics.

Costs use UTC calendar months and USD. Bunny's app-summary `monthlyCost` is
the authoritative Magic Containers projection. The month-to-date subtotal
and history estimate CPU, RAM, traffic and any available allocated volume cost.
Chart keys mark bucket starts in UTC, and daily buckets follow the UTC calendar.
The client filters by the requested range because Bunny can return buckets
beyond `toDate`, including the current partial hour or day. Only closed
buckets contribute to chart estimates and displayed averages.

| Chart field | Observed or published scope | Treatment |
| --- | --- | --- |
| `cpuUsageSplitChart` | Mean percentage of the standard 8-CPU pod allocation. The unit is inferred from a match with billed costs. The public chart docs omit the unit. | Convert to estimated cores as `percent / 100 × 8` and price closed hours at $0.02 per core-hour. The latest closed hour also shows the estimated average cores per pod. |
| `ramUsageSplitChart` | Sample-weighted mean bytes per pod, including daily buckets | Rounded up to whole 64 MiB blocks per pod and priced per hour. |
| `trafficSplitChart` | Bytes summed per bucket, with daily values equal to the sum of hourly values | Priced as egress at the rate for the reporting pod's region |

Daily CPU and RAM values are sample-weighted means of the underlying readings.
Current CPU cost uses closed hourly observations. Historical backfill reads
daily RAM and traffic, and history can retain CPU costs saved during the
current period. Daily RAM uses the mean for block rounding and applies the
resulting blocks over 24 hours.

Persistent volume cost uses the app summary's `totalVolumeSizeInGb` at
$0.10/GB-month. The current allocated size is prorated over the elapsed UTC
month. Volume chart units are unverified and those charts are excluded from
estimates. A missing allocation leaves the volume cost unavailable.
Historical backfill preserves any
month-to-date volume estimate saved in that month and has no allocation
history to fill in the remaining cost. A known volume estimate contributes
to the partial subtotal even before any chart data is available.

Missing values can leave gaps in these estimates. Bunny's billed RAM
includes variable system overhead, and traffic billing covers egress
outside the CDN. The chart schema leaves billing-specific traffic filters
unspecified.
See [Magic Containers pricing](https://bunny.net/docs/magic-containers/pricing)
and [monitoring](https://bunny.net/docs/magic-containers/monitoring).

Magic Containers traffic uses [Bunny’s published regional rates](https://bunny.net/docs/magic-containers/pricing#network-traffic-usage).
Positive traffic in a region with an unknown rate leaves the chart-derived
egress and total unavailable. Bunny’s summary projection remains available.
Storage uses the single-region Standard rate and Database uses the
public-preview rate. Estimates exclude the $1 monthly account minimum,
taxes and credits. Storage projections extrapolate based on the fraction of the month
elapsed. See [Storage pricing](https://bunny.net/docs/storage/pricing)
and [Database pricing](https://bunny.net/database/#pricing).

Successful figures refresh about every six hours. API failures keep the last
successful same-month figures with their timestamp and trigger bounded retries.
History groups daily estimates into monthly snapshots for each configured
resource. It preserves collected days as missing days fill in. Backfill uses up
to 365 days for Magic Containers and Storage and 180 days for Database.
The authenticated bot writer link must be configured to save snapshots. The
backup for the core database includes this history.

### Public legal details

Operators must fill in `[legal]` in the API's configuration before publishing
the policy URLs. The pages and the signed-out `GET /api/legal` endpoint publish
these values, so use text intended for the public. List each applicable
subprocessor with its role, such as hosting, CDN, or database. Blank or
omitted values appear as unconfigured on the pages. Restart the API to apply
changes.

| `[legal]` field | API environment override |
|---|---|
| `operator_name` | `JMODGUARD_LEGAL_OPERATOR_NAME` |
| `privacy_contact` | `JMODGUARD_LEGAL_PRIVACY_CONTACT` |
| `deletion_instructions` | `JMODGUARD_LEGAL_DELETION_INSTRUCTIONS` |
| `subprocessors` | `JMODGUARD_LEGAL_SUBPROCESSORS` (JSON array of strings) |
| `jurisdiction` | `JMODGUARD_LEGAL_JURISDICTION` |
| `effective_date` | `JMODGUARD_LEGAL_EFFECTIVE_DATE` |

Administrative policy (`[actions]`) is shared by bot/API; scheduling (`[action_worker]`)
is bot-only. See [all action defaults and environment overrides](BOT-ACTIONS.md#runtime-controls).
Bot-only `[member_sync]` config sets `page_size = 100` (allowed 1–500),
`min_request_interval_secs = 15` (minimum 15), `request_timeout_secs = 30`,
`retry_base_secs = 30`, and `retry_max_secs = 3600`. These exact environment
variables override the corresponding TOML fields:

| `[member_sync]` field | Environment variable |
|---|---|
| `page_size` | `JMODGUARD_MEMBER_SYNC_PAGE_SIZE` |
| `min_request_interval_secs` | `JMODGUARD_MEMBER_SYNC_INTERVAL_SECS` |
| `request_timeout_secs` | `JMODGUARD_MEMBER_SYNC_TIMEOUT_SECS` |
| `retry_base_secs` | `JMODGUARD_MEMBER_SYNC_RETRY_BASE_SECS` |
| `retry_max_secs` | `JMODGUARD_MEMBER_SYNC_RETRY_MAX_SECS` |

The single paced worker shares its request budget across guilds;
see [MEMBERS.md](MEMBERS.md).

Environment-only (not in `config.toml`):

| Variable | Default | Description |
|---|---|---|
| `JMODGUARD_REQUIRE_REMOTE_DB` | unset | If `1`/`true`, startup fails unless both a remote URL and token are configured. Container images set this so a missed Bunny secret cannot open a private SQLite file. |
| `JMODGUARD_HEALTH_BIND` | unset | Bot-only HTTP listener. `GET`/`HEAD /health` is liveness and returns `ok` with no database. `GET`/`HEAD /ready` is readiness: `503` `not ready` until startup has finished, then `200` `ready`. Readiness is an in-memory flag. Bound before database open. Container bot image sets `0.0.0.0:8080`; bind failure exits the process. Empty = disabled. The API serves the same paths on `web.bind`. |

## Database (runtime config)

This release requires the [fresh database baseline](DATABASE-BASELINE.md). Configuration
is managed via the WebUI or bot commands; the schema lives in
`migrations/` (`0023_baseline.sql` and later steps) and is applied by
`jmodguard-bot` on startup. The API opens with `migrate=false` and does not
migrate. It keeps retrying with a capped delay. `/health` stays 200. `/ready`
and app routes stay 503 until it is ready. It exits on a newer unsupported
schema, rejected remote credentials and other non-retryable open errors, and
bad config such as an empty OAuth client id or secret.

| Table | Holds |
|---|---|
| `meta` | schema family/version support and durable counters plus non-reusable revision tokens (`config_version`/`config_revision` for cache invalidation; `slash_registry_version`/`slash_registry_revision` for Discord slash re-sync) |
| `guild_enrollments` | the bot's own observed guild membership, not a member roster |
| `bot_actions` | typed administrative requests, membership-bound claims and outcome receipts; bounded resolved-history retention |
| `nickname_reconcile` | request/membership-bound nickname jobs with captured nickname, pending/dormant/terminal state, persisted retries and last operator-visible error |
| `mutation_receipts` | short-lived commit-outcome receipts that establish remote mutation results after an ambiguous COMMIT response |
| `guild_settings` | prefix and bot nickname as **partial overrides** (row per server; `guild_id=0` = global). Missing fields inherit from the next layer. |
| `rules` | the 7 automod rules (server + global rows) |
| `commands` | every command — name, aliases, behavior, response, permissions (server + global) |
| `templates` | log event templates (server + global) |
| `log_channels` | event type → channel mapping (per server only) |
| `log_filters` | ignored user/bot IDs and source channel IDs per event and server |
| `legacy_global_log_channels` | transitional empty global-channel scaffolding; new mappings are per server |
| `badwords` | regex patterns (server + global) |
| `events` | selected moderation records and classified moderation-plan summaries; routine channel reports do not append here |
| `warns` / `bans` | warnings and ban history |
| `guild_store_catalog` / `guild_store_placements` | nonsecret shared/dedicated store identities and initial placements |
| `guild_erasure_jobs` | queued live guild-store erasure; not a departed-guild membership ledger |

Environment variables override `config.toml` values (see table).

Remote database calls and waits for a pooled connection have a fixed 75-second
per-wire-attempt deadline. Interactive API calls use a shorter deadline. If
that deadline fires while a write transaction may still be open, including
while BEGIN itself is still in flight, one best-effort rollback runs on the
database runtime under a fresh few-hundred-millisecond deadline. The remote
client sends the stream close only from a live runtime, so the writer lock is
not left until the provider steals it. A provider-closed stream is not
rolled back; that request would wait out the client deadline and cannot change
the outcome. If the caller stops waiting while the write is still in flight,
that call ends and releases the pooled connection and any writer lock instead
of holding both until the operation deadline. Other rollback cleanup and an authoritative receipt check after a
failed COMMIT can each use an additional deadline before the logical operation
returns. A connection whose operation timed out is
discarded rather than reused with potentially stale remote stream state. Each
normal post-bootstrap application mutation carries a receipt in its transaction;
idempotent migrations and bootstrap fencing are reverified on startup instead. If a COMMIT
response is lost, jModGuard checks that receipt in a fresh primary-routed
transaction and returns the established result rather than inviting a duplicate
retry. If that authoritative check also fails, the operation remains explicitly
indeterminate and the process conservatively advances its local configuration
floor so it cannot immediately serve an older snapshot.

## Remote providers

The remote transport uses the standard libSQL protocol; the portable
configuration contract is `db.url` / `db.token` or `JMODGUARD_DB_URL` /
`JMODGUARD_DB_TOKEN`. Environment credentials are selected as an atomic pair:
if either portable variable is non-empty, both must be non-empty; otherwise the
same rule applies to Bunny's injected pair. Blank or whitespace-only values are
treated as unset. Partial or mixed-provider overrides fail startup instead of
pairing a URL with an unrelated token. It is not hard-coded to a Bunny hostname.

Bunny Database is the first-class launch provider: its injected environment
names and generic libSQL protocol path have automated tests, and its container
topology is documented. CI's disposable server does not reproduce Bunny's
production TLS/authentication service. Other libSQL-compatible services may
work through the portable settings, but they are not claimed as supported
until their authentication, consistency behavior, deployment, and recovery
workflow have provider-specific tests and documentation.

## Behavior on first boot

If the database is new, global defaults are seeded: 8 rules, ~20 commands,
and default templates. Servers inherit these until you override them.

## Backups

For a local deployment, the whole configuration lives in `jmodguard.db`,
shared by **both** `jmodguard-bot` and `jmodguard-api`. Either process can hold
WAL connections open.

**Cold copy (preferred for restores):** stop **both** the bot and the API,
then copy `jmodguard.db` plus `jmodguard.db-wal` / `jmodguard.db-shm` if
present, then start both again.

**Hot copy:** use SQLite's online backup API or
`sqlite3 jmodguard.db "VACUUM INTO 'jmodguard-backup.db'"` while the
processes keep running — do **not** file-copy the `.db` while either process
is still writing.

### Bunny Database

Bunny's internal snapshots are not an operator-controlled backup. The current
`bsql .dump` output also omits schema objects required by jModGuard, so it is
not a complete recovery path. Operators must verify their own complete backup and
restore procedure before production; the application provides no archive processor.

### Discord log delivery

`log.discord_concurrency` (default 8, environment `JMODGUARD_LOG_DISCORD_CONCURRENCY`)
and `log.discord_timeout_secs` (default 10, environment `JMODGUARD_LOG_DISCORD_TIMEOUT_SECS`)
are positive bot bootstrap controls. Routine and startup report admission never
waits; saturation drops those reports. A graceful-shutdown system notice may
wait for a delivery slot, but all shutdown notices share a three-second process
budget before shard closure. The per-report deadline includes admission (for
shutdown), config lookup and delivery. Failed or unfinished reports have no
persistent queue or replay. A cumulative counter produces sparse process warnings
without raw report content.
Operator UI controls and per-guild fairness remain pending. These defaults are tunable,
not measured throughput budgets. See [logging/data status](LOGGING-DATA.md).

`bot.retention_days` and its environment override `JMODGUARD_RETENTION_DAYS` are
deprecated. The bot warns at startup when either is explicitly set. Existing
values still load as the legacy database settings fallback and have no effect on
message or member history retention. The internal `retention_days` field is absent
from API responses, and settings requests containing it are rejected. The archive
tables and helpers are removed from this fresh baseline. Unsupported existing
databases are refused, not migrated or cleaned. The full tenant/case model remains
future work.

## Bot membership repair

Bot-only `[enrollment]` config controls positive membership discovery: enable/disable,
page size, polling, sweep pause, retry delay and per-request timeout. The API ignores
these TOML fields and their `JMODGUARD_ENROLLMENT_*` overrides. See
[defaults, bounds and behavior](ENROLLMENTS.md#bounded-positive-repair).

## Captured-scope data operations

New data-layer operations check the request's captured membership within configuration
transactions, preserving existing inheritance, nickname and version semantics. Current API
and bot callers retain their existing paths; this is groundwork for complete admission and
browser scope handling. See [configuration scopes](CONFIGURATION-SCOPES.md).

## Environment-only tenant stores

Set `JMODGUARD_GUILD_STORE_URL` and `JMODGUARD_GUILD_STORE_TOKEN` together to configure
the default shared guild database in both services. These override the first shared
TOML entry's connection pair, preserving its ID and every other store entry. If no
shared entry exists, one is added. Optional `JMODGUARD_GUILD_STORE_ID` requires the pair
and must match the durable identity; leaving it unset preserves discovery. This never
permits using the core database as a guild store. Dedicated/additional stores retain
the TOML configuration described above.

Remote cores also require an explicit evidence choice: `JMODGUARD_EVIDENCE_BACKEND=none`
for a fresh installation without objects, or the existing owning backend and its
configuration. Local evidence with a remote core needs `JMODGUARD_EVIDENCE_ROOT` on
persistent private storage. See [the complete container setup](DEPLOY-BUNNY.md#1-create-the-core-and-guild-databases).
