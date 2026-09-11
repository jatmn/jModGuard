# Guild-store identity and placement foundation

The data layer now stores nonsecret directory metadata for shared and dedicated guild
stores. This is a library foundation: the bot/API still uses its single configured database.
No service route, startup registration, automatic placement, provider provisioning or guild
move is enabled. The service-boundary guard prevents accidental runtime adoption.

## Stored identities

Migration0010 creates two empty tables in the existing `jmodguard-core-1` database:

- `guild_store_catalog`: opaque 32-character lowercase hexadecimal store ID and an optional
  dedicated guild ID. No dedicated owner means a shared store. Ownership is immutable.
- `guild_store_placements`: positive guild ID, registered store ID and opaque placement
  revision. Multiple guilds may select a shared store; only its owner may select a dedicated
  store. An index supports store reference checks without scanning every placement.

Store IDs are installation identifiers, not provider IDs, database URLs, credential handles
or proof that a physical store exists. Keep connection details and credentials in protected
installation configuration. Registration alone cannot establish physical isolation.
The separate store handle below checks the database against that registration.
Membership/enrollment IDs remain a separate lifecycle concept.

## Library operations

`jmodguard::db::guild_stores` exposes typed IDs, registrations, placements and four operations:

| Operation | Contract |
|---|---|
| `register_store` | Register an explicit ID and shared/dedicated policy. Same ID/policy retry returns the existing record. A changed policy conflicts. |
| `store_registration` | Primary lookup of a single ID; missing IDs return `None`. |
| `assign_initial_placement` | Assign an unplaced guild to a registered, permitted store. Same guild/store retry returns the original revision. Any different placement conflicts. |
| `guild_placement` | Primary lookup of one guild and validation of its referenced store/ownership. Missing placement returns `None`; invalid references fail. |

Generate a `StoreId` once and retain it across registration retries. Store/placement fields
are private with read accessors; no deserializer can bypass ID construction checks. Invalid
guild IDs are rejected, and ID validation errors never echo a supplied URL or secret.

All decisions share one primary transaction. Actual writes use existing mutation receipts
and lost-commit reconciliation; reads, matching retries and rejected changes create no
receipt. None changes configuration/slash versions. SQL checks and triggers independently
reject unknown/dedicated-owner-mismatched placements, invalid revisions, changed catalog
ownership and deletion of a referenced store. This does not depend on a connection-local
foreign-key PRAGMA, including over remote libSQL streams.

These operations are trusted internal storage APIs, not user authorization. Future callers
must authenticate and authorize installation-level operations; a guild user cannot select
an arbitrary database. No catalog write is exposed through today's API.

## Separate store schema and handle

`jmodguard::db::guild_data` adds `GuildStoreOptions` and `GuildDataStore`. Trusted
installation code supplies connection settings and the expected `StoreRegistration`.
Options intentionally have no Debug/Serialize implementation. Missing remote URL/token
pairs fail instead of falling back to a local file. Synchronous open/initialization belongs
on a startup or blocking worker, like core database opening.

The separate `jmodguard-guild-1` family uses migrations0001–0003. Version1 introduced
an immutable singleton identity (family, store ID, shared/dedicated owner), immutable initial
guild/revision bindings and mutation receipts. Version2 adds captured membership and
log-channel settings; see [transactional tenant operations](TENANT-LOG-CHANNELS.md).
Version3 adds [selected cases and bounded text evidence](TENANT-CASES.md).
There are no core configuration/default/enrollment tables or general event archives.
Core bootstrap never applies this schema; the existing core version remains10.

| Operation | Contract |
|---|---|
| `initialize` | Initialize an empty database atomically, upgrade a matching version1/2 store to version3, or verify a current identity. Refuse core, foreign, unmarked, newer or mismatched stores without adopting them. |
| `open` | Verify the current version3 schema, identity and ownership; create no schema or binding. |
| `bind_initial_placement` | Verify identity and install the catalog's initial guild/revision in one primary transaction. Matching retries write nothing; a different revision or dedicated owner conflicts. |
| `verify_placement` | Check identity, ownership and the exact stored guild/revision in one primary snapshot; missing bindings fail. This is not authorization for a later separate data operation. |

The wrapper keeps its raw pool private and reuses the existing SQLite/libSQL transport,
HTTPS connector, timeouts, pool and receipt recovery. Clones share that pool; separately
opened handles allocate their own configurable pool (minimum2). Initialization reconciles
a lost commit response by checking the committed identity/schema on primary; subsequent
binding writes use mutation receipts. Reads and matching retries create no receipts.
Identity/schema checks run again on every binding/check operation, including after open.
SQL guards prevent identity/binding updates, deletes and replacement, and enforce dedicated
ownership without relying on foreign-key or recursive-trigger settings.

The stamp is a logical identity stored in the database, not provider attestation: a restored
or cloned database can retain the same ID and revision. Checks detect a different expected
identity/policy and missing required tables/triggers; they are not a full schema-integrity
validator or protection from an administrator who can replace SQL definitions. No new
third-party dependency, service task, live connection or routing cache is enabled.

## Activation and recovery limits

Placement revisions are metadata, not writer leases. Looking up a placement cannot safely
move a guild or stop an older process from writing. Runtime adoption must include
the captured checks used by tenant log-channel operations across the remaining data model,
bounded handle/transport reuse, shared/dedicated routing, global-policy separation and
coordinated quiescing before moves. `verify_placement` alone is insufficient.
There is deliberately no placement-update/delete or store-delete library operation yet.
Raw SQL is not a supported move mechanism.

The catalog tables remain in the existing application database; identity and tenant
tables belong to separate guild-store databases. The directory/global-policy database split remains future work. No core guild data,
case/evidence records or credentials are copied into these tables; historical guild data
and membership are unchanged. This is not the final tenant schema or removal/purge mechanism.

Start matching bot/API binaries with bot migration first. Version9 binaries reject version10;
rollback needs the pre-upgrade database and matching binaries. No backup automation, live
Bunny validation, resource measurement or distributed consistency certification is supplied.
Local tests cover separate schema rejection, identity replacement after open, conflicting
revisions and receipt-free binding retries, as well as fresh/recognized-core startup,
persistence, no automatic placement, shared/dedicated isolation, conflicting/concurrent requests, receipt-free retries and SQL
constraints with foreign keys disabled. Disposable loopback libSQL checks cross-connection
reads, actual lost-commit recovery and dedicated ownership on remote transaction streams.

The core and guild-store remote smoke binaries each need their own empty disposable libSQL
server. CI runs them sequentially against separate containers at the same pinned digest.
The guild-store smoke covers schema initialization and binding after lost commit responses,
reopen visibility and wrong-store rejection. It never reuses or resets the core test database.
