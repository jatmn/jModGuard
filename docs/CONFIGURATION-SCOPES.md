# Captured configuration scopes

The data layer now provides configuration operations that retain the membership captured
for a request and verify it in the same primary transaction as the operation. If the bot
leaves or rejoins before that transaction, the old request fails with `EnrollmentChanged`.
The request is never silently rebound to the replacement membership.

The dashboard now uses these operations for settings, rules, commands, templates, badwords
and configuration copies. Guild requests first require fresh Discord user permissions and
an active bot membership from the primary database. Configuration reads capture membership
and their data in the same primary snapshot; writes verify it again in their transaction.
Bot configuration callers retain their guild-ID operations. No schema or dependency is added.

## Scope and caller responsibilities

`ConfigurationScope` is either `Global` or `Guild(GuildEnrollment)`. Its guild identity
includes guild ID, enrollment ID and bot join timestamp. Verification reuses the same
primary membership check as captured action storage. A database verification failure returns
`EnrollmentUnverified` with a generic message; server logs retain diagnostic details.

`ConfigurationWriteScope` retains both the request's scope and the write target. It permits
the same scope or a global target. A write targeting global configuration from a guild
context still verifies that guild's captured membership. It rejects changing the target
to another guild, including replacing a captured identity with a newer join of the same
guild. Its fields are private so callers cannot replace them after construction.

These values are trusted internal data, not credentials. Constructing `Global` or requesting
a global target does not prove owner status. Callers must authenticate the user, enforce
fresh guild/owner permissions, and capture and retain the intended scope. They must not
resolve a fresh membership immediately before writing an old browser request. The API constructs these values only after authorization; full tenant-data isolation remains separate work.

## Operations

| Captured operation | Transaction contract |
|---|---|
| `load_enrolled_web_config_snapshot` | Verify membership alongside configuration data in a primary read, preserving the API read floor. Membership is checked, then every configuration collection is read in one SQL statement so Bunny Database interactive transactions stay inside the provider timeout. |
| `patch_enrolled_settings` | Verify the captured scope before reading or changing settings, including empty patches and explicit nickname re-sync. |
| `copy_enrolled_guild_config` | Verify both captured source and destination before reading/copying any section. |
| `upsert_enrolled_rule`, `delete_enrolled_rule` | Verify request and target before the rule mutation and configuration version update. |
| `upsert_enrolled_command`, `delete_enrolled_command` | Verify request and target before the command mutation and configuration/slash version updates. |
| `upsert_enrolled_template`, `delete_enrolled_template` | Verify request and target before changing the template. |
| `add_enrolled_badword`, `delete_enrolled_badword` | Preserve regex validation and exact guild-ID deletion scope alongside the captured request check. |

The existing operations and captured variants share their mutation implementation. Settings
normalization, inheritance, nickname outbox capture, settings-heal checksums and config/slash
version semantics remain together in the transaction. Copies retain validate-before-replace
behavior for badwords, section restrictions and skipped guild-specific log channels. Any
later failure rolls back earlier sections, nickname work and version changes together.

An empty captured settings patch is a checked primary read. It verifies membership even
though it changes nothing, then returns without a mutation receipt or version change.
Actual settings writes enable recovery receipts after scope verification while retaining
the primary transaction lock. Existing lost-commit reconciliation still applies. Other
mutations retain their existing receipt and no-op version behavior. Captured configuration
writes check the exact enrollment and any target-guild move fence in one SQL statement.
On remote libSQL, transaction setup likewise checks the restore fence and creates the
lost-commit receipt in one statement. These preserve the same fail-closed checks while
removing avoidable serial provider round trips from each save.

No new dependency, connection pool, cache or per-message work is added. These checks are
not a transaction spanning Discord and SQL. Shared/dedicated routing, selected cases and
live guild erasure now run through [tenant runtime](TENANT-RUNTIME.md). The current
schema remains `jmodguard-core-1`, migrations 0001–0013.

Local tests cover stale scopes before and after rejoin, empty patches, nickname re-sync,
source/destination copy checks, global writes from a stale guild context, modified identity,
rollback, waiting for the settings lock, exact badword scope and unchanged version markers.
The disposable remote libSQL test covers lost commit responses for scoped settings, inserts
and copies and verifies receipt-free empty patches. It does not establish live Bunny or
Discord behavior. The service-boundary guard permits the DB/API captured configuration path
and requires captured administrative queue access.


## HTTP and browser contract

`GET` or `POST /api/guilds/{id}/config` returns `enrollment_id` alongside the configuration. For a
nonzero guild path, configuration mutations and the individual reads below must send it as
`x-jmodguard-enrollment`. The initial `/config` request obtains that token without requiring one.
The token identifies the page's bot membership; it grants no permissions and is not a
configuration revision or an optimistic lock between concurrent editors. Fresh Discord
permissions still apply on every guild request, including installation-owner requests.

Missing/stale tokens return HTTP 409 with `guild_enrollment_changed`. An absent/retired
membership returns 409 `guild_enrollment_inactive`. Membership verification failures return
503 `guild_access_unverified` without DB details. The browser does not refresh tokens or
retry a rejected read or mutation automatically. Reload configuration and review before resubmitting.
Unsaved drafts may survive a failed refresh only within the same captured membership.

Copy selection loads the source configuration and retains its membership. The copy body
must include `from_enrollment_id` with the existing `from_guild_id` and `sections`; the
request header identifies the destination page. Both endpoints receive fresh authorization
and are checked together in the copy transaction. Reselect the source after a rejoin.

An owner edit to a global target through a guild path retains that guild's membership check.
Explicit `/api/guilds/0/...` routes are an independent owner-only global context and require
no guild token. Copying into global configuration still requires the selected source token.

Deploy matching API and frontend builds and reload previously open pages: older frontend
builds lack the required tokens on some routes and those guild requests are rejected. The bot must
have observed each guild before its dashboard is accessible; existing settings alone do
not grant access. No installation-owner bypass or automatic enrollment adoption is provided.

[Moderation event, ban and statistics reads](MODERATION-READS.md) also require the page token
and verify membership with their data. Individual configuration reads now follow the same
captured membership contract described below. Case, evidence and case-removal-role routes
use the same page token against the live guild store. Unban and log-channel operations
use the page token and a stable request key for [queued bot dispatch](BOT-ACTIONS.md). Status, cancellation of unstarted work, and force-cancel of stuck log-channel requests
retain captured membership and current action permissions.
A missed Discord departure may leave an observation stale until reconciled. Live tenant
isolation and guild erasure are described in [tenant runtime](TENANT-RUNTIME.md). The API
no longer retains or needs a bot token.

## Individual configuration reads

The following GET routes retain the loaded page membership after fresh permission checks:

| Path after `/api/guilds/{id}/` | Data in one primary snapshot |
|---|---|
| `rules/{rule_id}` | Requested rule's guild/global rows; existing default resolution follows. |
| `commands`, `commands/{name}` | Guild/global lists or requested command rows. Name normalization and default fallback are unchanged. |
| `templates`, `templates/{event}` | Guild/global lists or requested template rows; existing default fallback follows. |
| `badwords` | Guild and global rows together, in existing ID order; validation status is computed after reading. |
| `logchannels`, `logchannels/{event}` | Only that scope's mappings; no global inheritance; channel IDs remain strings on the wire. |

The seven `get_enrolled_*`/`list_enrolled_*` data helpers use the existing `ConfigurationScope`.
Guild/global pairs return `ConfigurationLayers`; a global owner context retains the previous
response shape with global rows in both fields. Scope values are not credentials. HTTP must
still require the installation owner for guild zero and fresh Manage Guild authority for
real guilds, including when the user is the installation owner.

The primary read verifies membership even for empty results, captures the configuration
version and reads only the needed section or key. Its version must satisfy the process's
committed-version floor, as for the main configuration bundle. It adds no mutation receipt,
version change, schema, dependency, pool, cache or polling. Shared SQL/decoding preserves bot
helpers and response semantics. The primary writer lock spans verification and data. The
WebUI/config-parts bundle uses one SQL statement for the collections so a remote Hrana
interactive transaction is not a per-table round-trip loop. Individual section reads still
select only the requested key. This change does not add pagination, resource limits or
distributed scaling. Rollback after a successful read is not COMMIT; if the provider already
dropped that stream (`TRANSACTION_TIMEOUT` / no active transaction), the collected snapshot
stays determinate and reads may retry the whole snapshot.

Local and disposable libSQL tests cover stale/forged scopes, global owner versus guild
permissions, page tokens, empty/unavailable reads, read floors, receipt/version preservation,
inheritance, exact keys, foreign-guild exclusion, wire values, the one-statement WebUI/config-parts
bundle matching layer helpers, and retrying `TRANSACTION_TIMEOUT` without retrying membership
errors. Local tests also cover a read waiting behind a membership-changing writer and paired
layers during concurrent updates. No live provider behavior or complete tenant isolation is
established by these tests.
