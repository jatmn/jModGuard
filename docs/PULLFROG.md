# Pullfrog owner commands and reviews

Pullfrog handles owner commands and automatic reviews for this repository. The separate CI workflow owns
checks and image publication. Pullfrog's workflow,
`.github/workflows/pullfrog.yml`, listens for newly created issue comments and
top-level PR comments. It does not listen to inline PR review comments, which
run GitHub workflows from a PR merge commit. The Socket Security manifest scan is the separate
workflow `.github/workflows/socket.yml`, and it does not listen for comments.
Set the repository Actions secret `SOCKET_SECURITY_API_KEY` to a Socket API key
before relying on the scan. A missing secret skips the scan and does not fail the job, and the key is not stored in git.
The scan uploads discovered Cargo and npm manifests outside ignored paths to Socket, including workspace and member `Cargo.toml` files, `Cargo.lock`, and `tools/eslint/package.json` and its lockfile. It may comment on a pull request with the GitHub Actions token.
A new blocking alert can fail this check only when Socket compares a pull request change that touches a supported manifest. Push and full-report scans do not fail for alerts. Socket API errors do not fail this check. The CI passed check remains the merge gate.
After enabling the key, let a scan run on `main` before relying on pull request comparisons; without a main baseline, Socket may treat existing dependencies as newly added.
For manual commands, the agent step runs only when GitHub identifies
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

Manual commands have feature-branch push permission and a restricted shell.
Automatic reviews have push disabled.
## Automatic reviews

Pullfrog reviews new PRs, drafts, and updated diffs in this public repository
when GitHub identifies `jatmn` as the PR author. The person pushing an update
can be a different account. The GitHub App dispatches the workflow on the
default branch and the author check must pass before the agent starts.
Merge the workflow to the default branch and verify it declares
`workflow_dispatch` there before enabling automatic reviews in Pullfrog.
Then enable PR reviews, drafts, and re-reviews on new commits, and disable
managed mentions and external-contributor reviews. Open a fresh owner-authored
draft and push an update. On the latest PR commit, confirm the completed
automatic run published a review and both native checks. If a
non-owner collaborator is available, check a PR they authored. Confirm that
the workflow skips the review action, no review is posted, and any
Pullfrog-created run-status check reaches a terminal state.

## PR checks and run diagnostics

The workflow pins the Pullfrog v0.1.138 action bootstrap. The bootstrap installs
the compatible `pullfrog@^0.1.138` CLI from npm. Manual owner commands set the
legacy `status_checks` input to enable native checks. Automatic reviews use the
Pullfrog console settings. In **Automations → Review PRs**, enable both the run
status check and the approval verdict check to publish `pullfrog` and
`pullfrog-approval`.

Manual commands select `openai/gpt-sol` in the workflow. Automatic reviews use
the repository model in Pullfrog, currently `openai/gpt-sol`. Both use the ChatGPT
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
If GitHub author verification exhausts its retries during an outage, wait for
GitHub to recover and post a fresh top-level `@pullfrog review this PR` command
as `jatmn`. GitHub reruns reuse the original workflow revision and event.

Older Pullfrog v0.1.82 runs could log
`patchWorkflowRunFields failed: Error: PATCH workflow-run: 404` when a standalone
run attempts to save metadata without a server-reserved run record. This is an
upstream persistence limitation, separate from the review and GitHub check
publication. This repository does not suppress that warning or fix Pullfrog's
servers.

## Local checks

```bash
node scripts/pullfrog-command-check.test.mjs
bash scripts/dependabot-config-test.sh
bash scripts/ci-preflight.sh
```
