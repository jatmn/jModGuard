# Backup and restore

**Database compatibility:** this release requires a fresh database or the recognized
`jmodguard-core-1` family. Earlier databases/backups require their matching old release;
they cannot be adopted by this baseline. See [database setup and recovery](DATABASE-BASELINE.md).

This document separates a provider's internal durability from an
operator-controlled backup that can be exported, retained independently, and
restored into a different database.

## Conductor Administration

The conductor can create, list, download, upload and restore authenticated
encrypted archives when `JMODGUARD_CONDUCTOR_BACKUP_ROOT` is set. See
[CONDUCTOR.md](CONDUCTOR.md) for the environment keys.

- Format `JMODGUARD-BACKUP-1` with ChaCha20-Poly1305 frames and HKDF/PBKDF2 key
  wrapping. Scheduled archives use the process/control master key. Portable
  downloads wrap the same payload with an export password that is never stored.
- Capture snapshots each configured local SQLite store with `VACUUM INTO`, then
  drops `bot_actions`, `mutation_receipts` and `nickname_reconcile` so restore
  cannot replay Discord. Evidence files are copied from a distinct live evidence
  root. Objects listed in `backup_cleanup_intents` are omitted.
- Defaults (editable in Installation): every six hours, 24 scheduled points,
  seven daily points and three release checkpoints. Scheduled failures retry
  and alert; they never pause writes. Failed backup jobs are visible and
  retryable from the Backups page.
- Restore writes into an absent `JMODGUARD_CONDUCTOR_RESTORE_TARGET`. It does not
  overwrite the live database in place. After total loss of current policy state,
  the operator must acknowledge that later deletions cannot be verified. Surviving
  cleanup intents still remove those guilds/objects.
- Local tests do not prove Bunny session/durability, distributed writer exclusion
  or billed resource use.

Manual SQLite copies below remain valid for operators who are not using the
conductor.

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

**A Bunny recovery mechanism is not production-ready until it passes the live
acceptance drill below. Treat missing evidence as a production-readiness blocker.**

Operator tooling may implement export/restore/verify and pass local regression
tests while still lacking validation against Bunny's session, transaction and
size limits. Keep the tool version, actual drill results, encrypted off-service
backup reference, retention policy and responsible operator in the private ops
runbook. Local tests and provider durability are not proof of application recovery.
Tie production promotion to that evidence and the approved source/image digests;
keep updates disabled until the drill succeeds. Do not copy placeholder approval
records as evidence or infer that a running deployment has a usable backup.

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
5. Compare the exact schema SQL for tables, indexes, and triggers, including
   `PRAGMA user_version`, the migration/readiness metadata, and
   `sqlite_sequence` state. Row counts alone are not sufficient.
6. Produce deterministic per-table logical hashes on both databases. The
   canonical input must include every column's SQLite `typeof(...)`, a stable
   primary-key ordering, explicit NULL representation, and the complete value
   bytes (including JSON/text and blobs). Require every source/target hash to
   match; also compare row counts as a diagnostic.
7. Start the same-version bot against the target, then the API.
8. Verify a known guild configuration, moderation record, and audit event.
9. Perform and revert one harmless write through the WebUI.
10. Record the export/restore tool versions, source and target schema hashes,
    per-table content hashes and counts, export checksum, and drill result.

Do not test restoration by overwriting the source database.

## References

- [Bunny Database shell](https://docs.bunny.net/database/connect/database-shell)
- [Bunny durability and consistency](https://docs.bunny.net/database/durability-and-consistency)
