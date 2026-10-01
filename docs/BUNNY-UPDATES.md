# Independent Bunny updates

The owner opens **Administration → Updates** at `/administration/index.html` on the existing WebUI host. The page uses the existing Discord login and the API's owner check. The page shows the observed bot, API, and WebUI source SHAs, release publication times, install ages, failures, and available candidates. `Check now` refreshes one component; the API also checks all three every hour without an open browser. Discord `!stats`, `!status`, and `/stats` use cached observations and ship with Admin permission by default.

The page shares the workspace's Light, Dark, and System appearance preference. Each
component has a status badge, release controls, and expandable **Version & technical
details** with full hashes. **Uptime** uses compact durations such as `13m 38s` or
`1d 2h 13m 38s`. **Refresh status** reads the current recorded state;
**Check now** performs release discovery. Status refreshes every 30 seconds when idle
and every two seconds while a component or launcher update is pending. Controls stay
disabled during an action or after a failed status refresh until a fresh read succeeds.
An accepted update request is shown as pending, not as a completed installation.
If the status read that follows the acknowledgement already shows success or failure,
the page confirms only that the request was accepted and leaves that result on the card.
When the installed WebUI differs from this tab, **Reload page** loads the new UI.

## Release and runtime model

Buildkite publishes each component to its own GHCR package: `jmodguard-bot`, `jmodguard-api`, and `jmodguard-webui`. The owner controls default to the `latest` image published by trusted Buildkite builds; an ordinary `main` publication is sufficient, with no Stable or Nightly release build needed for on-demand checking and installation. The updater resolves that moving tag to an exact OCI digest before downloading. Stable and Nightly registry tags remain optional future discovery pointers. The WebUI image contains five static files under `webui/`. Bot/API images contain their baked application, a launcher, and `files.sha256` in the final COPY layer. The launcher downloads only the selected role binary from a verified image digest and checks it against that inventory. There is no second compiled bot/API release package, GitHub-to-Bunny push, or Bunny account API key.

The Bunny dashboard still selects the base bot/API images. Runtime updates change the child executable in that image's container, not Bunny's selected image or the base OS. The Administration page labels the embedded base-image **build Git SHA** separately from the observed running executable Git SHA and its file SHA-256. It does not claim Bunny's selected OCI digest. A child readiness marker, graceful stop, and rollback protect a running switch; after a container restart, the baked executable starts first and the launcher fetches the selected release again. A launcher update also uses a verified role image, then hands off through the fixed shell bootstrap. These downloaded-executable behaviors require a controlled proof on an existing Magic Container before enabling on-demand installs.

On restart, the baked child's successful readiness clears any managed application digest and install time from the prior process. A pending selected release remains queued for reinstallation. The owner controls reject an application or launcher release that is already installed; a baked application with the same source SHA can still be switched on demand because its image digest has not been verified.

For a dashboard-selected bot/API image, the running source SHA can match the checked release without a managed-install digest. Administration reports **no source update (image digest unverified)** in that case. Discord Stats shows ⚠️ for the same state. `first seen`/`Uptime` describe the observed process age; a managed release install time remains unknown until this updater installs one. The status does not claim byte or OCI digest equality for a manually selected image.

The update policy supports Off, Daily, Weekly, and Monthly UTC windows for promoted channels. `latest` allows owner-requested installs only; it cannot be assigned an automatic cadence. A missed hourly poll may run once in its current window. The same failed digest is never retried unattended; the owner can request it again. **Automatic installation is disabled by default** (`JMODGUARD_UNATTENDED_UPDATES` unset or `0`) and remains off while this repository is private. The separate `JMODGUARD_UPDATE_APPLY_ENABLED` and per-container `JMODGUARD_SELF_UPDATE` gates also default off. Do not enable them until the controlled Bunny rollout has passed. Stable/Nightly Git branches and their promotion policy are future work.

An owner install request requires a successful release check for the currently selected channel within the last two hours. If the last check failed or expired, use **Check now** before installing; this applies to the service application, launcher, and WebUI controls.

## Schema upgrades and recovery

Independent application and launcher switches require the candidate, current
rollback binary, and other service to support the same core schema. Published
schema labels follow the selected source's migrations; launcher observations use
the running release's declared range (or the baked build's schema). A schema
upgrade requires a coordinated deployment of matching bot/API images through
Bunny, following [the upgrade guide](UPGRADE.md#5-rollout-order).

Builds through `a5e02070e9e602cecafd1f0ccbc3d8c5b694e31d` incorrectly advertised
core schema 15 even after migration 0016 was added. Updating the bot alone could
therefore migrate to 16 while the old API and launcher kept reporting 15. Later
checks show **candidate cannot open current database schema** and disable both
application and launcher controls. Reloading the WebUI cannot repair this state. Republishing those historical service
sources keeps their true schema labels but omits updater protocol eligibility, so
their old launcher cannot be installed onto a repaired deployment.

For an affected deployment, select matching bot/API base images supporting schema
16 in Bunny's dashboard, including the corrected launcher and publication metadata
once available. Stop the old API, deploy/start the matching bot image, then start
the matching API image and verify readiness before using **Check now**. Replacing
base images also replaces the old baked binaries used on restart. Updating only
the WebUI or restarting an old base image does not fix compatibility. Do not lower
`user_version`, edit recorded compatibility ranges, or bypass the install checks;
old binaries reject newer schemas on startup. Image rollback requires the matching
pre-upgrade database backups described in the upgrade guide.

## Storage Zone publication

The API needs `BUNNY_STORAGE_ENDPOINT` (regional Storage hostname, default `storage.bunnycdn.com`), `BUNNY_STORAGE_ZONE`, and that **zone's Storage password** as `BUNNY_STORAGE_PASSWORD`. The password can read/write/delete within the entire zone. Keep it in Bunny's container environment, not source or browser code. The updater restricts operations to `index.html`, `administration/index.html`, and exact filenames under `assets/releases/<40-character source SHA>/`. It never deletes a directory or a legacy root file. A missing password disables WebUI installation while discovery remains available.

The publisher uploads and reads back immutable JS/CSS first, then Administration HTML, then root HTML. It records the new SHA only after readback of root HTML. It retains the active managed assets plus one previous managed release, then deletes only the three known files of an older managed release. A manual overwrite of any managed release file is shown as drift on the next successful check. Older manually uploaded `/app.js` and `/style.css` are left in place, and their release SHA remains unknown until an explicit managed install.

The API renews its WebUI check/install lease while Storage work is active so another worker cannot start a competing publish during a slow but healthy request. After an API interruption, the next successful check adopts a fully verified WebUI switch or reports an incomplete queued request as interrupted, including a partially switched pair of HTML files; the owner may then check Storage and request the digest again. An image digest change at the same source SHA does not prove the static file bytes changed, but this updater refuses that repack because the cacheable asset URLs would be reused. It retains source SHA reservations after managed assets are pruned; an older reservation whose digest is no longer known cannot be reused automatically or by an owner request.

For prompt production cutover, configure a Bunny Edge Rule with zero or short cache time for mutable `/`, `/index.html`, and `/administration/index.html` once. Keep immutable `/assets/releases/<sha>/...` cacheable. A one-time purge of already cached HTML may be necessary during transition. Routine managed releases do not use the account-level purge API. The active-plus-rollback bound applies to managed Storage objects; CDN cache copies expire separately. The existing `scripts/publish-frontend-bunny.sh` is a legacy manual publisher and requires an account API key to purge. It is not used by this update path.

## Rollout after the PR

1. Confirm the new WebUI GHCR package and bot/API release images can be read anonymously at `latest` and at each image's own full source-SHA tag. Check each image's source, component, publication-time, and compatibility labels. A trusted `main` publication is sufficient; Buildkite does not publish from PR builds.
2. Configure the three Storage values on the **existing API container**, and configure short caching for the mutable HTML paths. Preserve the existing API origin routing and session cookies.
3. In Bunny's dashboard, select one new launcher-equipped image by full source SHA on one existing service at a time. Leave all update gates off. Confirm the baked child starts, its observed SHA appears, and the other service remains healthy. Retain the prior dashboard image for manual recovery.
4. Prove an on-demand same-version child switch and launcher handoff on the existing service with the relevant gates enabled. Test the WebUI publish and readback, then confirm the owner page and Discord status. If Bunny's sandbox rejects execution from ephemeral storage, leave runtime self-update off and continue with manual image changes. No disposable Bunny app is required.
5. Keep `JMODGUARD_UNATTENDED_UPDATES=0` during this private-repository rollout. Define public-repository release promotion separately before enabling any scheduled installation.

The PR validates local behavior; it does not change live Bunny settings. Bunny documents [dashboard image updates](https://bunny.net/docs/magic-containers/update), [container sandboxing](https://bunny.net/docs/magic-containers/sandbox), [restart behavior](https://bunny.net/docs/magic-containers/restart-policy), the [Storage HTTP API](https://bunny.net/docs/storage/http), and [CDN cache purging](https://bunny.net/docs/cdn/purge-cache). The [OCI distribution specification](https://github.com/opencontainers/distribution-spec/blob/main/spec.md) defines the image manifest/blob reads. These references establish the APIs, not proof of this installation's current Bunny configuration.
