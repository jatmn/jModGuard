# Buildkite CI and image publishing

All CI and image publication for this repository run on the self-hosted
Buildkite `linux` queue. There are no GitHub Actions workflows or GHA runner
selection/fallback jobs. Keep **Update commit statuses** enabled in the
pipeline's GitHub settings: the GitHub commit/PR check links to the Buildkite
tests and image jobs. The separate private-ops frontend example is an optional
operator lock; it is not executed by this source repo and is not required to
install.

## Pipeline and agents

Keep the pipeline's initial step as:

```yaml
steps:
  - label: ":pipeline:"
    command: buildkite-agent pipeline upload .buildkite/pipeline.yml
    agents:
      queue: linux
```

The agents need Linux x86-64, Git, Python 3, Node, Rust 1.96.0 with Clippy and
rustfmt, `cargo-deny` 0.20.x, `cargo-audit` 0.22.x, Docker with the Buildx
plugin/default Docker builder, and a Buildkite agent supporting `secret get`,
`redactor add` and `meta-data`. The `test` job runs `bash scripts/ci-preflight.sh`
after GitHub git-credential setup. If `cargo-deny` or `cargo-audit` is missing,
that script installs the pinned versions with `cargo install --locked`.
Scheduled builds set `JMODGUARD_AUDIT_FAIL_CLOSED=1` so `cargo audit` fails the
job; pull requests and ordinary main pushes still run audit but do not fail on a
reported CVE advisories. `cargo audit` operational failures (for example
advisory-database fetch/load errors) still fail the test job even on the
fail-open path. The existing clone credential must also fetch tags and full commits from
this repository. Publishing uses the existing Docker daemon's BuildKit cache and
inline cache metadata in the published image; it does not depend on the Actions
cache service. Do not add `.github/workflows`; there are no GitHub-hosted CI
minutes for this repository.

The remote database job runs `remote_database_smoke` and `guild_store_smoke` from
`jmodguard-integration-tests`, plus the root library's
`bunny_evidence::tests::remote_bunny_journal_recovers_commits`. Each uses its own empty
disposable libSQL container; the Bunny test also starts a local HTTP storage fixture.
They run sequentially with loopback-only ephemeral ports and cleanup on exit. Never point
these ignored tests at an existing installation or share one database between the binaries.

Both test jobs must pass before preparation and publication. Prepare and publish
steps are scheduled for webhook main pushes, webhook tag builds, and UI/API
builds that set `JMODGUARD_PUBLISH_SOURCE`. Manual publication still requires
`main` and no tag in the publisher: an invalid UI/API source fails after tests
instead of looking like a successful skip. Scheduled or plain UI test
builds on `main` do not enqueue those jobs. Bot and API
publication each have a concurrency limit of one across this pipeline's builds.
There is one publisher per component, including migration changes. Keep those
concurrency group names consistent if another pipeline publishes the same packages.

Main builds compare each package's last published source revision to the new
commit across the Dockerfile, Cargo/toolchain files, shared source trees and
publishing configuration. This includes all intervening commits, not just the
last commit in a push. Frontend/docs-only changes skip image compilation when
the published source can be verified. That skip inspects public GHCR history
before fetching `GHCR_TOKEN` or logging in, so missing push credentials cannot
fail a docs-only main build. Missing image labels, unavailable Git
history or registry inspection errors cause a rebuild. Bot and API images use
the shared source trees, so shared Rust/migration changes can build both images.
Main SHA publication does not publish the conductor image or an installable
manifest. Scheduled Nightly and Stable tag/manual builds publish bot, API,
conductor and a complete signed `JMODGUARD-RELEASE-1` manifest. Nightly tags are
unique and must not move `latest`. Incomplete publications stay unavailable.
Skipping also requires `latest` and its labeled source-SHA tag to contain the
same manifest. If a previous push updated only one tag, a retry republishes
the image to repair the incomplete publication.
Explicit job retries always republish their pinned source, even if a newer
build has since moved `latest`. Input-based skipping applies to fresh jobs.

## Registry credentials: required before merging the migration

A working repository SSH deploy key does **not** grant GHCR push access.
GitHub's automatic Actions `GITHUB_TOKEN` is not supplied to self-hosted
Buildkite jobs. There is no Pipeline Settings → Environment page. Official
Buildkite docs put non-secret values in a top-level YAML `env` attribute
([environment variables](https://buildkite.com/docs/pipelines/configure/environment-variables))
and tokens in cluster Secrets
([Buildkite secrets](https://buildkite.com/docs/pipelines/security/secrets/buildkite-secrets)).

| Value | Where and purpose |
|---|---|
| `GHCR_USERNAME` | Non-secret. Set in this repo's `.buildkite/pipeline.yml` `env:` (already `jatmn`). The other official place is **Pipeline settings → Steps** (YAML editor) with a top-level `env:` before `steps`. |
| `GHCR_TOKEN` | Not under the pipeline. Top nav **Agents →** your cluster → **Secrets → New Secret**, key exactly `GHCR_TOKEN`. Then **Access** and restrict to this pipeline's `main` / release-tag builds. The publisher calls `buildkite-agent secret get GHCR_TOKEN`. Must push `jmodguard-bot`, `jmodguard-api` and `jmodguard-conductor`. |
| `GITHUB_TOKEN` | Optional separate read credential for Cargo's private Git dependency inside Docker and for Release Please. If absent, `GHCR_TOKEN` is used for Docker and must also read this private repository. Release Please is skipped when no token is available. Do not set this to GitHub Actions' automatic token. |
| `JMODGUARD_RELEASE_KEY_ID` | Non-secret signing key id for complete Stable/Nightly manifests. |
| `JMODGUARD_RELEASE_SIGNING_KEY` | 32-byte hex HMAC secret. Store as a Buildkite secret, never in git. |

For personal access tokens, GitHub's container registry requires a **classic
PAT** with `write:packages` and access to the existing packages. While this
repository is private, give the credential used for the Docker Git fetch the
required private-repository access (`repo` for a classic PAT). Do not add
`delete:packages`; this pipeline retains older images. A separate read-only
source token can keep registry and source permissions separate.

Use Buildkite's Secrets UI to store the token, never source YAML, Git, a manual
build's environment text box, or chat. Restrict secret access to the publishing
pipeline and trusted main/release-tag builds. Restrict who can create builds
at arbitrary commits and who can push release tags. Do not expose publishing
credentials to PR/fork jobs or agents executing untrusted code; use isolated
agents and server-side secret policies where contributors are untrusted.
The in-repository job condition is an execution guard, not a replacement for
the secret policy. An agent hook must impose equivalent restrictions if it
supplies credentials instead of Buildkite Secrets.

Each publisher fetches the secret only after validating the build and source
plan **and** deciding a rebuild is required, adds it to Buildkite's log redactor,
logs into GHCR using stdin, and uses
a temporary Docker configuration removed on normal completion/failure. The
source credential reaches Docker through a BuildKit secret, not a build arg.
An interrupted/killed agent may need its normal workspace cleanup before reuse.

Existing package visibility is preserved. Ensure the image packages are public
for unauthenticated Bunny pulls, or configure the private registry credentials
in your deployment. Publishing never grants public access or deploys containers.

## Main, tags and manual builds

- Keep branch/PR builds enabled as appropriate for CI. PRs and UI/API builds
  without `JMODGUARD_PUBLISH_SOURCE` run tests only. Main webhook
  pushes can publish changed bot/API images. A leftover `JMODGUARD_PUBLISH_SOURCE` on a
  webhook build is ignored so automatic publication still runs.
- Scheduled builds on `main` publish Nightly images and a complete signed
  manifest. Nightly IDs never become Release Please versions. The manifest job
  writes `dist/releases/index.json` and per-release JSON on the agent only;
  operators sync that tree to the HTTPS origin configured as
  `JMODGUARD_CONDUCTOR_RELEASE_FEED` (see [releases](RELEASES.md#publishing-the-discovery-feed)).
- For automatic tag publication, enable **Build tags** in the pipeline's GitHub
  settings and ensure branch filters allow your release tags. Use trusted tags
  containing this Buildkite pipeline. This replaces GitHub's release-published
  trigger; creating a GitHub Release alone does not publish images again.
- To publish a release tag or an older SHA manually, choose **New Build**, use
  current `main` for the pipeline code, and set the non-secret environment value
  `JMODGUARD_PUBLISH_SOURCE=refs/tags/v1.2.3` or a full lowercase 40-character SHA.
  This also works for source revisions predating the publishing scripts. Both
  components use the one resolved SHA recorded by the preparation job. Tests
  validate the current pipeline checkout; the image builds compile the selected
  source. A plain branch name such as `main` is rejected as a publication source.
  Manual publication by commit SHA publishes container images only; the manifest
  job skips when the source has no installable Stable/Nightly release id.
- A manual build without `JMODGUARD_PUBLISH_SOURCE` runs tests only. Use this in
  place of the retired Actions `CI failover`. No hosted fallback is automatic.

Published images remain single-manifest `linux/amd64`, without provenance/SBOM
indexes. Bot/API main publications carry `latest` and full-source-SHA tags.
Nightly publications use only the SHA and unique nightly tag. Tag/manual-tag
builds add the normalized tag name when Docker accepts it. Prior versions remain available
for approved releases and rollback. Publishing an older source can move `latest`
backward; production must keep using approved image digests.

## Cutover verification

1. Confirm `GHCR_USERNAME` is in `.buildkite/pipeline.yml`, `GHCR_TOKEN` exists
   under **Agents → cluster → Secrets** (not pipeline settings), and the secret
   policy plus Docker/Buildx on the `linux` agents. Existing clone/test success
   alone does not prove publication access.
2. Merge the reviewed migration and confirm the next main Buildkite build passes
   both tests, preparation, and both image publications. If a publish fails,
   correct the credential/agent setup and retry it; no GHA fallback starts.
3. Check the published image's source SHA and single-manifest shape in GHCR and
   the Buildkite check on GitHub. No Actions jobs should start for this repo.
4. Let existing Actions builds finish before stopping/unregistering
   `gha-runner-jmodguard`. Confirm it is not used by another repository or private
   ops workflow before removing its service. Keep the Buildkite agents running.

Local offline publisher tests: `python3 .buildkite/publish-image-test.py`.
They exercise source identity, event guards, multi-commit change detection,
credential handling and cleanup without pushing images or contacting Buildkite.
Release publication contracts: `python3 .buildkite/release-test.py`.
See [releases](RELEASES.md).

References: [Buildkite GitHub integration](https://buildkite.com/docs/pipelines/source-control/github),
[Buildkite secrets](https://buildkite.com/docs/pipelines/security/secrets/buildkite-secrets),
[secret access policies](https://buildkite.com/docs/pipelines/security/secrets/buildkite-secrets/access-policies),
[GHCR authentication](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry).
