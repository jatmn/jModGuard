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

`plan → checkpoint → freeze → copy → verify → switch → resume → clean source`

1. **Plan** shows source/destination, tenant row counts, neighboring guilds on
   a shared source, and the interruption/restart text.
2. **Checkpoint** (default) uses the R4 `checkpoint` archive kind when capture
   is configured. `recent` reuses a complete archive from the last six hours.
   `skip` is explicit: there is no pre-move archive.
3. **Freeze** inserts `guild_write_fences` for that guild only. Bot/API tenant
   writes, enrolled configuration/moderation writes, nickname jobs and Discord
   action dispatch for that guild fail closed. Other guilds continue.
4. **Copy** binds the destination with a new placement revision and copies this
   guild's membership, log mappings, cases, extra sources, history and live
   evidence. Global defaults and credentials are not copied. Pending Discord
   action tables stay in core and are not replayed.
5. **Verify** checks copied membership/case/log/history counts.
6. **Switch** updates `guild_store_placements` in the same core transaction as the
   fence phase change. Stale writers that still hold the old placement fail
   identity/membership checks after source cleanup.
7. **Resume** deletes the matching fence.
8. **Clean** removes this guild's rows from the source store only. A leftover
   closed copy is not reused as a reverse-move shortcut; rollback is restore
   from the checkpoint.

Interrupted running jobs fail closed. Manual retry resumes: if placement already
points at the destination revision, copy/switch are skipped and source cleanup
retries. A destination that already has a different binding is refused.

Backup/restore jobs and guild-move jobs share one idle mutex.

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
