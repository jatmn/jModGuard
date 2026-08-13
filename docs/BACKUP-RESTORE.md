# Backup and restore

This document separates a provider's internal durability from an
operator-controlled backup that can be exported, retained independently, and
restored into a different database.

## Local SQLite

For a cold backup, stop both `jmodguard-bot` and `jmodguard-api`, then copy
`jmodguard.db` and its `-wal` / `-shm` sidecars if present. For a hot backup,
use SQLite's online backup API or:

```bash
sqlite3 jmodguard.db "VACUUM INTO 'jmodguard-backup.db'"
```

Do not copy a live database file while either process may be writing. Restore
a cold copy while both processes are stopped, then start the bot before the
API.

## Bunny Database

**There is not yet a complete operator-controlled Bunny backup and restore
procedure for jModGuard. Treat this as a production-readiness blocker.**

Bunny documents service-managed snapshots and WAL segments for durability.
Those mechanisms do not provide an operator-selected export, retention policy,
or documented self-service restore into a separate database.

Bunny's Database Shell can emit SQL interactively with `.dump` and execute a
SQL file during restore:

```bash
npm install -g @bunny.net/database-shell@0.2.3
bsql
bsql "$BUNNY_DATABASE_URL" --token "$BUNNY_DATABASE_AUTH_TOKEN" backup.sql
```

However, `bsql` 0.2.3 is **not accepted as a complete jModGuard backup path**:

- `.dump` requires an interactive TTY and confirmation and has no documented
  output-file option;
- it emits table definitions and rows, but not standalone indexes or triggers;
- it includes internal tables such as `sqlite_sequence`;
- jModGuard uses standalone indexes and `AUTOINCREMENT`, so replaying that
  output alone does not faithfully reconstruct its database.

The shell is still useful for inspection and disposable experiments, but do
not treat copied `.dump` output as the only backup of a deployment.

### Required acceptance test

Before a Bunny deployment is promoted beyond disposable acceptance testing,
provide an export mechanism that preserves all application tables, SQLite
value types, schema objects, and migration metadata. Then verify this drill:

1. Stop both apps so the export has a defined write boundary.
2. Export the source database to protected off-service storage and record a
   checksum.
3. Create a separate empty Bunny Database with separate credentials.
4. Restore the export without pointing either production app at the target.
5. Compare schema objects and per-table row counts between source and target.
6. Start the same-version bot against the target, then the API.
7. Verify a known guild configuration, moderation record, and audit event.
8. Perform and revert one harmless write through the WebUI.
9. Record the tool version, source version, checksums, counts, and result.

Do not test restoration by overwriting the source database.

## References

- [Bunny Database shell](https://docs.bunny.net/database/connect/database-shell)
- [Bunny durability and consistency](https://docs.bunny.net/database/durability-and-consistency)
