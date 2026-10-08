# Pullfrog: owner commands only

Pullfrog is an on-demand agent for this repository. Buildkite remains the only
CI and image publisher. Pullfrog's workflow,
`.github/workflows/pullfrog.yml`, listens for newly created issue comments and
top-level PR comments. It does not listen to inline PR review comments, which
run GitHub workflows from a PR merge commit. It has no push, PR, schedule, or
`workflow_dispatch` trigger. The Socket Security manifest scan is the separate
workflow `.github/workflows/socket.yml`, and it does not listen for comments.
Set the repository Actions secret `SOCKET_SECURITY_API_KEY` to a Socket API key
before relying on the scan. A missing secret skips the scan and does not fail the job, and the key is not stored in git.
The scan uploads discovered Cargo and npm manifests outside ignored paths to Socket, including workspace and member `Cargo.toml` files, `Cargo.lock`, and `tools/eslint/package.json` and its lockfile. It may comment on a pull request with the GitHub Actions token.
A new blocking alert can fail this check only when Socket compares a pull request change that touches a supported manifest. Push and full-report scans do not fail for alerts. Socket API errors do not fail this check. Buildkite remains the merge gate.
After enabling the key, let a scan run on `main` before relying on pull request comparisons; without a main baseline, Socket may treat existing dependencies as newly added.
The agent step runs only when GitHub identifies
the commenter and event actor as `jatmn` (account ID `12479882`) and the comment
starts with `@pullfrog` followed by a visible instruction on the same first
line. A public contributor's comment, a quoted/passive mention, an edited
comment, a workflow rerun, and a
bare `@pullfrog` do not run the agent. The workflow checks out the trusted
default branch and passes the comment URL and verified issue/PR event context
to the agent, which must read the current issue or PR. The structured context
lets Pullfrog associate its native checks with the PR instead of treating the
request as an unknown event. Issue commands do not create PR checks.

For example, create a **new top-level comment** on an issue or PR:

```text
@pullfrog review this PR and report actionable defects
```

The workflow has no `workflow_dispatch`, so Pullfrog's dashboard prompt box and
its managed webhook automations cannot launch this workflow. Do not replace it
with the dashboard's generated workflow. The agent action has feature-branch-only
push permission and a restricted shell; default-branch pushes are not allowed.
## PR checks and run diagnostics

The workflow pins the Pullfrog v0.1.97 action bootstrap and explicitly enables
its native `pullfrog` run-status and `pullfrog-approval` verdict checks. The
bootstrap installs the compatible `pullfrog@^0.1.97` CLI from npm, so the runtime
can receive later 0.1.x updates. The `status_checks` input is
deprecated upstream but remains supported on v0; it enables
reporting even if the console's check toggles are off and emits a deprecation
warning. It does not enable managed
mentions, automatic reviews, or dispatch. Keep those automations off.

The workflow selects `openai/gpt-sol` and uses the existing ChatGPT
subscription stored by Pullfrog. It no longer passes the legacy
`OPENCODE_API_KEY` or `PULLFROG_MODEL` GitHub Actions secrets.

A completed PR command can publish `pullfrog` on the PR head through Pullfrog's
GitHub App credentials. A clean run that records a review verdict can also
publish `pullfrog-approval`; that check does not grant a GitHub approving review
when the console's approval permission is disabled. Read the posted review for
findings. Early startup failures can occur before Pullfrog initializes its
check reporter, so inspect the linked **Pullfrog owner command** Actions run
as well. Successful Actions status alone does not prove every internal API call
succeeded.

The helper builds Pullfrog's version-specific structured prompt with
`JSON.stringify`; owner text cannot inject workflow output keys or event fields.
Short commands are included once in the prompt. When JSON encoding would exceed
64 KiB, the prompt instead tells the agent to read the complete `comment.body`
from GitHub's existing event snapshot file before acting. This keeps large
commands below the action-input environment limit and preserves the original
authorized text, including later lines, without refetching an edited comment.
When updating the action pin, verify its `JsonPayload`, `PayloadEvent`, and
`reportStatusChecks` contracts, update `PULLFROG_VERSION` in
`scripts/pullfrog-command-check.mjs`, and run the local checks below.

Existing failed managed-dispatch checks are historical failures. Disabling
mentions or changing YAML does not rewrite them. A **new** owner PR command
can post a new native check on the current PR head; verify that check's
timestamp and linked Actions run.
Do not use **Re-run jobs**, which the first-attempt guard intentionally skips.

Older Pullfrog v0.1.82 runs could log
`patchWorkflowRunFields failed: Error: PATCH workflow-run: 404` when a standalone
run attempts to save metadata without a server-reserved run record. This is an
upstream persistence limitation, separate from the review and GitHub check
publication. This repository does not suppress that warning or fix Pullfrog's
servers. Adding `workflow_dispatch` would change the owner-only execution policy
and is not part of this workflow.

## Local checks

```bash
node scripts/pullfrog-command-check.test.mjs
bash scripts/dependabot-config-test.sh
bash scripts/ci-preflight.sh
```
