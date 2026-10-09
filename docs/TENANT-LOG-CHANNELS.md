# Captured guild-store log-channel settings

The separate guild-store library now stores a guild's log-channel mappings with captured
placement and bot membership. Directory log-channel delivery still uses core helpers;
see [tenant runtime](TENANT-RUNTIME.md) for live store routing and guild erasure.

## Scope and lifecycle

`GuildDataScope::new` captures a catalog `GuildPlacement` and `GuildEnrollment`. Guild IDs
must match and be positive; membership ID is a 32-character lowercase hexadecimal token
and join time must be positive. Fields are private and no deserializer bypasses validation.
The scope grants no user permissions and does not prove the directory or Discord is current.
Trusted callers must capture those observations under the eventual lifecycle protocol.

`bind_initial_membership` requires an already-bound placement, checks store identity and
placement on primary, and stores only the first membership. Matching active retries write
nothing. Different IDs/join times and closed memberships conflict, including newer joins:
there is no automatic adoption, rejoin, rebind or copying of another membership's settings.
Initialization and schema upgrade never infer memberships from catalog entries or core data.

`close_membership` checks the exact captured placement and membership and atomically removes
its log mappings and marks it closed. A concurrent edit either commits before closure and
is removed, or sees closure and fails. Other guilds are untouched. Repeating closure is
write-free. Closing a missing or mismatched scope fails. The closed identity remains to
reject stale initial binding. This is a local storage barrier, not full guild erasure;
coordinated cleanup of that identity, directory entries, other data and backups remains
required before runtime adoption. No permanent departed-guild ledger is a product goal.

## Reads and edits

Every operation uses one primary transaction to check store identity, placement revision,
exact membership ID/join time and active state alongside its data. This includes empty
reads, same-value edits and absent deletions. No earlier `verify_placement` result is used
as permission for later SQL. The raw pool remains private.

| Operation | Result |
|---|---|
| `log_channels(scope)` | Sorted event/channel mappings and their current revision in a `LogChannelSnapshot`. Only this guild/membership is read; there is no global inheritance. |
| `set_log_channel(scope, revision, event, Some(channel))` | Insert/change one mapping if the loaded revision is current. Return the new revision for a change. |
| `set_log_channel(scope, revision, event, None)` | Remove one mapping with the same scope/revision checks. |

The revision is opaque and belongs to this membership's settings. Every real edit changes
it atomically with the mapping; it is independent of core config/slash versions. Stale
revisions fail even if the submitted value happens to match. Same-value/absent-delete calls
with the current revision preserve it and create no mutation receipt. Real mutations reuse
existing remote receipt recovery. An application-level lost response after a completed edit
can leave the caller with a stale token: refresh and resolve the conflict, never silently
substitute a new revision and resend the old edit. This is not a request-id replay protocol.

Only the existing `EVENT_TYPES` are accepted, with one mapping per event (currently14), and
channel IDs must be positive integers. Member role, role, voice, nickname, username, and
thread logs are separate opt-in names on the core log-channel table. They are not
guild-store event types, so this check constraint stays unchanged. That bounds row count and read memory without an
arbitrary configurable history quota. No event payload is stored. Future event additions
must update the schema constraint and shared event types together. Actor permissions and
Discord channel ownership/availability must be checked before runtime mutation; positive
IDs and database scope alone do not establish them. No cross-guild copy operation is supplied.

SQL guards require a bound placement and active matching membership for log inserts/updates.
They prevent membership replacement/rebinding/reopening and movement of a mapping to another
scope without relying on foreign-key or recursive-trigger settings. Credentials still give
trusted SQL access; these checks do not defend against an administrator replacing schema.

## Schema, verification and remaining integration

At the version6 stage documented here, `jmodguard-guild-1` used migrations0001–0006. Version2 added empty membership/mapping
tables; version3 adds [selected case storage](TENANT-CASES.md). `initialize` verifies the
expected version1–5 identity before upgrading atomically to6, preserving bindings/settings.
`open` requires6 and never migrates. Older guild-store binaries reject6; rollback requires
matching pre-upgrade backup and binary. Core stays10; current services never open these stores.

Local tests cover cross-guild isolation, wrong/missing/closed scopes, validation, persistence,
all supported event types, conflicting edits, no-op writes, closure races/rollback, SQL
constraints and version1 upgrade. Disposable libSQL tests cover lost responses for membership,
mapping and closure commits, cross-handle reads and stale revision rejection.

Routing must still combine this storage layer with authoritative membership coordination,
global policy/configuration, bounded store handles, queue completion/version observation
and channel/user authorization. Closing this scope does not retire core membership, clear
service caches or recall an already-sent Discord request. A database clone can retain the
same identity and revisions; this is not distributed writer exclusion. Selected cases/text evidence now use this same captured scope. File evidence, other tenant
configuration and complete guild erasure remain later work. No live Bunny/Discord or resource-usage assurance is established by local tests.
