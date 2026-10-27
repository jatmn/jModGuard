# Bunny application and WebUI updates

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

GitHub Actions publishes each component to its own GHCR package: `jmodguard-bot`, `jmodguard-api`, and `jmodguard-webui`. The owner controls default to the `latest` image published by trusted GitHub Actions builds; an ordinary `main` publication is sufficient, with no Stable or Nightly release build needed for on-demand checking and installation. The updater resolves that moving tag to an exact OCI digest before downloading. Stable and Nightly registry tags remain optional future discovery pointers. The WebUI image contains five static files under `webui/`. Bot/API images contain their baked application, a launcher, and `files.sha256` in the final COPY layer. The launcher downloads only the selected role binary from a verified image digest and checks it against that inventory. There is no second compiled bot/API release package, GitHub-to-Bunny push, or Bunny account API key.

Bunny selects the base bot/API images. Runtime updates replace executables inside those containers; they do not change the selected image or base OS. The page distinguishes the base-image build SHA, running executable SHA, and verified file checksum. On container restart the stable launcher reads durable selections **before** starting an application, fetches the digest-pinned installed release again if ephemeral files were lost, and preserves its installation identity/time. It never interprets a database/network error as permission to start an old baked application.

Launcher updates use the same verified role images and the fixed shell bootstrap. A selected launcher is restored before normal application startup; the stable baked controller can recover applications if a replacement launcher fails. The one-time baseline must include this controller **and** API maintenance mode. Downloaded execution still needs a controlled proof in the actual Magic Container before enabling the update gates.

The launcher restarts ordinary crashed children on its 5-second poll, with
successive delays of 5, 10, 20, 40 and at most 60 seconds. A child that stays
up for five minutes resets that backoff. Lost-lease exit code 1 still restarts
with the initial delay. A fatal startup Discord 401 or gateway authentication close 4004 releases the
bot's active lease and exits with code 78
(`DISCORD_TOKEN_REJECTED_EXIT_CODE`, shared by bot and launcher). The launcher
holds that bot without immediate restart or rollback and logs once per hold entry:
`discord token rejected; bot held, fix token and restart container`.
This release requires launcher revision 7 or newer, which handles exit 78 before
checking the selected child's readiness deadline. Older launchers must install the
required launcher before starting either a pending selection or a restored install;
in-process idling cannot protect an unready selected child from their timeout.
The bot never writes the Discord-ready marker for a rejected token.
Fix the token and restart the container to resume immediately. As a fallback,
the launcher retries once after 30 minutes, doubling subsequent rejected-token
intervals up to six hours, with one warning per attempt. A container restart
during that hold still starts the same selected candidate when the selection
is unchanged, instead of the previously installed bot. The marker is removed
when that candidate becomes ready, when the selection changes, or when a
coordinated upgrade starts. A ready child, or one that stays up for five
minutes, resets the interval. While held, the launcher serves `/health` as 200 and `/ready` as
503 with the reason `discord token rejected` on the bot's configured health port
(8081 in the image); the platform uses only `/health` for liveness. Running
slash synchronization, identity lookup and enrollment workers still stop their
own retries on 401 without adding a process exit.

### Application and launcher updates

**Update** installs the selected bot/API application and automatically installs its
required launcher first. The launcher supervises startup, update sequencing and
recovery; it is a separate executable. A different source SHA is not a dependency.
Administration shows whether the running launcher already supports the release,
whether a launcher update is required first, or whether a newer launcher is optional.
Unknown or stale capability is reported explicitly; check the running baseline.
The expandable **Advanced launcher controls** are only for a deliberate optional
launcher update. They restart the service but do not install the bundled application.
A schema-changing application's launcher can be installed independently when its
control protocol and base ABI are compatible; the application's schema gate does
not incorrectly disable that launcher step.

Each published service image declares `io.jmodguard.launcher-revision` and
`io.jmodguard.min-launcher-revision`. The publisher reads `LAUNCHER_REVISION` and
`REQUIRED_LAUNCHER_REVISION` from the **selected source**, not the publishing checkout.
Maintainers increment the launcher revision for launcher behavior/fix changes, and
raise the required minimum when a release depends on that behavior (including a
required updater safety fix). Routine application-only changes keep the minimum.
The bundled launcher must satisfy the minimum. The running launcher checks its own
compiled revision before every managed application start, including cold restore
and rollback; a historical installed-launcher row is never proof of capability.
The replacement verifies its compiled revision against the pinned release before
recording activation. A failed prerequisite leaves the application uninstalled;
retry explicitly, or use paired recovery after a schema transition.

Install the **launcher-aware protocol-3 baseline** once in both Bunny containers.
If adopting the paired-update work and this follow-up together, wait for both to
merge/publish and perform that single baseline rollout. Protocol-2 launchers cannot
coordinate these requirements and reject protocol-3 releases; upgrading only the
WebUI does not change that. New controllers can restore existing protocol-2
application selections, but new upgrade admissions require the launcher-aware
contract. Shell handoff protocol 1 and the durable control tables/phase names remain
unchanged. Per-role `component_baked_compatibility_*_v1` meta records bind the
baked application's core/guild schemas to its base SHA. Only the actual baked
controller initializes them; downloaded launchers retain them for cancellation,
restore and truthful observations even when compiled for a newer app schema.
Future migrations must retain these records too. This does not make an incompatible future control protocol or base OS
replaceable through an application download.

For a dashboard-selected bot/API image, the running source SHA can match the checked release without a managed-install digest. Administration reports **no source update (image digest unverified)** in that case. Discord Stats shows ⚠️ for the same state. `first seen`/`Uptime` describe the observed process age; a managed release install time remains unknown until this updater installs one. The status does not claim byte or OCI digest equality for a manually selected image.

The API records each component's configured channel digest about once an hour through the bot writer, with no browser session. The first tick runs when the API becomes ready. That record does not install by itself. The update policy supports Off, Daily, Weekly, and Monthly UTC windows for promoted channels. `latest` allows owner-requested installs only; it cannot be assigned an automatic cadence, so its hourly check stays record-only. Stable and nightly rows with a daily, weekly, or monthly cadence still run the existing automatic install on that same tick. A missed window may run once. The same failed digest is never retried unattended; the owner can request it again. **Automatic installation is disabled by default** (`JMODGUARD_UNATTENDED_UPDATES` unset or `0`) and remains off while this repository is private. The separate `JMODGUARD_UPDATE_APPLY_ENABLED` and per-container `JMODGUARD_SELF_UPDATE` gates also default off. Do not enable them until the controlled Bunny rollout has passed. Stable/Nightly Git branches and their promotion policy are future work.

An owner install request requires a successful release check for the currently selected channel within the last two hours. If the last check failed or expired, use **Check now** before installing; this applies to the service application, launcher, and WebUI controls.

## Image signatures

GitHub Actions builds and pushes each component image to a staging reference,
captures the build output digest, verifies the image, signs that digest with a
pinned cosign key pair, and only then promotes the immutable source-SHA tag,
any planned version tag, and `latest`, `stable`, or `nightly`. Each signature is stored in the registry as a `sha256-<digest>.sig`
artifact using the cosign simple-signing envelope. Signatures carry an
`io.jmodguard.not-after` annotation set to 366 days after the image
`io.jmodguard.published-at` label. Historical dashboard rollback images carry only
that publication timestamp for signing, not updater compatibility labels. The matching
cosign public key is committed at `deploy/update-signing.pub`. The private signing key is not stored in this
repository; publication fails closed when that Actions secret is missing.

The launcher verifies every digest before it downloads image layers or stages an
executable. Unsigned, cryptographically invalid, expired, and untrusted-key
signatures are refused with a permanent error. Registry outages and other
transient fetch failures are retried and are not recorded as signature rejections.
The same verification runs during boot repair when ephemeral staging files are
missing. If the installed release cannot be re-downloaded or verified, the
launcher starts the baked application only when the database core and observed
guild schemas still fit that binary; otherwise moderation stays stopped until
owner recovery. Boot repair ignores `JMODGUARD_UNATTENDED_UPDATES`. When the
running digest again matches the installed release, the durable
`update_boot_mismatch_v1` record for that component is cleared.

After a manual in-app install, select that same published image as the Bunny
container image so a pod replacement already contains the verified binary. The bot
does not publish images.

## Schema upgrades and recovery

Independent application switches require the current application, candidate, and counterpart to share compatible **core and guild** schemas. Published runtime ranges remain exact; separate migration-origin labels describe which older stores the new bot can upgrade. Schema changes never run unattended.

The private API-to-bot control link supports either update order for these compatible
switches. An updated bot accepts correctly signed previous API frames that omit
both direction and route; frames that include either field must include both with
the control-link values. An updated API continues to send frames that a previous
bot can read, because that bot ignores the added fields after verifying the signed
payload. Container-read requests remain separate from control requests.

For a core or guild schema change, use **Update bot and API together / recovery** in Administration. Paired bot and API upgrades are refused until signed, launcher-routed upgrades land (issue 188). Single-container installs still work. Schema changes ship in new images. This opens `/api/service-upgrade/recovery` through the existing `/api*` proxy on the public WebUI host and uses the existing Discord owner login. Check a matching pair, inspect its source SHA and two pinned image digests, and confirm the interruption. Ordinary main pushes publish services independently: an API-only or bot-only fix may have no counterpart image at that SHA. Shared/schema inputs select both services; an owner-dispatched publication of an exact source builds all eligible components. Having a matching image pair permits Check, but does not enable the currently refused paired Apply. The API resolves the counterpart from the same full source-SHA tag. The channel and checked candidate for both services are stored in one step, and that check holds both update leases so it cannot overlap Administration discovery. If either image is missing, neither selection changes and nothing is stopped.

The durable operation first hands off any required launcher before that role acknowledges preparation, then prepares and verifies both application executables, stops and reaps the old API, stops and reaps the old bot, records an irreversible migration boundary, starts the new bot, then starts the matching API only after bot readiness. Completion requires both readiness markers. Closing the browser does not cancel it. A restarted controller invalidates its local staging/stop acknowledgements and resumes the operation; heartbeat expiry is never evidence of shutdown. Run **one container per role** against the shared core and guild stores; overlapping replicas or other writers are not supported by this protocol.

If preparation or startup fails, both applications stop and the API's restricted maintenance mode retains the recovery page and `/health`. The page opens only the stable update-control tables, not application or guild state. Sign in again if the API restarted. Before the irreversible boundary, **Cancel before migration** resumes the installed pair. After that boundary, use **Retry selected pair** or check and confirm a compatible newer pair. Recovery never downgrades core/guild stores or starts pre-migration rollback binaries. A failed selected digest can be retried without rediscovering a moving tag; a different pair requires a fresh check. A stopped application's stale heartbeat does not prevent an explicitly confirmed recovery, because both controllers must still prepare and acknowledge the new operation.

After the baseline rollout, supported application/schema upgrades and container restarts use this flow without manually reloading Bunny images. The boundary remains the installed Linux/amd64 base ABI, stable control protocol, fixed bootstrap, and working configuration/credentials. This cannot replace Debian/system libraries, repair unavailable database credentials or external stores, or change platform settings. Such changes still require operator deployment. No Bunny management credential is stored.

Future migrations must preserve the `component_updates` column contract and the versioned `meta.component_upgrade_v1` control record. Keep update-control protocol 3, launcher capability/minimum, and migration-origin labels truthful; changing these contracts requires an explicitly compatible baseline. Ordinary `Db` startup retains its strict family/version checks.

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

1. Confirm the new WebUI GHCR package and bot/API release images can be read anonymously at `latest` and at each image's own full source-SHA tag. Check each image's source, component, publication-time, and compatibility labels. A trusted `main` publication is sufficient; GitHub Actions does not publish from PR builds.
2. Configure the three Storage values on the **existing API container**, and configure short caching for the mutable HTML paths. Preserve the existing API origin routing and session cookies.
3. For the one-time baseline, stop the old API and bot, select matching launcher-aware protocol-3 images by full source SHA, start the bot and verify readiness, then start the API. Leave update gates off while verifying both observations and `/api/service-upgrade/recovery`. Retain matching backups; an old image alone cannot undo a schema migration.
4. Prove an on-demand child switch and launcher handoff with the relevant gates enabled, then a paired update and container restart with cleared ephemeral downloads. Verify ordering, real readiness, and owner recovery after a failed candidate. Test WebUI publication/readback and Discord status. If Bunny's sandbox rejects execution from ephemeral storage, leave runtime self-update off and continue with manual image changes. No disposable Bunny app is required.
5. Keep `JMODGUARD_UNATTENDED_UPDATES=0` during this private-repository rollout. Define public-repository release promotion separately before enabling any scheduled installation.

The PR validates local behavior; it does not change live Bunny settings. Bunny documents [dashboard image updates](https://bunny.net/docs/magic-containers/update), [container sandboxing](https://bunny.net/docs/magic-containers/sandbox), [restart behavior](https://bunny.net/docs/magic-containers/restart-policy), the [Storage HTTP API](https://bunny.net/docs/storage/http), and [CDN cache purging](https://bunny.net/docs/cdn/purge-cache). The [OCI distribution specification](https://github.com/opencontainers/distribution-spec/blob/main/spec.md) defines the image manifest/blob reads. These references establish the APIs, not proof of this installation's current Bunny configuration.
