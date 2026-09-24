# Pullfrog: owner commands only

Pullfrog is an on-demand agent for this repository. Buildkite remains the only
CI and image publisher. The sole GitHub Actions workflow,
`.github/workflows/pullfrog.yml`, listens for newly created issue comments and
top-level PR comments. It does not listen to inline PR review comments, which
run GitHub workflows from a PR merge commit. It has no push, PR, schedule, or
`workflow_dispatch` trigger. The agent step runs only when GitHub identifies
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
The workflow begins working only after this file is merged to the default
branch and the manual setup below is finished.

## Manual account setup after reviewing the draft PR

1. Merge the reviewed PR to `main`. GitHub comment event workflows load their
   workflow definition from the default branch.
2. Sign in at [Pullfrog](https://pullfrog.com/console) using `jatmn`. Install
   the [Pullfrog GitHub App](https://github.com/apps/pullfrog) on the personal
   account, granting access **only to `jatmn/jModGuard`**. Select this repo in
   the Pullfrog console. If onboarding offers to generate or overwrite
   `pullfrog.yml`, skip that step; this repository already has a custom workflow.
3. In the repo console, keep every automation off: automatic PR reviews,
   review responses, issue enrichment, CI autofix, merge-conflict fixes, and
   any other event-triggered agent run. Turn off Quick Links for issues and
   PRs. Do not enable non-collaborator mentions. If the Mentions settings offer
   a switch for Pullfrog's built-in mention dispatcher, turn it off too. The
   workflow's owner check is the final execution guard, including against
   mentions by other collaborators.
4. Under account **Billing → Model usage**, choose **BYOK**. Add your OpenCode
   Go key as the repository's GitHub Actions secret named `OPENCODE_API_KEY`;
   the workflow already maps that secret only to the Pullfrog step. Choose an
   **OpenCode Go** model in the repo's Agent settings. The key and model
   configuration are account details and are not stored in this repository.
5. After setup, post one new `@pullfrog ...` comment yourself and confirm that
   exactly one `Pullfrog owner command` Actions job runs. Check that a comment
   by another account, or your own comment without an instruction, never
   reaches the agent step. A workflow rerun is skipped; post a new owner comment
   if you want another run. Do not test with a paid or destructive instruction.

Pullfrog's standard console workflow uses `workflow_dispatch` and allows all
repository collaborators to invoke mentions by default. This repository uses
the documented [standalone action](https://docs.pullfrog.com/headless-action)
instead so GitHub's signed comment event supplies the requester's identity.
Pullfrog account settings may change over time; verify the toggles during setup.

## PR checks and run diagnostics

The workflow pins the Pullfrog v0.1.82 action bootstrap and explicitly enables
its native `pullfrog` run-status and `pullfrog-approval` verdict checks. The
bootstrap installs the compatible `pullfrog@^0.1.82` CLI from npm, so the runtime
can receive later 0.1.x updates. The `status_checks` input is
deprecated upstream but remains supported on v0; it enables
reporting even if the console's check toggles are off and emits a deprecation
warning. It does not enable managed
mentions, automatic reviews, or dispatch. Keep those automations off.

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
When updating the action pin, verify its `JsonPayload`, `PayloadEvent`, and
`reportStatusChecks` contracts, update `PULLFROG_VERSION` in
`scripts/pullfrog-command-check.mjs`, and run the local checks below.

Existing failed managed-dispatch checks are historical failures. Disabling
mentions or changing YAML does not rewrite them. After this repair is merged to
the default branch, a **new** owner PR command can post a new native check on
the current PR head; verify that check's timestamp and linked Actions run.
Do not use **Re-run jobs**, which the first-attempt guard intentionally skips.

Pullfrog v0.1.82 can still log
`patchWorkflowRunFields failed: Error: PATCH workflow-run: 404` when a standalone
run attempts to save metadata without a server-reserved run record. This is an
upstream persistence limitation, separate from the review and GitHub check
publication. This repository does not suppress that warning or fix Pullfrog's
servers. Adding `workflow_dispatch` would change the owner-only execution policy
and is not part of this setup.

## Local checks

```bash
node scripts/pullfrog-command-check.test.mjs
bash scripts/dependabot-config-test.sh
bash scripts/ci-preflight.sh
```
