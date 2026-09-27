# Deploy on AWS

This is the honest AWS route: **one EC2 host** (or similar VM) running the
public jModGuard binaries or container images under your process supervisor.
The database is **local SQLite** or a **compatible libSQL** replica you already
operate. There is **no** RDS, Aurora, or Postgres adapter in this source.

Bunny.net remains the first-class cloud provider ([DEPLOY-BUNNY.md](DEPLOY-BUNNY.md)).
Use AWS only when you are self-hosting on EC2.

## What this path is

```text
Browser  →  reverse proxy (HTTPS)  →  jmodguard-api (embedded UI or static files)
Discord  ←  jmodguard-bot
Bot/API  →  separate core and guild SQLite files or libSQL URLs (not RDS)
```

- Bot token stays on the bot process.
- Dashboard OAuth stays on the API.
- Keep database and private evidence storage separate from public frontend files.

## VM

1. Use Ubuntu/Debian x86-64 with the pinned Rust **1.96.0** toolchain, or pull
   verified public GHCR bot/API images (`jmodguard-bot`, `jmodguard-api`).
   See [INSTALL.md](INSTALL.md) for source and image selection.
2. Open only the HTTPS reverse-proxy port to the internet. Keep service binds
   on loopback or a private interface.
3. Put SQLite files on local disk (not NFS). Remote libSQL is the distributed
   option; it is still libSQL, not Postgres.
4. Follow [SETUP.md](SETUP.md) for credentials, configuration and startup order.
   Verify your own backup and restore procedure before production.

Images still require `JMODGUARD_REQUIRE_REMOTE_DB=1` unless you intentionally
unset it for a local-SQLite container. On EC2 with SQLite files, unset it and
point `JMODGUARD_DB_PATH` at the core database path.

## What is not provided

- RDS/Aurora/Postgres schema or migrations (Not RDS/Postgres)
- AWS Secrets Manager, IAM database auth, or ECS/EKS operators
- Automatic VPC, load-balancer, or multi-AZ failover
- A billed AWS cost quote (see [COST.md](COST.md))

Live AWS account, VPC, and billed provisioning remain deferred. Local
validation is described in [INSTALL.md](INSTALL.md).
