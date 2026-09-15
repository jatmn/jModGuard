# Point-and-click updates and rollback

The conductor can install a selected compatible signed release for frontend, API,
bot and conductor, then roll that code back. Discovery stays a browse/poll path;
there is no silent automatic update and no Bunny registry. Live provider
exclusion/cutover is not claimed by these local tests.

## Requirements

- Conductor operations, a release feed and trust keys (R6 discovery).
- `JMODGUARD_CONDUCTOR_UPDATE_ROOT` — dedicated local install root for inventory,
  staged artifacts, `recover.json` and current frontend files. It must not overlap
  backup, evidence, restore, capture or guild-store paths.
- Optional `JMODGUARD_CONDUCTOR_ARTIFACT_ROOT` (defaults to `update_root/cache`).
  Process artifacts are files keyed by digest; frontend artifacts are directories
  containing `index.html`, `app.js` and `style.css`.
- Optional `JMODGUARD_CONDUCTOR_UPDATE_PROVIDER` — HTTP fixture that records
  desired/observed/ready. It is not live Bunny proof.

`/version` `update_available` is true only when operations, feed, keys and the
update root are configured.

## Workflow

`planned → preflight → checkpointed → staged → deploying → verifying → committed`

1. **Plan** re-fetches the signed manifest, records compatibility/impact, component
   order (`frontend → API → bot → conductor`) and expected interruptions.
2. **Checkpoint** (default) uses the R4 `checkpoint` archive kind when capture is
   configured. `recent` reuses a complete archive from the last six hours. `skip`
   is explicit: there is no pre-update archive.
3. **Exclusive mutation.** `release_update` shares the backup/guild-move idle
   mutex. One installation mutation at a time.
4. **Stage** copies digest-keyed artifacts. Missing files fail before activate.
5. **Deploy** drains the previous process, confirms it is dead, then activates the
   replacement and waits for health. Bot is exclusive: the old worker must be dead
   before the new one starts. Local API is one-replica stop/start. Frontend is a
   file switch.
6. **Conductor last.** Before replacing conductor, the adapter writes sidecar
   `update_root/recover.json`. Independent `jmodguard-conductor recover-update`
   reads that file and restores previous code. It does not use the replacement
   conductor, OAuth, or private ops.
7. **Commit** records `installed_release` in the control store. Interrupted
   `running` jobs fail closed on restart (`worker interrupted; delivery outcome
   unknown`); they do not activate a replacement.

Compatible **rollback** switches retained verified binaries/files. It does not
restore the previous database or rewind later settings or revocations. Restore a
matching backup to rewind data.

Uncertain ownership (force-kill, old instance still serving, failed replacement
that could not restore) pauses and writes `recover.json` status `uncertain`.
HTTP health is not mutation authority.

## HTTP

| Request | Behavior |
|---|---|
| `GET /api/updates` | Cached catalog; polls when due or `?refresh=true`. Includes jobs and recover status when an update root is configured. |
| `GET /api/updates/plan?release_id=` | Compatibility/impact preflight. |
| `POST /api/updates` and `POST /api/updates/apply` | Queue `release_update` when `confirm_interruption` is true. 404 when the update root is unset. |
| `POST /api/updates/{id}/rollback` | Queue code-only rollback of a committed update. |

## Recover command

```bash
JMODGUARD_CONDUCTOR_UPDATE_ROOT=/var/lib/jmodguard/update \
  jmodguard-conductor recover-update
```

Use this when conductor self-replacement fails. It restores previous artifacts
from `recover.json` and does not apply an old database.

See [releases](RELEASES.md), [backups](BACKUP-RESTORE.md) and [conductor](CONDUCTOR.md).
