# Development notes

Product architecture and dual-process design:
**[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

Optional Bunny.net deploy guidance (for self-hosters who want that shape):
**[docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md)**.

Upgrades: **[docs/UPGRADE.md](docs/UPGRADE.md)**.

Contributor / agent build commands and invariants: **[AGENTS.md](AGENTS.md)**.

## Local validation

For ordinary PRs, reproduce the reported failure and run the smallest relevant
regression plus the affected local checks. Record the commands and results in the
PR. **CI passed** must succeed before merge; GitHub Actions selects all affected
groups from the complete PR diff, including earlier code commits.

`--affected` runs the groups GitHub Actions would select for this checkout:
its commits since the base plus uncommitted and untracked files. It is the
complete local plan for a change, and it runs the full suite by itself when the
change needs one (CI orchestration or an unclassified path). It leaves out only
the Docker-based remote-database job and says so when that job is affected.

```bash
bash scripts/ci-preflight.sh --affected
# For a PR whose base is not main:
# bash scripts/ci-preflight.sh --affected --base origin/<base-branch>
```

The examples below are narrower partial feedback, not complete affected-check
plans. `--only` runs the named groups; it does not add their affected consumers
or boundary checks. CI runs the complete affected plan before merge.

```bash
# Documentation or sample configuration:
bash scripts/ci-preflight.sh --only contracts
# Browser changes (includes dependency install/build, lint and frontend tests):
bash scripts/ci-preflight.sh --only frontend
# Bot-only Rust work (Clippy, package tests and binary build):
bash scripts/ci-preflight.sh --only fmt --only bot
# API work: both feature modes and builds:
bash scripts/ci-preflight.sh --only fmt --only api
# Inspect all groups selected for concrete paths; include every changed surface:
bash scripts/select-checks.sh plan --files src/db.rs frontend/app.js
```

Shared data changes need their consumers; service-boundary changes need integration
and boundary checks. Run `bash scripts/ci-remote-database.sh` when changing remote
SQL/transaction behavior; it uses disposable local libSQL containers, not a live
provider. Frontend embedding and other affected consumers are also checked by CI.
A focused local run does not replace that hosted gate.

Run the full preflight before submitting changes to CI orchestration, toolchains,
workspace-wide dependency/build configuration, or when affected scope is uncertain.
Do not run it for an ordinary change: `--affected` already covers what CI will
check, and selects the full suite by itself when the change needs it.

```bash
bash scripts/ci-preflight.sh
# Equivalent full-suite alias (not a cheap smoke command):
# bash scripts/smoke-dual-process.sh
```

Run broad validation once on the final candidate. After another edit, rerun checks
whose inputs changed and reuse unaffected evidence; do not rerun the entire suite
just because a commit or push occurred. Do not omit a distinct regression merely
to lower the test count.

Install cargo-nextest for local Rust runs (`cargo install cargo-nextest --locked
--version 0.9.148`, the CI pin). Preflight uses it automatically. Without nextest,
the GNU timeout fallback invokes Cargo separately for each test and runs them
sequentially; thousands of cases make this much slower. Both paths retain per-test
timeouts. A cold install/build is setup cost, not application-test execution time.

Optional local hooks:

```bash
bash scripts/install-git-hooks.sh
# For a PR whose base is not main:
# bash scripts/install-git-hooks.sh --base origin/<base-branch>
```

This sets worktree-local `core.hooksPath` to `.githooks`. Pre-commit checks only
staged whitespace; pre-push runs `ci-preflight.sh --only contracts` once and blocks
on failure. Neither starts the full Rust suite. Run relevant tests before pushing;
the hooks do not infer application coverage. Git uses only `.githooks` in this
worktree; previous hook directories are not chained. Linked worktrees are unaffected
unless you install there too. Existing installations pick up the tracked hook edits.

Keep secrets, production domains, and operator-specific promote steps out of
this repository — document those in your own ops environment.

CI and image publication run on standard GitHub-hosted runners. Setup and manual
builds: **[docs/CI.md](docs/CI.md)**.

## Dependency updates

`.github/dependabot.yml` schedules weekly Cargo workspace checks and weekly
npm updates for the host-only ESLint pin under `tools/eslint` after they are
merged to `main`. Cargo and npm minor and patch version updates are grouped;
major updates remain separate. Dependabot permits up to
five open version-update PRs per ecosystem (Cargo and npm each). Review and run the GitHub Actions checks before
merging dependency updates.

The `libsql-sqlite3-parser` Git patch is excluded from automatic updates:
maintain its reviewed revision and security regression test manually.
The vendored `tokio-tungstenite` compatibility patch also requires manual
maintenance. Container images remain manual: Rust builder versions must
stay aligned with `rust-toolchain.toml`, the root Cargo version requirements,
and the CI toolchain. `Dockerfile.{bot,api}` pin
`rust:1.96-bookworm` and `debian:bookworm-slim` by linux/amd64 digest. There is
no Docker Dependabot job; bump those digests together with the toolchain pin.

Dependabot version updates are supported on this private GitHub.com
repository. GitHub enqueues those jobs as **Actions → Dependabot Updates**
dynamic workflows (`dynamic/dependabot/dependabot-updates`). They are not
`.github/workflows` files, they do not replace application CI, and on standard
GitHub-hosted runners they do not consume included Actions minutes. A
missing Insights update log does not mean the weekly schedule is idle: if
the job stays **queued** on a `dependabot` label and then **cancelled** at
24 hours, no runner matched.

Keep **Settings → Advanced Security → Dependabot on self-hosted runners**
**disabled** so GitHub-hosted standard runners can execute the job. This
repository has no GitHub Actions self-hosted runners; CI uses standard GitHub-hosted runners.
The Socket Security manifest scan is `.github/workflows/socket.yml` on GitHub
Actions. It is not CI. Set the repository Actions secret
`SOCKET_SECURITY_API_KEY` to a Socket API key before relying on the scan.
A missing secret skips the scan and does not fail the job, and the key is not stored in git.
The scan uploads discovered Cargo and npm manifests outside ignored paths to Socket, including workspace and member `Cargo.toml` files, `Cargo.lock`, and `tools/eslint/package.json` and its lockfile. It may comment on a pull request with the GitHub Actions token.
A new blocking alert can fail this check only when Socket compares a pull request change that touches a supported manifest. Push and full-report scans do not fail for alerts. Socket API errors do not fail this check. The CI passed check remains the merge gate.
After enabling the key, let a scan run on `main` before relying on pull request comparisons; without a main baseline, Socket may treat existing dependencies as newly added.
Do not register leftover Proxmox GHA runners or label any runner
`dependabot`. `bash scripts/dependabot-config-test.sh` guards this YAML
and the operator-copy contract.

The Cargo Git patch points back to this repository; it is not a GitHub
Packages registry dependency. The 2026-09-08 GitHub Packages
automatic-access change does not authenticate that git clone. If a job
starts and then reports private Git authentication failures, inspect that
run's Actions log. Do not put tokens in YAML. The separate GitHub Packages
feature requires granting the consuming repository Read access under the
package's **Manage Actions access** settings; it does not provide
Git fetch authentication.

Dependabot alerts and security updates are separate repository settings.
Keep both enabled under **Settings → Advanced Security → Dependabot**;
the weekly schedule controls version updates, not the timing of security
fixes. After a job **starts**, check **Actions → Dependabot Updates** and
**Insights → Dependency graph → Dependabot**. These jobs generate PRs;
application CI and image publication remain on GitHub Actions.
