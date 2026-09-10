# Captured configuration scopes

The data layer now provides configuration operations that retain the membership captured
for a request and verify it in the same primary transaction as the operation. If the bot
leaves or rejoins before that transaction, the old request fails with `EnrollmentChanged`.
The request is never silently rebound to the replacement membership.

The dashboard now uses these operations for settings, rules, commands, templates, badwords
and configuration copies. Guild requests first require fresh Discord user permissions and
an active bot membership from the primary database. The configuration bundle captures membership
and data in the same primary snapshot; writes verify it again in their transaction.
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
| `load_enrolled_web_config_snapshot` | Verify membership alongside configuration data in a primary read, preserving the API read floor. |
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
mutations retain their existing receipt and no-op version behavior.

No new dependency, connection pool, cache or per-message work is added. These checks are
not a transaction spanning Discord and SQL. Shared/dedicated routing, guild erasure, owner
recovery and runtime queue activation are not implemented by this boundary. The current
schema remains `jmodguard-core-1`, migrations 0001–0009.

Local tests cover stale scopes before and after rejoin, empty patches, nickname re-sync,
source/destination copy checks, global writes from a stale guild context, modified identity,
rollback, waiting for the settings lock, exact badword scope and unchanged version markers.
The disposable remote libSQL test covers lost commit responses for scoped settings, inserts
and copies and verifies receipt-free empty patches. It does not establish live Bunny or
Discord behavior. The service-boundary guard permits the DB/API captured configuration path
and requires captured administrative queue access.


## HTTP and browser contract

`GET /api/guilds/{id}/config` returns `enrollment_id` alongside the configuration. For a
nonzero guild path, configuration mutations must send it as `x-jmodguard-enrollment`.
The token identifies the page's bot membership; it grants no permissions and is not a
configuration revision or an optimistic lock between concurrent editors. Fresh Discord
permissions still apply on every guild request, including installation-owner requests.

Missing/stale tokens return HTTP 409 with `guild_enrollment_changed`. An absent/retired
membership returns 409 `guild_enrollment_inactive`. Membership verification failures return
503 `guild_access_unverified` without DB details. The browser does not refresh tokens or
retry a rejected mutation automatically. Reload configuration and review before resubmitting.
Unsaved drafts may survive a failed refresh only within the same captured membership.

Copy selection loads the source configuration and retains its membership. The copy body
must include `from_enrollment_id` with the existing `from_guild_id` and `sections`; the
request header identifies the destination page. Both endpoints receive fresh authorization
and are checked together in the copy transaction. Reselect the source after a rejoin.

An owner edit to a global target through a guild path retains that guild's membership check.
Explicit `/api/guilds/0/...` routes are an independent owner-only global context and require
no guild token. Copying into global configuration still requires the selected source token.

Deploy matching API and frontend builds and reload previously open pages: older frontend
builds lack these tokens and their guild configuration edits are rejected. The bot must
have observed each guild before its dashboard is accessible; existing settings alone do
not grant access. No installation-owner bypass or automatic enrollment adoption is provided.

[Moderation event, ban and statistics reads](MODERATION-READS.md) also require the page token
and verify membership with their data. Other individual configuration GET routes check
membership at admission but still need scoped snapshots. Unban and log-channel operations
use the page token and a stable request key for [queued bot dispatch](BOT-ACTIONS.md). Status and cancellation
retain captured membership and current action permissions.
A missed Discord departure may leave an observation stale until reconciled. This wave does
not provide full tenant isolation, erasure, distributed worker ownership or atomicity across
Discord and SQL. The API no longer retains or needs a bot token.
