# GitHub Actions CI and image publishing

`.github/workflows/ci.yml` validates PRs and runs on standard ephemeral `ubuntu-24.04` runners.
`.github/workflows/publish.yml` queues trusted events and calls the same validation
workflow before release/publication. CI and publication stay inactive while the repository is private. The owner can
set repository variable `JMODGUARD_ENABLE_PRIVATE_CI=true` for an explicit trial;
it is not needed after publication. Pullfrog and Socket retain their separate triggers.

There are no registered GitHub Actions self-hosted runners. Pullfrog and Socket
remain separate workflows; neither replaces CI or publishes images.

## Affected checks

`scripts/select-checks.sh` computes changes with local Git history, without the
GitHub path-filter file limit. PRs use the event base/head merge base and cover
all changes in the PR, including an earlier code edit followed by a docs commit.
Main pushes use the entire event `before..after` range. Renames select both the
old and new paths; deletions count. Missing commits, unknown paths and CI control
changes select every group. Proven empty diffs run contracts only.
Opening a draft PR already runs CI; marking it ready does not repeat the same
commit's checks. New commits and reopened PRs still run affected checks.

| Touched surface | Checks |
|---|---|
| Documentation or sample configuration | Contracts only |
| Frontend | Frontend and API (the default API embeds the generated UI) |
| API or bot | That service, integration, boundaries, formatting, scoped Dylint and supply chain |
| Shared root source or migrations | Core plus bot/API/updater/integration consumers and remote databases |
| Runtime | Runtime and its core/integration consumers, boundaries, formatting, Dylint and supply chain |
| Updater | Updater plus bot/API consumers and boundaries, formatting, Dylint and supply chain |
| Cargo manifests/lockfile, toolchain or vendor | Relevant Rust gates, with global inputs selecting all Rust gates |
| Unknown paths or CI orchestration | All checks |

Selected groups run in up to eight jobs: quick contracts/shell checks/formatting,
frontend, three Rust jobs, Dylint, supply chain and remote databases. Rust work is
split into API/boundaries, bot/integration, and core/updater/runtime. Each selected
group still runs its existing independent package commands and feature variants.
Bot and integration share their service builds on one runner; each job has its own
Cargo target directory. The Rust jobs restore the existing shared dependency cache
and use pinned sccache 0.18.0 to share compiler outputs through GitHub's cache.
Source, feature, compiler and embedded-file changes invalidate compiler entries;
cache hits never skip tests. Linking and check-only compiler calls still run,
and simultaneous cold misses can compile the same input before either saves it.
A Rust toolchain is not installed for documentation-only contracts. PR runs cancel
superseded runs; trusted publication runs queue without replacement or cancellation
(up to GitHub’s limit of 100 waiting runs). npm and
Rust caches reduce repeated downloads/compilation. PRs do not save the shared
Rust target cache; GitHub isolates fork/PR caches from default-branch writes.
Compiler-cache writes from PRs stay in that PR's merge-ref scope, which main and
other PRs cannot restore. Main pushes populate the shared compiler baseline;
scheduled/manual/tag validations use it read-only. Compiler-cache statistics
are included in each Rust job's log and summary.
Cache entries contain no credentials.

Require **CI passed** in branch rules. It is always evaluated for an eligible
run and fails if planning or any selected job fails, is cancelled, or is skipped.
Do not require individual matrix jobs, whose presence depends on changed paths.
The workflow has no top-level path filters that leave required checks pending.

## Toolchains and gates

Rust follows `rust-toolchain.toml` (1.96.0), with clippy and rustfmt. Dylint uses
6.1.0 and nightly-2026-08-20 from `dylint.toml`; the script installs source tools
and their driver in a dedicated cache. Node 24 satisfies both npm projects.
Cargo deny 0.20.2 and cargo audit 0.22.2 remain pinned. The local complete gate is:

```bash
bash scripts/ci-preflight.sh
```

The hosted Rust jobs install checksum-pinned cargo-nextest 0.9.148. Preflight uses
it to run tests in parallel, in a separate process per test, without launching
Cargo for every test. Its concurrency follows the runner's available CPUs; the
existing 180-second per-test limit remains. Doctests still use the timed Cargo
path, and the ignored two-process smoke still runs after building both services.
Local preflight uses nextest when installed, with the existing GNU timeout
fallback for unavailable nextest or unsupported arguments.

CI passes selected groups as repeated `--only` arguments. Local hooks always run
the full gate. Separate `bash scripts/ci-remote-database.sh` starts disposable
loopback-only libSQL containers for the existing remote database, guild store,
and Bunny journal tests. These fixtures do not contact Discord, Bunny accounts,
or operator databases.

Both npm audits fail on high/critical findings or endpoint errors, including
`npm audit --prefix tools/eslint --audit-level=high`. Cargo advisory reports remain
non-blocking for PRs/ordinary main pushes; operational audit failures always fail.
Scheduled runs set `JMODGUARD_AUDIT_FAIL_CLOSED=1`, run all gates, and withhold
Nightly publication when an advisory is outstanding. `RUSTSEC-2026-0258` is the
one ignored cargo-audit advisory (see #349); its UTC expiry is in
`.cargo/audit-expiries` (2027-01-15). Every ignored advisory needs an expiry
there; any other unfixable advisory is handled by pinning or dropping that
package. Preflight fails on the expiry date while the ignore remains. Scheduled
runs still fail closed on every other advisory.

## Publication and trust

PR jobs use `pull_request`, `contents: read`, and no repository publishing secrets.
They never publish images. Checkout does not persist credentials. A read-only
workflow token authenticates the repository's Cargo Git patch while private.
Pullfrog keeps its separate owner identity/command checks.

After successful checks, trusted main pushes publish changed components. Owner
pushes of `v*` tags publish release images. A daily schedule runs full validation
and produces Nightly images if every gate passes. The owner can run **Publish → Run workflow**
on `main`; leave `source` empty for checks only or supply a full lowercase commit
SHA or `refs/tags/<tag>` to publish that historical source. Plain branch names and
abbreviated SHAs are rejected. Manual/tag publication checks the owner's login,
numeric account ID and rerun actor. Historical images retain their actual source
layout and compatibility limitations.

One preparation job resolves all components to the same source SHA. Its small
`publication-plan` artifact is retained for 90 days and restored on retries of
that run; a mutable tag is not resolved again. An expired, unreadable or missing
retry plan fails rather than silently changing the source. Release Please runs
only on the first attempt; retries restore the plan without creating another release.
If a release was created before the action, preparation, or artifact upload failed
to retain its plan, start a fresh owner-dispatched **Publish** run on `main` with
`source=refs/tags/<created-tag>`. An empty source runs checks only; a later ordinary
main run cannot recreate the already-consumed release output. Start a new run for
a deliberate new selection. The publisher verifies image contents before promoting moving tags.
Nightly tags never move `latest`. An explicit rerun republishes its pinned source.

Each component compares its actual production inputs against the source revision
in its public `latest` image and confirms `latest` and the recorded SHA tag agree.
Unchanged inputs skip compilation and registry login. Bot-only production source
does not rebuild the API; API-only source does not rebuild the bot. Shared source,
launcher, migrations, every Cargo manifest and the license affect both services.
The WebUI has its own inputs, including schema/compatibility-label sources. Missing
history or registry evidence rebuilds conservatively. PR tests do not build/push
release images. Published image layers use the previous `latest` cache.

The package jobs alone receive `packages: write`; each uses the ephemeral
`GITHUB_TOKEN`, not a registry PAT. For all three existing GHCR packages, grant
this repository **Write** under **Package settings → Manage Actions access**.
Keep anonymous package reads available to public installers. Publication jobs
serialize per component, and trusted workflow runs share a concurrency group.

## Releases

The pinned Release Please action uses `release-please-config.json` and
`.release-please-manifest.json` after trusted main checks pass. It opens/updates
Stable release PRs and creates releases after the owner merges them. It never
merges a PR. Enable GitHub Actions permission to create pull requests if using
the default token. PRs created with `GITHUB_TOKEN` do not automatically trigger
new PR workflows: the owner must close/reopen the release PR to start CI before
merging it. A release created by this action is published within the same trusted
workflow, since tags created with `GITHUB_TOKEN` do not trigger another run.
The plan binds the action's release tag to its reported source SHA, which can
differ from the workflow's triggering commit. When it differs, the same reusable
CI workflow checks out that exact pinned source and runs every check group before
any component can publish; the triggering commit's affected diff is not reused.
A failed, cancelled, or skipped source-validation gate blocks all three publishers.
Same-source releases reuse the checks already passed. Retries skip Release Please,
restore the original tag/source, and apply the same source-validation rule.
Owner-selected historical/manual publication keeps its existing policy: validate
the current workflow commit, then build and verify the selected historical source.
See [RELEASES.md](RELEASES.md).

## Dependency tools

Dependabot version updates appear in **Actions → Dependabot Updates**. Keep
**Dependabot on self-hosted runners** disabled. These jobs propose dependency
updates; the PR then receives the selected application CI checks.

Set the
repository Actions secret `SOCKET_SECURITY_API_KEY` to a Socket API key before
relying on the scan. A missing secret skips the scan and does not fail the job, and the key is not stored in git.
The scan uploads discovered Cargo and npm manifests outside ignored paths to Socket, including workspace and member `Cargo.toml` files, `Cargo.lock`, and `tools/eslint/package.json` and its lockfile. It may comment on a pull request with the GitHub Actions token.
Socket runs once per PR update and on main pushes, without a duplicate feature-
branch push scan. `select-checks.sh socket` uses the same complete Git diff to
select Cargo/npm manifests, lockfiles, npm/ignore configuration and scanner changes.
Known unrelated source/documentation changes skip Python/CLI setup and the scan;
unknown paths or unavailable history still scan. This uses no GitHub path-filter
file limit and includes deleted or renamed dependency inputs.
A new blocking alert can fail this check only when Socket compares a pull request change that touches a supported manifest. Push and full-report scans do not fail for alerts. Socket API errors do not fail this check. The CI passed check remains the merge gate.
After enabling the key, let a scan run on `main` before relying on pull request comparisons; without a main baseline, Socket may treat existing dependencies as newly added.

## Local contract checks

```bash
bash scripts/select-checks-test.sh
bash scripts/publish-image-test.sh
node scripts/ci-workflow-test.mjs
```

Publisher tests simulate Docker/registry responses and never publish images.
They do not prove live GHCR access, a GitHub-hosted run, or provider rollout.
