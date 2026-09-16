# Public installation

A new operator can install from **this public repository** without the private
ops repo or this development workspace. Bunny.net remains the first-class cloud
path. Local Linux and an honest AWS EC2 route are documented here.

The license stays **PolyForm Noncommercial 1.0.0** (`LICENSE.md`).

This page is source-delivery acceptance: a disposable local install and recovery
drill that uses public artifacts and docs. It is **not** live Bunny, Discord, or
AWS proof. Eight merged overhaul PRs do not prove production cutover.

## Choose a topology

| Path | Doc | What it is |
|---|---|---|
| Bunny (first-class) | [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md) | CDN admin UI, private Magic Containers, Bunny Database / libSQL |
| Local Linux | [SETUP.md](SETUP.md) | Bot + API on one machine; optional conductor |
| AWS | [DEPLOY-AWS.md](DEPLOY-AWS.md) | EC2 with the local adapter and SQLite or compatible libSQL. Not RDS/Postgres. |
| Cost | [COST.md](COST.md) | Formulas from local measurements plus current provider list prices |

Private GitHub Actions that publish Storage files are an **optional extra lock**
([examples/private-ops](examples/private-ops/README.md)). They are not required.
Buildkite publishes bot and API images from this repository on every main SHA.
Conductor images and a complete signed `JMODGUARD-RELEASE-1` manifest ship only on
scheduled Nightly, Stable tag, and manual builds ([BUILDKITE.md](BUILDKITE.md)).

## Public artifacts

Use a git tag or the matching Buildkite image digests:

- source: `https://github.com/jatmn/jModGuard`
- images on every main SHA: `ghcr.io/jatmn/jmodguard-bot`, `ghcr.io/jatmn/jmodguard-api`
- conductor image and installable release manifest: Nightly / Stable tag / manual
  Buildkite builds only (`ghcr.io/jatmn/jmodguard-conductor`); on main SHA build
  from source (`cargo build --locked -p jmodguard-conductor`) or pin a tagged release
- admin UI files: `frontend/index.html`, `frontend/app.js`, `frontend/style.css`
  (from source or release manifest; Buildkite does not upload a separate frontend
  artifact bundle—see [BUILDKITE.md](BUILDKITE.md))
- license: `LICENSE.md`

Do not copy secrets into git. Bunny Storage passwords, Discord tokens, OAuth
secrets, and database tokens stay with the operator.

## Validate a plan

The conductor checks topology, distinct private roots, scope-limited URLs, and
that private ops is not required:

```bash
cargo build --locked -p jmodguard-conductor
jmodguard-conductor init-local --root /var/lib/jmodguard
jmodguard-conductor validate-install --plan /var/lib/jmodguard/plan.json
```

`init-local` writes disjoint `backup/`, `evidence/`, `update/`, `artifacts/`,
`capture/`, and `guild/` directories, env examples without secrets, and
`NEXT-STEPS.txt`. It does **not** create `restore/` — restore writes into an
absent target.

Owner Administration can run the same check: Installation → Validate install
plan (`GET /api/install`, `GET /api/install/plan`, `POST /api/install/validate`).

## Guided owner setup

1. Create a **fresh** `jmodguard-core-1` database or empty local SQLite file.
   Never adopt an unmarked old database ([DATABASE-BASELINE.md](DATABASE-BASELINE.md)).
2. Create a **separate** guild store. Never point guild data at the core
   database, the public frontend zone, or the backups-only zone.
3. Fill bot token only on the bot process. Fill dashboard OAuth only on the API.
   Fill conductor OAuth (`identify` only) only on the conductor.
4. Start **bot**, then **API**, then **conductor**.
5. Login as the configured owner. The first successful conductor login stamps
   operator identity.
6. Record the backup key before the first portable export
   ([BACKUP-RESTORE.md](BACKUP-RESTORE.md)).
7. Practice restore into the absent restore target. After conductor
   self-replacement, run `jmodguard-conductor recover-update` independently of
   OAuth and private ops ([UPDATES.md](UPDATES.md)).

## Local disposable install and recovery drill

```bash
python3 scripts/install-local.py --root /tmp/jmodguard-disposable
# fill the generated env examples locally
cargo run --locked -p jmodguard-bot
cargo run --locked -p jmodguard-api
cargo run --locked -p jmodguard-conductor
```

Recovery without live providers:

- Local backup/restore tests: `cargo test --locked -p jmodguard-conductor --test backup`
- Update/rollback and recover-update: `cargo test --locked -p jmodguard-conductor --test updates`
- Guild-store moves: `cargo test --locked -p jmodguard-conductor --test moves`
- Public install/resource checks: `cargo test --locked -p jmodguard-conductor --test install`
- Dual-process bot/API: `cargo test --locked -p jmodguard-integration-tests --test dual_process_smoke`

`scripts/public-install-acceptance-test.py` checks that these docs stay public-source
and do not require private ops.

## Still-deferred live provider checks

Local tests do **not** replace:

- Live Bunny Database TLS and production credentials
- Live Magic Container deploy, private-zone confirmation, and CDN origin-pull
- Live Discord Gateway, OAuth redirect, and guild permission checks
- Live Bunny evidence zone privacy and attachment capture
- Live AWS account, VPC, and billed resource provisioning
- Provider-billed cost at current Bunny/AWS list prices
- Multi-replica session stores and writer leases

Production readiness stays blocked on those checks.
