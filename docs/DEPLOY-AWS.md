# Deploy on AWS

This is the honest AWS route: **one EC2 host** (or similar VM) running the
public jModGuard binaries or container images with the **local process adapter**.
The database is **local SQLite** or a **compatible libSQL** replica you already
operate. There is **no** RDS, Aurora, or Postgres adapter in this source.

Bunny.net remains the first-class cloud provider ([DEPLOY-BUNNY.md](DEPLOY-BUNNY.md)).
Use AWS only when you are self-hosting on EC2.

## What this path is

```text
Browser  →  reverse proxy (HTTPS)  →  jmodguard-api (embedded UI or static files)
Discord  ←  jmodguard-bot
Owner    →  jmodguard-conductor     →  local backup / update roots
                 │
                 └─ SQLite files or libSQL URLs (not RDS)
```

- Bot token stays on the bot process.
- Dashboard OAuth stays on the API.
- Conductor OAuth is identify-only and never receives guild credentials.
- Backup, evidence, restore, update, and capture paths stay disjoint.
- `jmodguard-conductor validate-install` with `"profile":"aws"` rejects
  `postgres://`, `postgresql://`, and `rds.amazonaws.com` URLs.

## VM

1. Ubuntu/Debian x86-64 with the pinned Rust **1.96.0** toolchain, or pull the
   public GHCR bot and API images on every main SHA (`jmodguard-bot`,
   `jmodguard-api`). For `jmodguard-conductor`, build from source on main or pin
   a Nightly/Stable/manual Buildkite tag ([INSTALL.md](INSTALL.md)).
2. Open only the HTTPS reverse-proxy port to the internet. Keep bot and
   conductor binds on loopback or a private interface.
3. Put SQLite files on local disk (not NFS). Remote libSQL is the distributed
   option; it is still libSQL, not Postgres.
4. Follow [INSTALL.md](INSTALL.md) for `init-local`, owner login, backup-key
   recording, and `recover-update`.

Images still require `JMODGUARD_REQUIRE_REMOTE_DB=1` unless you intentionally
unset it for a local-SQLite container. On EC2 with SQLite files, unset it and
point `JMODGUARD_DB_PATH` at the capture core path.

## What is not provided

- RDS/Aurora/Postgres schema or migrations (Not RDS/Postgres)
- AWS Secrets Manager, IAM database auth, or ECS/EKS operators
- Automatic VPC, load-balancer, or multi-AZ failover
- A billed AWS cost quote (see [COST.md](COST.md))

Live AWS account, VPC, and billed provisioning remain deferred. Local
acceptance is the disposable install and recovery drill in [INSTALL.md](INSTALL.md).
