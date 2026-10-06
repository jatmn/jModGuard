# Guild-store identity and placement

The data layer stores nonsecret directory metadata for shared and dedicated guild
stores. Bot and API processes now route live tenant data through `TenantRuntime`; see
[tenant runtime](TENANT-RUNTIME.md). Catalog rows still never store URLs or tokens.
There is no supported operator workflow for moving populated guilds between stores.

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

`jmodguard::db::guild_stores` exposes typed IDs, registrations and placements:

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

The separate `jmodguard-guild-1` family uses migrations 0001–0014. Version1 introduced
an immutable singleton identity (family, store ID, shared/dedicated owner), immutable initial
guild/revision bindings and mutation receipts. Version2 adds captured membership and
log-channel settings; see [transactional tenant operations](TENANT-LOG-CHANNELS.md).
Version3 adds [selected cases and bounded text evidence](TENANT-CASES.md).
Version4 adds [owned append-only case history](CASE-HISTORY.md).
Version5 adds [explicit source associations](CASE-SOURCES.md).
Version6 adds an [evidence ownership journal](CASE-EVIDENCE.md).
Version7 adds durable [Bunny transfer attempts](BUNNY-EVIDENCE.md) for the opt-in HTTP adapter.
Version8 allows closed-scope erasure after owned data is gone and stores delegated
case-removal role IDs. Version9 adds bounded, latest-observed message snapshots with
fixed 24-hour expiry and store-level capture health. Departure erasure deletes
those rows in transactions of at most 100 and removes membership only after
they are gone. Version10 adds the bounded member directory, stays, case projection,
sync state and immutable initial-seed completion, plus member-state observations,
durable targeted checks and bounded audit recovery ([MEMBERS.md](MEMBERS.md)).
Version11 tracks coverage-gap generations and last-successful-run totals, and
updates member-state observation constraints. Version12 adds nullable committed
counts by import phase and a measured member-page interval; old aggregate counts
do not invent historical phase counts.
Version13 preserves exact, dated bot-executor audit actions while their local case
source association remains unresolved; a later exact source link consumes the staged
action without creating a duplicate case. Version14 lets case-retention updates
preserve in-flight member import pages. There are no core
configuration/default/enrollment tables or general event archives. Core bootstrap
never applies this schema; the existing core version is 17.

| Operation | Contract |
|---|---|
| `initialize` | Initialize an empty database atomically, upgrade a matching earlier store to version14, or verify a current identity. Refuse core, foreign, unmarked, newer or mismatched stores without adopting them. |
| `open` | Verify the current version14 schema, identity and ownership; create no schema or binding. |
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

Placement revisions are metadata, not writer leases. A placement lookup cannot stop an
older process from writing. Live operations require captured tenant checks in the same
transaction; `verify_placement` alone is insufficient. There is no store-delete or
placement-rebinding library operation. Persisted `guild_write_fences` still block writes
and erasure so an interrupted operation cannot silently become writable after an upgrade.

The catalog tables remain in the existing application database; identity and tenant
tables belong to separate guild-store databases. The directory/global-policy database split remains future work. No core guild data,
case/evidence records or credentials are copied into these tables; historical guild data
and membership are unchanged. This is not the final tenant schema or removal/purge mechanism.

Start matching bot/API binaries with bot migration first. Version10 and version11 binaries reject version12;
rollback needs the pre-upgrade database and matching binaries. No backup automation, live
Bunny validation or distributed consistency certification is supplied. The local
SQLite [member-directory sizing fixtures](MEMBERS.md#storage-sizing) estimate
stored bytes only.
Local tests cover separate schema rejection, identity replacement after open, conflicting
revisions and receipt-free binding retries, as well as fresh/recognized-core startup,
persistence, no automatic placement, shared/dedicated isolation, conflicting/concurrent requests, receipt-free retries and SQL
constraints with foreign keys disabled. Disposable loopback libSQL checks cross-connection
reads, actual lost-commit recovery and dedicated ownership on remote transaction streams.

The core and guild-store remote smoke binaries each need their own empty disposable libSQL
server. CI runs them sequentially against separate containers at the same pinned digest.
The guild-store smoke covers schema initialization and binding after lost commit responses,
reopen visibility and wrong-store rejection. It never reuses or resets the core test database.
