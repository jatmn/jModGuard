# Guild-store catalog foundation

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
installation configuration. Multiple IDs could currently describe the same external database;
future provisioning must verify physical identity before routing. Registration alone cannot
establish physical isolation. Membership/enrollment IDs remain a separate lifecycle concept.

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

## Activation and recovery limits

Placement revisions are metadata, not writer leases. Looking up a placement cannot safely
move a guild or stop an older process from writing. Runtime adoption must add verified
physical-store identity, data-side placement checks, bounded handle/transport reuse,
shared/dedicated routing, global-policy separation and coordinated quiescing before moves.
There is deliberately no placement-update/delete or store-delete library operation yet.
Raw SQL is not a supported move mechanism.

The current tables are housed in the existing application database to land this foundation
independently. The directory/global-policy database split remains future work. No guild data,
case/evidence records or credentials are copied into these tables; historical guild data
and membership are unchanged. This is not the final tenant schema or removal/purge mechanism.

Start matching bot/API binaries with bot migration first. Version9 binaries reject version10;
rollback needs the pre-upgrade database and matching binaries. No backup automation, live
Bunny validation, resource measurement or distributed consistency certification is supplied.
Local tests cover fresh/recognized-schema startup, persistence, no automatic placement,
shared/dedicated isolation, conflicting/concurrent requests, receipt-free retries and SQL
constraints with foreign keys disabled. Disposable loopback libSQL checks cross-connection
reads, actual lost-commit recovery and dedicated ownership on remote transaction streams.
