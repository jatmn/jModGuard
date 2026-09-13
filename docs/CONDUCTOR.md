# Conductor development

The conductor is an optional independent installation-control process. When its
control store, Discord OAuth client and operator identity are configured, it serves
an owner-only Administration UI for settings, service status, telemetry and
notifications. Guild credentials, tenant routing, backups and updates are not
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
| `GET` / `HEAD /version` | `operations_available: false`. | `true`; `backup_available` and `update_available` stay false. |
| `/login` | `404`. | Discord OAuth for the conductor operator. |

Sessions use the host-only `jmodguard_conductor_session` cookie stored in the control
database. Logout revokes that row; a restart keeps unrevoked sessions. Administration
pages: System Status, Installation, Notifications. Updates and Backups are shown as
not available yet, with no working action buttons.

Owner Discord notifications use an incoming webhook stored write-only on the
conductor. Payloads never include tokens, chat, or evidence. Delivery health is listed
on the Notifications page. A test send queues a bounded job that can fail and be
replayed.

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

The image is not automatically published or deployed. Guild database moves, backups and
point-and-click updates remain later work. See [overhaul status](OVERHAUL.md).
