# Pullfrog: owner commands only

Pullfrog is an on-demand agent for this repository. Buildkite remains the only
CI and image publisher. The sole GitHub Actions workflow,
`.github/workflows/pullfrog.yml`, listens for newly created issue comments and
top-level PR comments. It does not listen to inline PR review comments, which
run GitHub workflows from a PR merge commit. It has no push, PR, schedule, or
`workflow_dispatch` trigger. The agent step runs only when GitHub identifies
the commenter and event actor as `jatmn` (account ID `12479882`) and the comment
starts with `@pullfrog` followed by an instruction. A public contributor's
comment, a quoted/passive mention, an edited comment, a workflow rerun, and a
bare `@pullfrog` do not run the agent. The workflow checks out the trusted
default branch and passes the comment URL to the agent, which must read the
current issue or PR.

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

## Local checks

```bash
node scripts/pullfrog-command-check.test.mjs
bash scripts/dependabot-config-test.sh
bash scripts/ci-preflight.sh
```
