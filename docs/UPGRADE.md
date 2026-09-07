# Upgrade guide

How to move an existing jModGuard install to a newer version (git tag or release).

For a **first** install, use [SETUP.md](SETUP.md). For a Bunny
deploy, see [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md). If you maintain your own
registry/CDN/secrets pipeline, follow this guide for product steps, then apply
your operator runbook for promote and rollout.

## 0. Upgrading from the single `jmodguard` binary

Older builds shipped one process: `target/release/jmodguard`
and typically one systemd unit (`jmodguard.service`).
This release **removes that entrypoint**. You must run two binaries:

| Old | New |
|-----|-----|
| `jmodguard` | `jmodguard-bot` (Discord gateway, migrations) **and** `jmodguard-api` (WebUI) |

1. Stop the old `jmodguard` process/service (there is no “old API” yet).
2. Build both new binaries (`jmodguard-bot` with `--no-default-features`,
   `jmodguard-api` with the default `web` feature). See [SETUP.md](SETUP.md).
3. Keep the same working directory and `config.toml` / `jmodguard.db`.
4. Replace the one systemd unit with **two** (`jmodguard-bot` then `jmodguard-api`).
   Logs are now `logs/jmodguard-bot.log.*` and `logs/jmodguard-api.log.*`
   instead of a single `jmodguard.log`.
5. Fill in Discord OAuth client id/secret. The API process **refuses to start**
   without them; the old monolith could still run Discord without WebUI login.
6. Start the bot first (it migrates and completes default-data bootstrap). The
   API waits about 30 seconds locally (about six minutes against a remote
   database) for that work, then exits if the bot never completes it.

Then continue with backup / rollout below as for any later upgrade.

## 1. Pick a version

Prefer a **git tag** or release (`vX.Y.Z`) over an arbitrary `main` commit.
Read the release notes for:

- New or renamed `config.toml` / env keys
- Database migrations
- Breaking WebUI or Discord permission changes

## 2. Backup

Both `jmodguard-bot` and `jmodguard-api` can hold the shared DB open.

**Cold copy (safest):**

1. Stop **both** processes.
2. Copy `jmodguard.db` plus `jmodguard.db-wal` / `jmodguard.db-shm` if present.
3. Keep the previous binaries or container image tags until you verify the upgrade.

**Hot copy:** use SQLite online backup or
`sqlite3 jmodguard.db "VACUUM INTO 'jmodguard-backup.db'"` — do not file-copy
the `.db` while either process is writing. See [CONFIGURATION.md](CONFIGURATION.md).

For Bunny Database, the local file-copy procedure does not apply. An
operator-controlled recovery method must pass a live drill; `bsql .dump` alone is
insufficient for the jModGuard schema. Do not perform a production upgrade
until the acceptance drill in [BACKUP-RESTORE.md](BACKUP-RESTORE.md) passes.

## 3. Build or pull the new artifacts

From the tagged source:

```bash
git fetch --tags
git checkout vX.Y.Z   # or your chosen tag

cargo build --release --locked --bin jmodguard-bot --no-default-features
cargo build --release --locked --bin jmodguard-api
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

1. Stop the old API, then the old bot (or stop both).
2. Install the new binaries/images (and SPA assets if separate).
3. **Start the bot first** — it runs migrations (`migrate=true`).
   Migration 0003 retires unsafe global log-channel defaults: Discord channel
   IDs are server-specific, so old `guild_id=0` mappings are moved to
   `legacy_global_log_channels` and no longer route events. Before or after
   the rollout, inspect that table and configure the required channel for each
   server in the WebUI or with `!logchannel set <event> <#channel>`.
   On migrate-open the bot also **migrates** legacy `guild_settings` rows:
   - legacy full blobs are normalized against their parent layer. A blob that
     differs from its parent is retained because old data cannot distinguish an
     automatic `ensure_guild` seed from an intentional operator override;
   - the **global** row is migrated first (parent = configured terminal defaults), then
     guild rows against the healed global;
   - other full/effective blobs are rewritten as parent-relative partial
     overrides (fields equal to global/defaults are stripped);
   - unparseable `guild_settings` JSON is cleared (guild row deleted; global
     rewritten to `{}`) so inheritance resumes instead of leaving reads broken;
   - `config_version` is bumped when anything changes so a running API can
     invalidate.
   **Legacy ambiguity:** an old complete default blob may be an untouched
   `ensure_guild` seed or an intentional operator override. The upgrade keeps
   any field that differs from its parent rather than risk deleting a real
   setting. New writers store parent-relative partial overrides, so this
   ambiguity does not recur for post-upgrade writes.
   Start the bot once after this upgrade before relying on global settings
   inheritance. Runtime reads ignore corrupt rows so inheritance remains
   available, but only a bot migrate-open durably heals them. The bot also writes durable
   `meta.settings_terminal_defaults` from `bot.default_prefix` /
   the transitional `bot.retention_days` field on every migrate-open (routine chat
   collection/purge and its UI control are retired); both processes read that meta
   row on each settings resolve (not cached at open). The API **refuses to
   open** if that meta row is missing — start the bot first (do not let the API
   seed terminal defaults from its own config). Legacy settings heal is skipped
   on open when `guild_settings` checksum is unchanged (backup restores still
   re-run heal when row data differs).
4. Confirm bot logs show `database ready (migrate=true)` and gateway/shards.
5. **Start the API** — it opens with `migrate=false` and waits for the bot to
   finish the schema/default-data bootstrap.
6. Confirm API logs show `API listening` and OAuth login still works.
7. Sanity check: change a setting in the WebUI; within `config_poll_secs` the
   bot should pick it up (prefix, rules, etc.).

Do **not** run a new API against a DB that has not been migrated by a new bot
(or an equivalent migrate step).

### Adopt basic report templates

This logging wave preserves saved templates and commands, including old seeded values.
Edit the `message_edit` and `message_delete` templates using the
[provided basic report values](TEMPLATES.md#existing-saved-templates) to display message
references and supplied attachment counts. Resetting only a guild template inherits its
saved global value. Existing custom templates are never overwritten automatically.
The saved `stats` command description may still mention member/message counts; update
its description in Commands if needed. The command itself now reports moderation counts.
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

Same order: migrate with the bot app, then roll the API app. Both must keep
using the same Bunny Database. Upload matching `frontend/` to Storage and purge
the Pull Zone when the page needs the new API. CSS-only UI updates do not
require new images. Topology: [DEPLOY-BUNNY.md](DEPLOY-BUNNY.md). Origin-pull
the API with **OriginUrl**, not OriginMagicContainers.

Select production from a reviewed ops release: full source SHA, exact bot/API
digests and matching recovery evidence. Source publication creates artifacts;
it does not authorize deployment. Scheduled/manual reconciliation must retry the
approved selection, including after a failed rollout. The frontend example and
its installation requirements are in [private ops](examples/private-ops/README.md).

Retain current and rollback images before publishing a candidate. Source image
workflows retain older versions; manual registry cleanup must protect those
inventories. Verify approved digests remain pullable. Rebuilding an old source
SHA can produce a different digest; approve that replacement explicitly rather
than silently substituting it. Tags, including SHA tags, can be retagged.

Pause reconciliation and wait for in-flight runs before incident changes. For
remote data recovery, restore into a separate database, verify it, and switch
configuration under the incident plan. Do not overwrite production to test a
restore. Start the matching bot then API and restore matching frontend assets
with a successful CDN purge. Reverting an ops release does not reverse migrations.

## Related

- [ARCHITECTURE.md](ARCHITECTURE.md) — dual-process and `config_version`
- [CONFIGURATION.md](CONFIGURATION.md) — config keys
- [BACKUP-RESTORE.md](BACKUP-RESTORE.md) — provider backup status and restore gate
