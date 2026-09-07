# Conductor development

The conductor is the independent installation-control process being built on the
`overhaul` branch. Current code supplies process startup and status only. Updates,
backups, recovery, owner authentication and provider operations are not available yet.
Do not route production administration traffic to this development service.

## Build and run locally

From the source workspace, using the pinned Rust toolchain:

```bash
cargo build --locked -p jmodguard-conductor
cargo run --locked -p jmodguard-conductor
```

It defaults to `127.0.0.1:8081`. It does not read `config.toml`,
`JMODGUARD_CONFIG`, the bot's token, API OAuth settings or bot/API database credentials.
Current process settings are:

| Environment variable | Default | Purpose |
|---|---|---|
| `JMODGUARD_CONDUCTOR_BIND` | `127.0.0.1:8081` | IP address and port; no DNS lookup. |
| `JMODGUARD_CONDUCTOR_LOG` | `info` | Valid tracing filter; logs go to stdout. |
| `JMODGUARD_CONDUCTOR_SHUTDOWN_SECS` | `5` | Positive shutdown grace after SIGTERM/Ctrl+C. |

These are process/bootstrap controls. Durable installation options and credentials
will have separate validated configuration when the control store/setup is implemented.
No provider access key is accepted or required by this foundation.

## Status contract

| Request | Current response |
|---|---|
| `GET` / `HEAD /health` (also `/health/`) | `200`, process liveness only. |
| `GET` / `HEAD /ready` | `503`, JSON code `control_not_configured`. |
| `GET` / `HEAD /version` | `200`, service/version and `operations_available: false`. |
| Unimplemented paths | `404`; no provider or application action. |

Responses are `Cache-Control: no-store` and disclose no private resource mappings or
credentials. These status endpoints are unauthenticated. They are not owner operations
or evidence of database readiness. The process has no database/gateway/cache dependency.
There is no TLS listener or browser session implementation in this milestone.

SIGTERM and Ctrl+C trigger the shared shutdown latch. HTTP shutdown waits up to the
configured grace; incomplete client requests cannot hold the process indefinitely.
Future backup/cryptographic jobs need their own bounded work, drain and recovery design;
this HTTP grace is not permission to abandon an in-progress provider mutation.

## Validation and image

```bash
cargo test --locked --workspace
cargo clippy --locked -p jmodguard-runtime -p jmodguard-conductor --all-targets -- -D warnings
python3 scripts/check-service-boundaries.py
```

Process tests bind loopback ports and send SIGTERM to their own child on Unix. They
use synthetic inputs and no external account. The dependency check evaluates normal
runtime edges; it does not use package count as a memory estimate. Build with `-p
jmodguard-conductor` to select its graph independently of bot/API feature unification.
The foundation reuses already-locked Axum/Tokio/tracing/serde packages and adds no new
third-party package versions. It does not import the existing broad application state.

`Dockerfile.conductor` uses the same Rust pin and non-root runtime UID as the existing
images. Its bind is `0.0.0.0:8080`; liveness is `/health`, readiness remains `503`.
While source is private, the build uses the existing BuildKit `github_token` secret to
fetch the repository's patched Cargo dependency. It is never a runtime environment key.

```bash
docker build --secret id=github_token,env=GITHUB_TOKEN -f Dockerfile.conductor .
```

The image is not automatically published or deployed. Real resource budgets, request
admission limits, provider permissions and operational readiness remain acceptance work
as features are added. See [overhaul status](OVERHAUL.md).
