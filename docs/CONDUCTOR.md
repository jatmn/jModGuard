# Conductor development

The conductor is an optional independent installation-control process. When its
control store, Discord OAuth client and operator identity are configured, it serves
an owner-only Administration UI for settings, service status, telemetry,
notifications and backups. Guild credentials, tenant routing and updates are not
available on this service.

## Build and run locally

From the source workspace, using the pinned Rust toolchain:

```bash
cargo build --locked -p jmodguard-conductor
cargo run --locked -p jmodguard-conductor
```

It defaults to `127.0.0.1:8081`. It does not read `config.toml`,
`JMODGUARD_CONFIG`, the bot's token, API OAuth settings or bot/API database credentials.

| Environment variable | Default | Purpose |
|---|---|---|
| `JMODGUARD_CONDUCTOR_BIND` | `127.0.0.1:8081` | IP address and port; no DNS lookup. |
| `JMODGUARD_CONDUCTOR_LOG` | `info` | Valid tracing filter; logs go to stdout. |
| `JMODGUARD_CONDUCTOR_SHUTDOWN_SECS` | `5` | Positive shutdown grace after SIGTERM/Ctrl+C. |
| `JMODGUARD_CONDUCTOR_DB` | unset | Local SQLite control store. Unset keeps operations unavailable. |
| `JMODGUARD_CONDUCTOR_PUBLIC_URL` | empty | Public origin for conductor OAuth redirects. |
| `JMODGUARD_CONDUCTOR_OWNER_ID` | `0` | Discord user allowed to operate this installation. |
| `JMODGUARD_CONDUCTOR_OAUTH_CLIENT_ID` / `_SECRET` | empty | Conductor-only Discord OAuth client (`identify` only). |
| `JMODGUARD_CONDUCTOR_BOT_HEALTH_URL` / `_API_HEALTH_URL` | empty | Optional unauthenticated `/health` probes. |
| `JMODGUARD_CONDUCTOR_BACKUP_ROOT` | unset | Dedicated backups-only directory. Required for `backup_available`. |
| `JMODGUARD_CONDUCTOR_BACKUP_KEY` | unset | Optional 64-hex master key. If unset, the conductor stores a generated key in the control database. |
| `JMODGUARD_CONDUCTOR_CAPTURE_STORES` | empty | JSON array of `{role,path}` local SQLite files to snapshot. `core` is the application directory database. Required for `capture_available` (scheduled/manual capture). Paths must be distinct from the backup root, evidence root and restore target. |
| `JMODGUARD_CONDUCTOR_EVIDENCE_ROOT` | unset | Optional live evidence directory copied into backups. Must be distinct from the backup root and restore target. |
| `JMODGUARD_CONDUCTOR_RESTORE_TARGET` | unset | Absent directory that restore writes into. Required to enqueue restore. |

These are process/bootstrap controls. Durable session, notification and monitoring
options are stored in the conductor control database after owner login. No provider
access key is accepted. A guild API session cookie cannot authenticate this origin.

Fresh setup stamps the operator identity on the first successful owner login. A
different Discord user cannot become operator. Restored control data cannot grant
operator authority by itself: OAuth and owner identity must be supplied independently.

## Status contract

| Request | Unconfigured | Configured |
|---|---|---|
| `GET` / `HEAD /health` (also `/health/`) | `200`, process liveness only. | Same. |
| `GET` / `HEAD /ready` | `503`, JSON code `control_not_configured`. | `200`. |
| `GET` / `HEAD /version` | `operations_available: false`; `backup_available` false; `capture_available` false. | `operations_available: true`; `backup_available` is true only with a dedicated backup root; `capture_available` is true only when that zone is present and a reachable `core` capture store is configured; `update_available` stays false. |
| `/login` | `404`. | Discord OAuth for the conductor operator. |

Sessions use the host-only `jmodguard_conductor_session` cookie stored in the control
database. Its browser lifetime follows the applied absolute-session setting; server-side
idle and absolute limits remain authoritative. Logout revokes that row; a restart keeps
unrevoked sessions. Administration
pages: System Status, Installation, Notifications, Backups. Updates remain
not available yet, with no working update buttons.

Backups use a dedicated local zone marked `.jmodguard-backup-zone`. Capture
takes a SQLite `VACUUM INTO` snapshot of each configured store, drops pending
Discord action tables, recursively copies evidence with its relative paths while
omitting intent-scoped evidence at capture time, queues `backup_sanitize` to
rewrite complete archives when `backup_cleanup_intents` rows exist (see
[BACKUP-RESTORE.md](BACKUP-RESTORE.md)), and writes a versioned authenticated
encrypted archive (`JMODGUARD-BACKUP-1`). Sanitize acknowledges those intent
rows only after at least one complete archive is rewritten.
Scheduled work defaults to every six hours,
retries/alerts on failure, and never pauses application writes. Maintenance
capture is an explicit owner action. Portable download wraps the archive with
an export password that is kept only in conductor memory while a queued restore
needs it. Failed backup jobs are listed on the Backups page for explicit retry
when no other backup job is already queued or running, without blocking later
scheduled capture attempts. Catalog overdue status uses the same last
scheduled-or-complete timestamp as the scheduler, and stays false while
`capture_available` is false.
Failed restores are resubmitted with a fresh password instead of replaying a
stored secret.
Uploads land in quarantine after a magic-header check; password authentication
and archive-family validation occur during restore. Restore writes into
`JMODGUARD_CONDUCTOR_RESTORE_TARGET` only; it does not overwrite the live
database in place. A marked sibling staging directory makes publication atomic
and permits retry after interruption. After total loss of current policy state,
restore requires an explicit historical acknowledgment. Surviving cleanup
intents still remove those guilds/objects. Discord tokens, OAuth secrets,
sessions and pending
`bot_actions` are not restored. Live Bunny export/durability is not claimed by
these local tests.

Owner Discord notifications use an incoming webhook stored write-only on the
conductor. Payloads never include tokens, chat, or evidence. Delivery health is listed
on the Notifications page. Mention parsing remains disabled by default and follows the
explicit Allow mention parsing setting when enabled. A test send queues a bounded job
that can fail and be replayed with a fresh retry budget. Jobs interrupted by a conductor
restart fail with an unknown-outcome message for deliberate operator replay instead of
being automatically redelivered.

Telemetry samples at the background interval unless an authorized operator holds a
dashboard lease (Status page visible). History is bounded by the configured hours and
row budget. Observed RSS is not a configured memory limit.

## Validation and image

```bash
cargo test --locked --workspace
cargo clippy --locked -p jmodguard-runtime -p jmodguard-conductor --all-targets -- -D warnings
python3 scripts/check-service-boundaries.py
```

Process tests bind loopback ports and send SIGTERM to their own child on Unix. They
use synthetic inputs and no external account. Control-store tests use a local OAuth
fixture. The dependency check evaluates normal runtime edges; the conductor still must
not depend on `jmodguard`, Serenity, bot/API crates or tenant routing.

`Dockerfile.conductor` uses the same Rust pin and non-root runtime UID as the existing
images. Its bind is `0.0.0.0:8080`. Readiness is `503` until the control store and
operator identity are configured in the container environment.

```bash
docker build --secret id=github_token,env=GITHUB_TOKEN -f Dockerfile.conductor .
```

The image is not automatically published or deployed. Guild database moves and
point-and-click updates remain later work. See [overhaul status](OVERHAUL.md)
and [backup and restore](BACKUP-RESTORE.md).
