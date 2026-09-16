# Private ops example (optional)

This example is an **optional extra lock** for operators who want GitHub Actions
to publish Storage files without putting Bunny passwords in the public
jModGuard repo. It is **not required** to install from public source.
[INSTALL.md](../../INSTALL.md) and [DEPLOY-BUNNY.md](../../DEPLOY-BUNNY.md)
cover a Bunny install with a manual Storage upload or Buildkite frontend
artifacts.

If you do keep a private ops repository, production releases can be selected
there instead of tracking source `main`. Source publishes artifacts; an operator
reviews the source, compatible bot/API image digests, migrations and recovery
evidence before promoting a release. Keep production IDs, secrets and actual
approval records in ops.

## Install this frontend example

1. Copy `publish-frontend.yml` into your ops `.github/workflows/` and
   `frontend-release.json` into its root. It starts disabled.
2. Copy these three files from this source revision into ops `scripts/`:
   `publish-frontend-bunny.sh`, `frontend_publication.py`, and
   `frontend_release.py`. Review tooling updates as ops changes. Never execute
   scripts from a newly selected source checkout with production credentials.
3. Configure the Storage/purge secrets listed in DEPLOY-BUNNY and, while source
   is private, `JMODGUARD_CHECKOUT_TOKEN` with source `contents:read` access.
4. Set repository variables `JMODGUARD_API_APP_ID` and
   `JMODGUARD_READINESS_URL` (`https://YOUR-CDN-HOSTNAME/api/me`). The API container
   template image name must be `jmodguard-api`. Use the same concurrency group
   `deploy-api-<app-id>` for API deployment workflows in this ops repo. Manual
   provider changes and workflows in other repositories cannot share that lock;
   pause publication before such changes.
5. Protect ops `main` with your release-review policy. Before enabling, complete
   the [recovery drill](https://github.com/jatmn/jModGuard/blob/main/docs/BACKUP-RESTORE.md),
   retain the backup and running/rollback images, and record actual evidence in
   your private runbook. The example does not perform or certify that drill.
6. Review and commit `frontend-release.json` on ops `main` with `enabled: true`,
   `source_sha` equal to the full lowercase source commit, and `api_digest` equal
   to the compatible API's exact `sha256:<64 lowercase hex>` manifest digest.
   The release review must establish compatibility from complete API inputs or
   tested API behavior; a SHA/tag alone is not proof. Deploy the required bot
   migrations and API first through your container runbook.

This is a **frontend** example, not a bot/database deployment orchestrator or a
full production-release/recovery attestation system. Your ops system should bind
bot and API digests and recovery evidence to the same approved source revision.
It may adapt this example to read that single release record instead of keeping
a separate frontend selection.

## Reconcile and recover

A push changing the release file on ops `main` triggers publication. Scheduled
and manual runs retry the currently committed selection; neither follows source
`main` nor accepts a source override. Dispatch from ops `main`.

The checker compares live Storage bytes and a mutable publication receipt.
Changed JS/HTML also requires the selected API digest, active container status,
and the anonymous `/api/me` readiness response. `/health` is only liveness.
CSS-only changes can publish without an API rollout once previous JS/HTML
publication is established. Unavailable or unready APIs fail the run; a later
run can retry. The check verifies the approved image is configured and the API
is responding, not per-request backend image identity during a rolling update;
wait for rollout completion before approving publication.

The publisher requires a valid Pull Zone and purge key before touching Storage.
It backs up all objects before upload, rejects incomplete downloads, restores
all potentially modified assets on upload failure, and completes its receipt
only after a successful purge. This is not an atomic CDN rollout: readers may
observe mixed files during changes, and a failed upload rollback cannot evict
already-cached attempted assets. A pending receipt keeps the next retry active.
It also retains whether JS/HTML was involved, including repair of drifted files,
so retries repeat API checks even after origin bytes match the selected source.
Older pending receipts without that evidence also require API checks; known
CSS-only retries remain independent. Successful completion clears that intent.

To pause, set `enabled: false`, disable the workflow and wait for in-flight runs
before manual intervention. Disabling future runs does not undo a running job
or restore old assets. To roll back, review and commit the known-good source SHA
and compatible API digest, then publish and confirm purge. Restoring frontend
files does not reverse database migrations. Manual runs re-upload even matching
assets; schedules skip only when bytes and the completed receipt match.
