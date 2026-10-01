# Guild-store moves

The conductor can move one enrolled guild between an already-configured shared
store and that guild's dedicated store. Bot and API keep tenant routing; the
conductor copies SQL rows and updates the core directory. It does not import
`TenantRuntime`, Serenity or guild Discord credentials.

This is a local SQLite workflow. Live Bunny writer exclusion, credential
hot-reload and automatic shared-service restarts are not claimed.

## Requirements

- Conductor operations are configured.
- `JMODGUARD_CONDUCTOR_CAPTURE_STORES` includes a reachable `core` path (the
  application directory database).
- `JMODGUARD_CONDUCTOR_GUILD_STORES` lists at least one `shared` and one
  `dedicated` store as `[{id,role,path}]` with absolute paths and 32-hex
  catalog IDs. Those IDs must already exist in `guild_store_catalog`.
- Bot/API process configuration already contains the destination store
  credentials. Adding a new store is a restart of those services, confirmed in
  the move plan, not a secret delivered through the conductor.

## Workflow

`plan → checkpoint → freeze → copy → verify → switch → clean source → resume`

1. **Plan** shows source/destination, tenant row counts, neighboring guilds on
   a shared source, and the interruption/restart text.
2. **Checkpoint** (default) uses the R4 `checkpoint` archive kind when capture
   is configured. `recent` reuses a complete archive from the last six hours.
   `skip` is explicit: there is no pre-move archive.
3. **Freeze** inserts `guild_write_fences` for that guild only. Bot/API tenant
   writes, enrolled configuration/moderation writes, nickname jobs and Discord
   action dispatch for that guild fail closed. Other guilds continue. Overdue
   queued-action receipts still expire on their existing `expires_at`; they are
   not held `pending` merely because the guild is fenced.
4. **Copy** binds the destination with a new placement revision and copies this
   guild's membership, member directory and immutable initial-seed completion,
   log mappings, cases, extra sources, history and live
   evidence. Global defaults and credentials are not copied. Pending Discord
   action tables stay in core and are not replayed.
5. **Verify** checks copied membership/case/log/history and member-state counts,
   the immutable initial-seed completion marker, and member-directory row contents.
6. **Switch** updates `guild_store_placements` in the same core transaction as the
   fence phase change. Stale writers that still hold the old placement fail
   identity/membership checks after source cleanup.
7. **Clean** removes this guild's rows from the source store only. A leftover
   closed copy is not reused as a reverse-move shortcut; rollback is restore
   from the checkpoint.
8. **Resume** deletes the matching fence after source cleanup succeeds.

Interrupted running jobs fail closed. Manual retry resumes: if placement already
points at the destination revision, copy/switch are skipped and source cleanup
retries. A destination that already has a different binding is refused.

Rolling message snapshots are transient and are not copied to the destination.
The source retains them until successful cutover and source cleanup; a failed
copy must not erase them early. Source cleanup deletes that guild's snapshots
in transactions of at most 100 rows and removes membership only after none
remain. The write fence stays until that cleanup finishes. Destination capture starts with a history gap.
Required configuration and moderation rows must fit the application database
budget before copying begins. Its conservative copy reserve measures only this
guild’s durable SQL rows, including their indexes and transaction margin; it
excludes source free pages, other guilds and rolling snapshots. Source cleanup
reserves its evidence-state changes separately and must finish without growing
source allocation. An insufficient allowance refuses the move; it
does not raise any quota. See [database capacity](DATABASE-CAPACITY.md).

Recovery after a failed attempt keeps the original move generation in the
failed job payload. Use **Retry failed move** in the conductor Moves tab or
`POST /api/jobs/{id}/retry` on that job record. Starting a new move mints a
different destination revision and is rejected while the guild write-fence from
the failed attempt remains.

Backup/restore jobs and guild-move jobs share one idle mutex. Release install,
code rollback, and **retry of a failed `release_update` job** also use that mutex
and are refused while a guild write-fence from a failed move is still present
(see [updates](UPDATES.md)). Independent `recover-update` does not read fences.

## Core schema

Migration 0012 adds `guild_write_fences`. It is not a restore fence. When no
`guild_move` job is queued or running, the conductor only deletes fence rows whose
destination already matches `guild_store_placements` (orphans after cutover).
Fences for failed moves that copied to the destination but never switched placement
stay until manual retry or restore.

`rebind_placement` is a data-crate helper for tests and future callers. The
conductor uses SQL against the core file and must not call catalog write APIs
or open `GuildDataStore`.

## Limits

- Bunny object bytes are not copied; journal rows for live evidence may move.
- Unsettled Bunny PUTs can still fail destination insert.
- Writer leases and multi-replica fencing remain later work.
- Local tests are not live provider proof.
