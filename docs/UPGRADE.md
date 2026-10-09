# Upgrade guide

How to move an existing jModGuard install to a newer version (git tag or release).

For a **first** install, use [INSTALL.md](INSTALL.md) (Bunny, local, or AWS).
[SETUP.md](SETUP.md) is the local Discord walkthrough. For a Bunny
deploy, see [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md). If you maintain your own
registry/CDN/secrets pipeline, follow this guide for product steps, then apply
your operator runbook for promote and rollout.

## 0. Check database compatibility

This release starts the `jmodguard-core-1` family. An installation from before this
baseline must use a fresh database and be configured again; no in-place conversion or
old-backup import is provided. Keep any old environment and its backup paired with its
old binaries. Never add the marker manually to bypass the check.

Use [SETUP.md](SETUP.md) for that fresh installation. The steps below apply to subsequent
upgrades **within a supported family/version**, subject to each release's compatibility
notes. Both bot and API must match the selected release. The bot initializes the database;
the API waits for completed bootstrap. See [DATABASE-BASELINE.md](DATABASE-BASELINE.md).

Tenant-runtime releases also require a separate shared guild database for a remote
core. Before rollout, configure both services with the shared-store URL/token pair
and an explicit evidence backend from [the Bunny runbook](DEPLOY-BUNNY.md#1-create-the-core-and-guild-databases).
Local installs use `{core-path}.guild.db` and, on Unix, `{core-path}.evidence` by default.
Keep these stores and evidence resources with their core catalog during recovery;
a missing owned guild database fails closed. Guild schema version 15 requires matching
binaries; rollback needs the corresponding pre-upgrade stores, not just the core file.
The completed initial member import marker is preserved by compatible upgrades;
upgrading does not trigger another full roster or current-ban scan. See [MEMBERS.md](MEMBERS.md).

## 1. Pick a version

Prefer a **git tag** or release (`vX.Y.Z`) over an arbitrary `main` commit.
Nightly identities (`nightly-YYYYMMDD-<sha12>`) are separate from that stable
baseline. Owner Administration lists image candidates. See
[releases](RELEASES.md).
Read the release notes for:

- New or renamed `config.toml` / env keys
- Database migrations
- Breaking WebUI or Discord permission changes

## 2. Backup

Both `jmodguard-bot` and `jmodguard-api` can hold the shared DB open.

**Cold copy (safest):**

1. Stop **both** processes.
2. Copy `jmodguard.db` plus `jmodguard.db-wal` / `jmodguard.db-shm` if present.
3. Copy every configured local guild database and its WAL/SHM files, plus the private evidence roots, while both processes remain stopped.
4. Keep the previous binaries or container image tags until you verify the upgrade.

**Hot copy:** use SQLite online backup or
`sqlite3 jmodguard.db "VACUUM INTO 'jmodguard-backup.db'"` — do not file-copy
the `.db` while either process is writing. See [CONFIGURATION.md](CONFIGURATION.md).

For Bunny Database, the local file-copy procedure does not apply. An
operator-controlled recovery method must pass a live drill; `bsql .dump` alone is
insufficient for the jModGuard schema. Do not perform a production upgrade
until a complete restore drill for your databases and evidence passes.

## 3. Build or pull the new artifacts

From the tagged source:

```bash
git fetch --tags
git checkout vX.Y.Z   # or your chosen tag

cargo build --release --locked -p jmodguard-bot
cargo build --release --locked -p jmodguard-api
```

Or rebuild/push container images from `Dockerfile.bot` / `Dockerfile.api` at
that tag. On Bunny, upload `frontend/` from the **same** tag when the page
needs the new API; CSS/layout-only files can publish independently. Purge
the Pull Zone after the upload.

## 4. Config diff

Compare your `config.toml` to `config.example.toml` at the new tag. Add any
new keys (for example `bot.config_poll_secs` or `db.url` / `db.token`).
Env overrides (`JMODGUARD_*`) still win over the file.

## 5. Rollout order

With the launcher-aware coordinated-update baseline installed, supported in-container upgrades
use **Administration → Updates → Update bot and API together / recovery**.
That flow enforces the stop/migrate/start ordering below and persists restart
recovery. See [Bunny updates](BUNNY-UPDATES.md#schema-upgrades-and-recovery).
Normal Update installs a required launcher before its application; the update page
shows whether that step is necessary. Optional launcher-only updates live under
Advanced launcher controls. See `BUNNY-UPDATES.md` for release capability labels.

The manual sequence remains for the one-time baseline and changes to the base OS,
configuration, credentials, or incompatible control protocol.


1. Stop the old API, then the old bot (or stop both).
2. Install the new binaries/images (and SPA assets if separate).
3. **Start the bot first** — it runs migrations (`migrate=true`).
   It resumes supported schema steps, seeds defaults and heals configuration overrides
   when needed. It does not adopt old/unmarked databases. The API waits for the durable
   bootstrap marker and terminal defaults written by the bot.
4. Confirm bot logs show `database ready (migrate=true)` and gateway/shards.
5. **Start the API** — it opens with `migrate=false` and waits for the bot to
   finish the schema/default-data bootstrap.
6. Confirm API logs show `API listening` and OAuth login still works.
7. Sanity check: change a setting in the WebUI; within `config_poll_secs` the
   bot should pick it up (prefix, rules, etc.).

Do **not** run a new API against a DB that has not been migrated by a new bot
(or an equivalent migrate step).

### Adopt basic report templates

The readable-log upgrade updates exact previous default templates once at bot
startup, including copies in guild scopes. Custom templates stay intact.
See [template upgrade behavior](TEMPLATES.md#existing-saved-templates).
Untouched saved `stats` commands gain the `status` prefix alias unless a saved
command already uses that name/alias; customized commands stay intact.
Add the alias manually in the Commands editor where needed. Stats now shows
readable uptime with bold labels and inline-code values.
Publish matching frontend/API artifacts and reload older settings pages.

## 6. Verify

- Discord: prefix or slash command responds.
- WebUI: login, load a guild, save a harmless setting.
- Logs: no repeated `config_version poll failed` or schema-behind errors.
- Optional: `./scripts/smoke-dual-process.sh` on a non-production checkout.

## 7. Rollback

If the upgrade fails:

1. Stop both new processes.
2. Restore the local DB files or remote database backup taken in step 2.
3. Run the **previous** binaries/images (same version that matched that DB).
4. Start bot, then API.

Do not assume new migrations are reversible. Rollback = old code + matching
database backup unless a release note says otherwise.

## Bunny / dual-container notes

Both containers ship together in one app update. The bot container migrates,
and the API container keeps `/health` at 200 and `/ready` at 503 until the
schema matches. Both must keep using the same Bunny Database. Upload
matching `frontend/` to Storage and purge the Pull Zone when the page needs
the new API. CSS-only UI updates do not
require new images. Topology: [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md). Origin-pull
the API with **Override Origin URL** (`OriginUrl`), not OriginMagicContainers.

Select production from a reviewed release: full source SHA, exact bot/API
digests and matching recovery evidence. Source publication creates artifacts;
it does not authorize deployment.

Retain current and rollback images before publishing a candidate. Source image
workflows retain older versions; manual registry cleanup must protect those
inventories. Verify approved digests remain pullable. Rebuilding an old source
SHA can produce a different digest; approve that replacement explicitly rather
than silently substituting it. Tags, including SHA tags, can be retagged.

Before incident rollback or manual repair, wait for any active owner-selected
image update or Storage publication you started to finish; overlapping
changes can leave mixed frontend and API state.

Do not mix a new website with an old API when the page needs new endpoints.
For remote data recovery, restore into a separate database, verify it, and switch
configuration under the incident plan. Do not overwrite production to test a
restore. Start the matching bot then API and restore matching frontend assets
with a successful CDN purge. Rolling back images or frontend files does not
reverse migrations.

## Related

- [ARCHITECTURE.md](ARCHITECTURE.md) — dual-process and `config_version`
- [CONFIGURATION.md](CONFIGURATION.md) — config keys
