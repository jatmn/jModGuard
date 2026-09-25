# Development notes

Product architecture and dual-process design:
**[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

Optional Bunny.net deploy guidance (for self-hosters who want that shape):
**[docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md)**.

Upgrades: **[docs/UPGRADE.md](docs/UPGRADE.md)**.

Contributor / agent build commands and invariants: **[AGENTS.md](AGENTS.md)**.

Automated verification (no Discord tokens):

```bash
bash scripts/ci-preflight.sh
# or: ./scripts/smoke-dual-process.sh
```

Local hooks run that full script. Buildkite selects groups from the diff
(`bash .buildkite/select-checks.sh`) and runs `ci-preflight.sh --only <group>`.
Remote libSQL containers stay in the `remote-database` group, which is
skipped for docs-only, WebUI-only, and API-only diffs. Optional local hooks:

```bash
bash scripts/install-git-hooks.sh
# For a PR whose base is not main:
# bash scripts/install-git-hooks.sh --base origin/<base-branch>
```

That sets worktree-local `core.hooksPath` to `.githooks` so `pre-commit` and
`pre-push` run the full `bash scripts/ci-preflight.sh` suite (commit then
push runs it twice). Git then uses only `.githooks` in this worktree; previous
hook directories are not chained. Linked worktrees are unaffected unless you run
install there too. It does not add GitHub Actions.

Keep secrets, production domains, and operator-specific promote steps out of
this repository — document those in your own ops environment.

CI and image publication run on self-hosted Buildkite agents. Setup and manual
builds: **[docs/BUILDKITE.md](docs/BUILDKITE.md)**.

## Dependency updates

`.github/dependabot.yml` schedules weekly Cargo workspace checks and weekly
npm updates for the host-only ESLint pin under `tools/eslint` after they are
merged to `main`. Cargo and npm minor and patch version updates are grouped;
major updates remain separate. Dependabot permits up to
five open version-update PRs per ecosystem (Cargo and npm each). Review and run the Buildkite checks before
merging dependency updates.

The `libsql-sqlite3-parser` Git patch is excluded from automatic updates:
maintain its reviewed revision and security regression test manually.
The vendored `tokio-tungstenite` compatibility patch also requires manual
maintenance. Container images remain manual: Rust builder versions must
stay aligned with `rust-toolchain.toml`, the root Cargo version requirements,
and the Buildkite agents. `Dockerfile.{bot,api,conductor}` pin
`rust:1.96-bookworm` and `debian:bookworm-slim` by linux/amd64 digest. There is
no Docker Dependabot job; bump those digests together with the toolchain pin.

Dependabot version updates are supported on this private GitHub.com
repository. GitHub enqueues those jobs as **Actions → Dependabot Updates**
dynamic workflows (`dynamic/dependabot/dependabot-updates`). They are not
`.github/workflows` files, they do not replace Buildkite, and on standard
GitHub-hosted runners they do not consume included Actions minutes. A
missing Insights update log does not mean the weekly schedule is idle: if
the job stays **queued** on a `dependabot` label and then **cancelled** at
24 hours, no runner matched.

Keep **Settings → Advanced Security → Dependabot on self-hosted runners**
**disabled** so GitHub-hosted standard runners can execute the job. This
repository has no GitHub Actions self-hosted runners; CI is Buildkite.
The Socket Security manifest scan is `.github/workflows/socket.yml` on GitHub
Actions. It is not CI. Set the repository Actions secret
`SOCKET_SECURITY_API_KEY` to a Socket API key before relying on the scan.
A missing secret skips the scan and does not fail the job, and the key is not stored in git.
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
credentials to Buildkite.

Dependabot alerts and security updates are separate repository settings.
Keep both enabled under **Settings → Advanced Security → Dependabot**;
the weekly schedule controls version updates, not the timing of security
fixes. After a job **starts**, check **Actions → Dependabot Updates** and
**Insights → Dependency graph → Dependabot**. These jobs generate PRs;
application CI and image publication remain on Buildkite.
