# Copilot code review instructions for jModGuard

Copilot is a backup reviewer here, requested manually only when Sourcery can't review. It is advisory: PR Reviewer and Pullfrog form the merge gate. See `AGENTS.md` for the full contracts.

## Layout
- `src/`: the shared `jmodguard` data and config crate. `src/db.rs` is the local/remote libSQL pool and data-layer facade, and it runs its own database runtimes. `src/db/` holds bootstrap/migrations, configuration, settings, moderation and tests. `src/db_compat.rs` has remote deadlines and rollback, and `src/tenant.rs` holds tenant stores.
- `crates/bot`: the Discord gateway bot (Serenity). It handles automod, commands, action and nickname workers, and is the only role that migrates.
- `crates/api`: the axum WebUI/API. It never migrates and dispatches through the queue, not the bot token.
- `crates/updater`: the in-app updater. `src/launcher_child.rs` is the launcher child side.
- `crates/runtime`: process primitives (health, cgroup). It must not import root data, Serenity, or bot/API state.
- `crates/integration-tests`: cross-service tests. `migrations/`, `frontend/` and `docs/` round it out.

## Hard rules (flag any break as P1)
- The bot must always be able to moderate. No change may block or disable bans, kicks, timeouts, deletes or automod enforcement.
- The in-app updater never runs schema migrations. Schema changes use the durable bot/API paired protocol, and recovery after the migration fence is forward-only.
- Never display or estimate host-wide memory; reading MemTotal solely to detect an unbounded cgroup limit is allowed. Use the process's or container's own limits only.
- Keep the service boundaries. Bot and API dependency graphs stay independent, the API never migrates, and API work never takes unscoped queue or moderation access.
- Never run core migrations or defaults on a guild store. Credentials stay in process config, never in catalog rows.

## Rust conventions
- Code must pass Clippy with `-D warnings`. No `unwrap()`/`expect()` in runtime paths (tests are fine).
- Every behavior change needs a test at the smallest reliable boundary that proves it.

## How to review
- Stay within the PR's stated scope. Don't ask for unrelated refactors.
- Report only real bugs, contract breaks, security issues, data loss, and missing tests for changed behavior. Skip style nits and formatting.
- Start each finding with a priority: **P1** for a bug, data loss, security issue, or hard-rule break; **P2** for a likely bug or a missing test; **P3** for a minor but real issue.
- Never comment on Release Please PRs (`chore(main): release …`) or on `CHANGELOG.md` / `.release-please-manifest.json` changes.
