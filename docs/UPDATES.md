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
  containing `index.html`, `app.js` and `style.css`. The Guild WebUI still loads
  daisyUI and Headless UI from a CDN at runtime; those libraries are not extra
  files in the frontend artifact directory. Custom artifact roots must stay
  disjoint from backup, evidence, restore, capture, guild-store, and update roots.
- Optional `JMODGUARD_CONDUCTOR_UPDATE_PROVIDER` — HTTP fixture that records
  desired/observed/ready. It is not live Bunny proof.
- **First exclusive install:** when bot or API `/health` URLs are configured in
  conductor settings, `inventory.json` must already list those components (prior
  conductor-managed install or documented bootstrap). Otherwise apply fails closed.

`/version` `update_available` is true only when operations, feed, keys and the
update root are configured.

## Workflow

`planned → checkpointed → staged → deploying → verifying → committed`

(`GET /api/updates/plan` is the operator preflight; it is not a separate job phase.)

1. **Plan** re-fetches the signed manifest, records compatibility/impact, component
   order (`frontend → API → bot → conductor`) and expected interruptions.
2. **Checkpoint** (default `fresh`) captures an R4 `checkpoint` archive when capture
   is configured. `recent` reuses any **complete** backup archive from the last six
   hours when one exists (same rule as guild moves); otherwise it captures fresh.
   `skip` is explicit: there is no pre-update archive.
3. **Exclusive mutation.** `release_update` shares the backup/guild-move idle
   mutex. One installation mutation at a time.

### Mutation entry points

| Entry | Recover gate | Guild write-fence | Incumbent inventory | `recover.json` |
|---|---|---|---|---|
| `GET /api/updates/plan` | New mutation (`idle` / absent / `committed` / `recovered`) | Yes | Yes | Read-only |
| `POST /api/updates` (enqueue) | New mutation | Yes | Yes | Not written |
| `POST /api/updates/{id}/rollback` | New mutation | Yes | Yes | Not written |
| Worker `update::run` | Retry-allowed (blocks `uncertain` only) | Yes | Yes | Written before deploy; **same-job retry keeps `previous` / `previous_release_id`** |
| `POST /api/jobs/{id}/retry` (`release_update`) | Retry-allowed | Yes | Yes | Replays job → same as `run` |
| CLI `recover-update` | Reads sidecar; refuses `committed` / `recovered` as no-ops | **No** (emergency path independent of conductor/OAuth; finish or retry a failed guild move before relying on guild writes) | N/A | Restores `previous` digests; sets `recovered` |

New installs and rollbacks are refused while `recover.json` is `uncertain`,
`failed`, or mid-`replacing`/`running`. They are also refused while any guild
**write-fence** remains (failed move recovery); retry the move job or clear the
fence before installing code. HTTP retry of a **failed** `release_update` is
allowed when recover is `failed`/`replacing`/`running` so the same job can
continue, but not when recover is `uncertain`. Guild write-fences still block
retry and worker execution.

### Recover sidecar lifecycle

| Status | Meaning | New install / rollback | HTTP retry of that job | `recover-update` |
|---|---|---|---|---|
| absent / idle | No in-flight mutation | Allowed | N/A | Error: no recover inventory |
| `running` / `replacing` | Deploy in progress or interrupted | Refused | Allowed (same job); preserves `previous` | Restores `previous` code |
| `failed` | Readiness reverted or verifying failed; ownership not uncertain | Refused | Allowed; preserves `previous` | Restores `previous` code |
| `uncertain` | Exclusive ownership not confirmed | Refused | Refused | Restores `previous` code |
| `committed` | Job finished successfully | Allowed | N/A | No-op |
| `recovered` | CLI already restored previous code | Allowed | N/A | No-op |

`recover.json` is written **before the deploy loop** (not only before conductor
replacement) so a crash during frontend/API/bot still has a restore baseline.

Forward `run` refreshes the SQL job `plan` from current inventory so operators
see live digests. That refresh must **not** replace `recover.previous` or
`previous_release_id` for the same `job_id` while recover is non-terminal.
`recover-update` always restores those preserved digests.

`inventory.json` is written with the same tmp+rename durability as `recover.json`.
Forward deploy and `recover-update` persist each component after it passes
readiness. When verify succeeds, `recover.json` is marked `committed` **before**
`inventory.release_id` and control-store `installed_release` advance, so
`recover-update` cannot restore `previous` after a completed deploy. Those
release labels still advance only on that commit path (or when CLI recover
finishes the loop and sets `release_id` to `previous_release_id`).

HTTP health is not mutation authority.

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

When `recover-update` restores code, the running conductor reconciles
`installed_release` meta from `inventory.json` on startup. Discovery **channel**
checkboxes filter the catalog and notifications only; install still requires a
compatible signed `release_id` from the feed (see [releases](RELEASES.md)).

## HTTP

| Request | Behavior |
|---|---|
| `GET /api/updates` | Cached catalog; polls when due or `?refresh=true`. Includes jobs and recover status when an update root is configured. |
| `GET /api/updates/plan?release_id=&preop=` | Compatibility/impact preflight (same preop/capture rules as install). |
| `POST /api/updates` and `POST /api/updates/apply` | Queue `release_update` when `confirm_interruption` is true. 404 when the update root is unset. |
| `POST /api/updates/{id}/rollback` | Queue code-only rollback of a committed update. |
| `POST /api/jobs/{id}/retry` | Re-queue a failed `release_update`. Same fence/inventory gates as `run`. Refused while recover is `uncertain`. |

## Recover command

```bash
JMODGUARD_CONDUCTOR_UPDATE_ROOT=/var/lib/jmodguard/update \
  jmodguard-conductor recover-update
```

Use this when conductor self-replacement fails. It restores previous artifacts
from `recover.json` and does not apply an old database. Drain and exclusive-ownership
checks must succeed (same fail-closed rules as apply). Each restored component is
persisted to `inventory.json` after readiness so a later failure does not hide
earlier restores. This command does not open the control store or consult guild
write-fences; finish or retry a failed move before relying on guild write behavior.
After recover, start or reload the conductor so Administration catalog meta matches
inventory.

See [releases](RELEASES.md), [backups](BACKUP-RESTORE.md) and [conductor](CONDUCTOR.md).
