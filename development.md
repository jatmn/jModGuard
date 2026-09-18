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

The Buildkite `test` job is that script. Remote libSQL containers stay in the
separate `remote-database` job.

Keep secrets, production domains, and operator-specific promote steps out of
this repository — document those in your own ops environment.

CI and image publication run on self-hosted Buildkite agents. Setup and manual
builds: **[docs/BUILDKITE.md](docs/BUILDKITE.md)**.

## Dependency updates

`.github/dependabot.yml` schedules weekly Cargo workspace
checks after it is merged to `main`. Cargo minor and patch version updates
are grouped; major updates remain separate. Dependabot permits up to
five open version-update PRs. Review and run the Buildkite checks before
merging dependency updates.

The `libsql-sqlite3-parser` Git patch is excluded from automatic updates:
maintain its reviewed revision and security regression test manually.
The vendored `tokio-tungstenite` compatibility patch also requires manual
maintenance. Container images remain manual: Rust builder versions must
stay aligned with `rust-toolchain.toml`, the root Cargo version requirements,
and the Buildkite agents. `Dockerfile.{bot,api,conductor}` pin
`rust:1.96-bookworm` and `debian:bookworm-slim` by linux/amd64 digest. There is
no Docker Dependabot job; bump those digests together with the toolchain pin.

Dependabot supports private source repositories. The Cargo Git patch points
back to this repository; it is not a GitHub Packages registry dependency.
If a job reports private Git authentication failures, inspect its update log
and the repository's Dependabot access settings. Do not put tokens in YAML.
The separate GitHub Packages automatic-access feature requires granting the
consuming repository Read access under the package's **Manage Actions access**
settings; it does not provide credentials to Buildkite.

Dependabot alerts and security updates are separate repository settings.
Keep both enabled under **Settings → Advanced Security → Dependabot**;
the weekly schedule controls version updates, not the timing of security
fixes. Check update logs under **Insights → Dependency graph → Dependabot**
after merging the configuration. These jobs generate PRs; application CI
and image publication remain on Buildkite.
