# Cost formulas

These formulas use **local measurements** from this repository's install
acceptance tests plus **current provider list prices** that the operator must
read at purchase time. This file does not invent a Bunny or AWS invoice.

## How numbers are measured

`crates/conductor/tests/install.rs` samples:

| Sample | How | What it is |
|---|---|---|
| Idle conductor RSS | `/proc/<pid>/status` `VmRSS` after `GET /health` | Observed process memory, not a cgroup limit |
| Dashboard RSS | `GET /api/status` `conductor.rss_bytes` after owner login | Same observation while Administration is up |
| Backup input | 4 KiB stand-in core file | Archive size scales with capture + evidence bytes |
| Interruption | `jmodguard-conductor recover-update` without `UPDATE_ROOT` | Must fail closed quickly |

Re-run:

```bash
cargo test --locked -p jmodguard-conductor --test install -- --nocapture
```

Debug binaries on CI-class hosts are expected to stay under **256 MiB** RSS for
an idle conductor. Treat that as a local bound, not a billed Magic Container
size. Bot/API idle/load under a live Discord Gateway is not measured here.

Backup archive size ≈ sum of VACUUM snapshots of configured capture stores +
copied evidence bytes + encryption overhead. Cleanup/growth follow the
configured retain counts in conductor backup settings.

## Monthly estimate (operator fills prices)

```
monthly ≈
  local_idle_power_watts × 730 h × electricity_price
  + bunny_database_price            # current Bunny list price
  + bunny_magic_container_price     # bot + API (+ optional conductor)
  + bunny_cdn_storage_and_pull      # frontend zone + traffic
  + aws_ec2_price                   # only if using docs/DEPLOY-AWS.md
```

Do **not** convert local RSS into Magic Container RAM billing. Do **not** copy
a dollar amount from an old worksheet. Read Bunny Database, Magic Container,
Storage, and Pull Zone prices, and AWS EC2 list prices, on the day you buy.

Local SQLite on a machine you already run adds no Bunny Database line.
RDS/Postgres is not a jModGuard path, so it has no formula here.

## Burst and interruption

- `/health` and `/version` are process-local and are not Discord load.
- Dashboard live sampling runs only while the Status tab holds a lease.
- Failed backups retry without pausing application writes.
- Conductor self-replacement uses `recover.json`; recover-update does not
  require private ops.

Live billed usage under production Discord load remains a deferred provider
check ([INSTALL.md](INSTALL.md)).
