# Captured configuration scopes

The data layer now provides configuration operations that retain the membership captured
for a request and verify it in the same primary transaction as the operation. If the bot
leaves or rejoins before that transaction, the old request fails with `EnrollmentChanged`.
The request is never silently rebound to the replacement membership.

These helpers are groundwork for dashboard integration. Current API and bot callers still
use their existing guild-ID operations and permission checks. No HTTP route, browser token,
new permission, schema migration or background task is introduced here.

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
resolve a fresh membership immediately before writing an old browser request. Full API
admission, browser context and tenant-data isolation remain separate work.

## Operations

| Captured operation | Transaction contract |
|---|---|
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
Discord behavior. Runtime use of these helpers is blocked by the service-boundary guard
until the API admission and browser contract is implemented as a complete follow-up.
