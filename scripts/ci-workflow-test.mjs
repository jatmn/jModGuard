#!/usr/bin/env node
// Lightweight CI routing/trust contracts; actionlint validates YAML/expression syntax.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import { spawnSync } from "node:child_process";
const root = path.resolve(import.meta.dirname, "..");
const workflow = fs.readFileSync(path.join(root, ".github/workflows/ci.yml"), "utf8");
const publication = fs.readFileSync(path.join(root, ".github/workflows/publish.yml"), "utf8");
const combined = workflow + "\n" + publication.replace(/^  ci:\n[\s\S]*?(?=^  release:)/m, "");
const jobs = new Map([...combined.matchAll(/^  ([a-z][a-z-]*):\n([\s\S]*?)(?=^  [a-z][a-z-]*:\n|$(?![\s\S]))/gm)]
  .map(match => [match[1], match[2]]));
for (const name of ["plan", "checks", "ci", "release", "prepare-publication", "publish-build", "publish-sign"]) {
  assert.ok(jobs.has(name), "missing job: " + name);
}
const publishBuild = jobs.get("publish-build");
const publishSign = jobs.get("publish-sign");
assert.doesNotMatch(combined, /^  (?:pull_request_target|workflow_run):/m);
assert.doesNotMatch(workflow, /\bself-hosted\b|^    paths(?:-ignore)?:/m);
assert.match(workflow, /^permissions:\n  contents: read$/m);
for (const name of ["plan", "checks", "ci"]) {
  assert.doesNotMatch(jobs.get(name), /secrets\.|(?:packages|contents|pull-requests): write/);
}
assert.match(jobs.get("checks"), /matrix: \$\{\{ fromJSON\(needs\.plan\.outputs\.matrix\) \}\}/);
assert.match(jobs.get("checks"), /bash scripts\/ci-preflight\.sh "\$\{args\[@\]\}"/);
assert.match(jobs.get("checks"), /bash scripts\/ci-remote-database\.sh/);
assert.match(jobs.get("ci"), /needs: \[plan, checks\]/);
assert.match(jobs.get("ci"), /if: .*always\(\)/);
assert.match(jobs.get("ci"), /test "\$PLAN_RESULT" = success && test "\$CHECK_RESULT" = success/);
assert.match(jobs.get("prepare-publication"), /needs\.ci\.result == 'success'/);
assert.match(jobs.get("prepare-publication"), /github\.actor_id == '12479882'/);
assert.match(jobs.get("prepare-publication"), /github\.triggering_actor == 'jatmn'/);
assert.match(publishBuild, /needs\.prepare-publication\.result == 'success'/);
assert.match(publishBuild, /packages: write/);
assert.doesNotMatch(publishBuild, /contents: write/);
assert.doesNotMatch(publishBuild, /JMODGUARD_UPDATE_SIGNING_KEY/);
assert.doesNotMatch(publishBuild, /JMODGUARD_UPDATE_SIGNING_PASSWORD/);
assert.match(publishBuild, /upload-artifact@/);
assert.match(publishBuild, /publish-state-\$\{\{ matrix\.component \}\}/);
assert.doesNotMatch(publishBuild, /outputs:\s*\n\s+should_sign:/);
assert.match(publishBuild, /cosign-installer@/);
assert.match(publishSign, /needs: \[.*publish-build.*\]/);
assert.doesNotMatch(publishSign, /should_sign/);
assert.match(publishSign, /download-artifact@/);
assert.match(publishSign, /JMODGUARD_UPDATE_SIGNING_KEY/);
assert.match(publishSign, /JMODGUARD_PUBLISH_PHASE: sign/);
assert.doesNotMatch(publishSign, /JMODGUARD_PUBLISH_PHASE: build/);
assert.match(publishSign, /packages: write/);
assert.doesNotMatch(publishSign, /contents: write/);
assert.match(jobs.get("release"), /github\.event_name == 'push' && github\.ref == 'refs\/heads\/main'/);
assert.match(jobs.get("release"), /sha: \$\{\{ steps\.release\.outputs\.sha \}\}/);
assert.match(jobs.get("prepare-publication"), /JMODGUARD_RELEASE_SHA: \$\{\{ needs\.release\.outputs\.created == 'true' && needs\.release\.outputs\.sha \|\| '' \}\}/);
assert.match(jobs.get("prepare-publication"), /release_sha: \$\{\{ steps\.prepare\.outputs\.release_sha \}\}/);
assert.match(publishBuild, /JMODGUARD_RELEASE_SHA: \$\{\{ needs\.prepare-publication\.outputs\.release_sha \}\}/);
assert.match(publishSign, /JMODGUARD_RELEASE_SHA: \$\{\{ needs\.prepare-publication\.outputs\.release_sha \}\}/);
assert.doesNotMatch(combined, /gh pr merge|enablePullRequestAutoMerge|auto-merge/);
for (const match of combined.matchAll(/^\s+(?:- )?uses: (\S+)/gm)) {
  if (match[1].startsWith("./")) continue;
  assert.match(match[1], /@[0-9a-f]{40}$/, "action must be pinned to a commit");
}
for (const name of ["plan", "ci"]) {
  assert.match(jobs.get(name), /!github\.event\.repository\.private \|\| vars\.JMODGUARD_ENABLE_PRIVATE_CI == 'true'/);
}
assert.match(workflow, /^  pull_request:/m);
assert.match(workflow, /^    types: \[opened, synchronize, reopened\]$/m);
assert.match(workflow, /^  workflow_call:/m);
assert.doesNotMatch(workflow, /(?:contents|packages|pull-requests): write/);
assert.match(workflow, /cancel-in-progress: \$\{\{ github.event_name == 'pull_request' \}\}/);
assert.match(publication, /^  queue: max$/m);
assert.match(publication, /^  cancel-in-progress: false$/m);
assert.match(publication, /uses: \.\/\.github\/workflows\/ci\.yml/);
assert.doesNotMatch(publication, /secrets: inherit/);
// Evaluate the actual workflow guards and bindings using only their JS-compatible
// expression subset. This is a local workflow contract, not a hosted Actions run.
function expression(text, context) {
  return Function("github", "needs", "inputs", "always", `return (${text.replace(/needs\.([a-z-]+)/g, 'needs["$1"]')});`)(
    context.github, context.needs, context.inputs, () => true);
}
function condition(job, context) {
  const match = job.match(/^    if: (?:>-\n([\s\S]*?)(?=^    \S)|([^\n]+))/m);
  assert.ok(match, "job must have an explicit guard");
  return Boolean(expression((match[1] || match[2]).trim(), context));
}
function binding(job, key, context) {
  const match = job.match(new RegExp(`^ +${key}: \\$\\{\\{ (.+) \\}\\}$`, "m"));
  assert.ok(match, "missing workflow binding: " + key);
  return expression(match[1], context);
}
function run(command, args, options = {}) {
  const result = spawnSync(command, args, { encoding: "utf8", ...options });
  assert.equal(result.status, 0, result.stderr || result.stdout || String(result.error));
  assert.ifError(result.error);
  return result.stdout.trim();
}
const configure = jobs.get("checks").match(/^      - name: Configure runner\n([\s\S]*?)(?=^      - |$(?![\s\S]))/m)?.[1];
assert.ok(configure, "missing Configure runner step");
assert.match(configure, /^        run: bash scripts\/ci-setup\.sh "\$NEED_RUST"$/m);
const configureEnv = configure.match(/^        env:\n((?:          [^\n]+\n)+)/m)?.[1];
assert.ok(configureEnv, "Configure runner must receive its setup flags");
const compilerCache = jobs.get("checks").match(/^      - name: Cache Rust compiler outputs\n([\s\S]*?)(?=^      - |$(?![\s\S]))/m)?.[1];
assert.ok(compilerCache, "missing compiler cache setup");
assert.match(compilerCache, /version: v0\.18\.0/);
const compilerGuard = compilerCache.match(/^        if: (.+)$/m)?.[1];
assert.ok(compilerGuard);
const selectedChecks = jobs.get("checks").match(/^      - name: Run selected checks\n([\s\S]*?)(?=^      - |$(?![\s\S]))/m)?.[1];
assert.ok(selectedChecks);
assert.doesNotMatch(jobs.get("checks").slice(0, jobs.get("checks").indexOf("      - name: Run selected checks")), /RUSTC_WRAPPER:/);
assert.match(selectedChecks, /SCCACHE_GHA_ENABLED: 'true'/);
const cacheMode = selectedChecks.match(/SCCACHE_GHA_RW_MODE: \$\{\{ (.+) \}\}/)?.[1];
assert.ok(cacheMode);
for (const [event, ref, expected] of [
  ["pull_request", "refs/pull/353/merge", "READ_WRITE"],
  ["push", "refs/heads/main", "READ_WRITE"],
  ["push", "refs/heads/feature", "READ_ONLY"],
  ["schedule", "refs/heads/main", "READ_ONLY"],
  ["workflow_dispatch", "refs/heads/main", "READ_ONLY"],
]) {
  assert.equal(Function("github", `return (${cacheMode});`)({ event_name: event, ref }), expected);
}
for (const lane of ["rust-api", "rust-bot", "rust-core", "quick", "frontend", "dylint", "supply-chain", "remote-database"]) {
  const evaluate = (key, block = jobs.get("checks")) => {
    const match = block.match(new RegExp(`${key}: \\$\\{\\{ (.+) \\}\\}`));
    assert.ok(match, `missing ${key} workflow binding`);
    return Function("matrix", "startsWith", `return (${match[1]});`)({ lane }, (value, prefix) => value.startsWith(prefix));
  };
  const rustTests = ["rust-api", "rust-bot", "rust-core"].includes(lane);
  assert.equal(Function("matrix", "startsWith", `return (${compilerGuard});`)({ lane }, (value, prefix) => value.startsWith(prefix)), rustTests);
  assert.equal(evaluate("RUSTC_WRAPPER", selectedChecks), rustTests ? "sccache" : "");
  assert.equal(evaluate("NEED_NEXTEST", configureEnv), rustTests);
  assert.equal(evaluate("shared-key"), rustTests ? "rust" : lane, "retain the existing dependency cache");
  assert.equal(evaluate("cache-targets"), rustTests || ["dylint", "remote-database"].includes(lane));
}
const socket = fs.readFileSync(path.join(root, ".github/workflows/socket.yml"), "utf8");
assert.match(socket, /^  push:\n    branches: \[main\]$/m);
assert.match(socket, /JMODGUARD_DIFF_BASE: \$\{\{ github.event.pull_request.base.sha \|\| github.event.before \}\}/);
assert.match(socket, /JMODGUARD_DIFF_HEAD: \$\{\{ github.event.pull_request.head.sha \|\| github.sha \}\}/);
assert.match(socket, /scan="\$\(bash scripts\/select-checks.sh socket\)"/);
const socketSteps = [...socket.matchAll(/^      - ([\s\S]*?)(?=^      - |$(?![\s\S]))/gm)].map(match => match[1]);
for (const marker of ["uses: actions/setup-python@", "name: Install Socket CLI", "name: Run Socket Security Scan"]) {
  const step = socketSteps.find(step => step.startsWith(marker));
  assert.ok(step, `missing ${marker}`);
  assert.match(step, /^        if: steps.changes.outputs.scan == 'true'$/m);
}
const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "ci-workflow-"));
try {
  // Cold hosted setup must establish the driver precondition it advertises.
  const prefix = path.join(scratch, "cold-cache"), envFile = path.join(scratch, "env");
  // Redirect only the fixed hosted cache root to scratch; keep HOME unchanged.
  const setupScript = path.join(scratch, "ci-setup.sh");
  fs.writeFileSync(setupScript, fs.readFileSync(path.join(root, "scripts/ci-setup.sh"), "utf8")
    .replaceAll("$HOME/.cache/jmodguard-ci", prefix));
  run("bash", [setupScript, "false"], { cwd: root, env: {
    ...process.env, GITHUB_ACTIONS: "true", RUNNER_ENVIRONMENT: "github-hosted",
    RUNNER_TEMP: scratch, GITHUB_ENV: envFile,
  } });
  const setup = Object.fromEntries(fs.readFileSync(envFile, "utf8").trim().split("\n").map(line => {
    const split = line.indexOf("="); return [line.slice(0, split), line.slice(split + 1)];
  }));
  assert.equal(setup.DYLINT_DRIVER_PATH, path.join(prefix, "dylint-drivers"));
  assert.ok(fs.statSync(setup.DYLINT_DRIVER_PATH).isDirectory(), "cold driver cache must exist");
  fs.writeFileSync(path.join(setup.DYLINT_DRIVER_PATH, "probe"), "usable");

  // A failed downloader must have finite deadlines and stop setup before
  // publishing an unusable nextest path. The network failure is simulated.
  const bin = path.join(scratch, "bin"), curlArgs = path.join(scratch, "curl-args");
  fs.mkdirSync(bin);
  fs.writeFileSync(path.join(bin, "curl"), '#!/bin/sh\nprintf "%s\\n" "$@" > "$CURL_ARGS"\nexit 28\n', { mode: 0o755 });
  const githubPath = path.join(scratch, "github-path");
  const failedSetup = spawnSync("bash", [setupScript, "false"], { cwd: root, encoding: "utf8", env: {
    ...process.env, PATH: `${bin}:${process.env.PATH}`, CURL_ARGS: curlArgs,
    GITHUB_ACTIONS: "true", RUNNER_ENVIRONMENT: "github-hosted", NEED_NEXTEST: "true",
    RUNNER_TEMP: scratch, GITHUB_ENV: envFile, GITHUB_PATH: githubPath,
  } });
  assert.ifError(failedSetup.error);
  assert.equal(failedSetup.status, 28, "download failure must stop setup");
  const args = fs.readFileSync(curlArgs, "utf8").trim().split("\n");
  for (const option of ["--connect-timeout", "--max-time", "--retry-max-time"]) {
    const index = args.indexOf(option), seconds = Number(args[index + 1]);
    assert.ok(index >= 0 && seconds > 0 && seconds <= 120, `${option} must bound download delays`);
  }
  assert.ok(!fs.existsSync(githubPath), "failed setup must not publish nextest");

  const selectScan = socketSteps.find(step => step.startsWith("name: Select dependency scan"));
  const selectRun = selectScan?.match(/^        run: \|\n([\s\S]*)/m)?.[1];
  assert.ok(selectRun);
  fs.writeFileSync(path.join(bin, "bash"), '#!/bin/sh\nexit 19\n', { mode: 0o755 });
  const scanOutput = path.join(scratch, "scan-output");
  const failedSelector = spawnSync("/bin/bash", ["-e", "-c", selectRun], { cwd: root, env: {
    ...process.env, PATH: `${bin}:${process.env.PATH}`, GITHUB_OUTPUT: scanOutput,
  } });
  assert.ifError(failedSelector.error);
  assert.equal(failedSelector.status, 19, "selector failure must fail the scan job");
  assert.ok(!fs.existsSync(scanOutput), "failed selection must not publish a skip decision");

  // Real Git fixture: docs-only H selects contracts, then R adds Rust changes.
  // Resolve checkout/diff bindings from the actual reusable workflow, not a
  // parallel test implementation of its source-selection rules.
  const repo = path.join(scratch, "repo");
  fs.mkdirSync(repo);
  const git = (...args) => run("git", args, { cwd: repo });
  git("init", "-q", "-b", "main");
  git("config", "user.name", "CI fixture");
  git("config", "user.email", "ci@example.invalid");
  git("config", "commit.gpgsign", "false");
  git("commit", "-q", "--allow-empty", "-m", "base");
  const base = git("rev-parse", "HEAD");
  fs.writeFileSync(path.join(repo, "README.md"), "docs");
  git("add", "."); git("commit", "-qm", "docs");
  const head = git("rev-parse", "HEAD");
  fs.mkdirSync(path.join(repo, "crates/api/src"), { recursive: true });
  fs.writeFileSync(path.join(repo, "crates/api/src/lib.rs"), "// Changed release source\n");
  git("add", "."); git("commit", "-qm", "release code");
  const release = git("rev-parse", "HEAD");
  const context = {
    github: { sha: head, event_name: "push", ref: "refs/heads/main", repository: "jatmn/jModGuard", run_attempt: 1,
      event: { before: base, pull_request: { base: { sha: "" }, head: { sha: "" } } } },
    inputs: { source_sha: "" },
    needs: { ci: { result: "success" }, release: { result: "success" },
      "prepare-publication": { result: "success", outputs: { release_sha: release } },
      "release-ci": { result: "success" } },
  };
  function matrix() {
    git("checkout", "-q", "--detach", binding(jobs.get("plan"), "ref", context));
    return JSON.parse(run("bash", [path.join(root, "scripts/select-checks.sh"), "matrix"], { cwd: repo, env: {
      ...process.env, GITHUB_EVENT_NAME: "push", GITHUB_REF_TYPE: "branch",
      JMODGUARD_DIFF_BASE: binding(jobs.get("plan"), "JMODGUARD_DIFF_BASE", context),
      JMODGUARD_DIFF_HEAD: binding(jobs.get("plan"), "JMODGUARD_DIFF_HEAD", context),
    } }));
  }
  context.needs["release-ci"].result = "failure";
  assert.equal(condition(publishBuild, context), false, "failed differing source must block publication");
  context.needs["release-ci"].result = "success";
  assert.deepEqual(matrix().include.map(item => item.groups), ["contracts"]);
  assert.ok(jobs.has("release-ci"), "differing release source requires validation");
  assert.match(jobs.get("release-ci"), /uses: \.\/\.github\/workflows\/ci\.yml/);
  assert.match(publishBuild, /needs: \[.*release-ci.*\]/);
  assert.match(publishSign, /needs: \[.*release-ci.*\]/);
  for (const source of [release, base]) {
    context.needs["prepare-publication"].outputs.release_sha = source;
    assert.ok(condition(jobs.get("release-ci"), context));
    context.inputs.source_sha = binding(jobs.get("release-ci"), "source_sha", context);
    assert.equal(context.inputs.source_sha, source);
    assert.equal(binding(jobs.get("checks"), "ref", context), source);
    assert.equal(binding(jobs.get("checks"), "JMODGUARD_DIFF_BASE", context), "");
    assert.equal(binding(jobs.get("checks"), "JMODGUARD_DIFF_HEAD", context), source);
    assert.equal(matrix().include.length, 8, "a different source requires all eight lanes");
    assert.equal(git("rev-parse", "HEAD"), source);
    // The provider gate result is simulated; the production job guard must
    // withhold the entire bot/API/WebUI matrix for every unsuccessful R check.
    for (const attempt of [1, 2]) {
      context.github.run_attempt = attempt;
      for (const result of ["failure", "cancelled", "skipped", "success"]) {
        context.needs["release-ci"].result = result;
        assert.equal(condition(publishBuild, context), result === "success");
        context.needs["publish-build"] = { result: result, outputs: {} };
        assert.equal(
          condition(publishSign, context),
          result === "success",
          "signing must follow a successful build matrix",
        );
      }
    }
  }
  for (const source of ["", head]) {
    context.needs["prepare-publication"].outputs.release_sha = source;
    assert.equal(condition(jobs.get("release-ci"), context), false);
    context.needs["release-ci"].result = "skipped";
    assert.ok(condition(publishBuild, context), "same-source and manual routes keep original checks");
    context.needs["publish-build"] = { result: "failure", outputs: {} };
    assert.equal(condition(publishSign, context), false, "failed build must not run signing");
    context.needs["publish-build"].result = "success";
    assert.ok(condition(publishSign, context), "successful build must allow per-component signing");
  }

  // Saved main and release plans both skip release creation before restoration,
  // even if Release Please could create a newer release from live main.
  for (const source of ["", release]) {
    context.needs["prepare-publication"].outputs.release_sha = source;
    for (const attempt of [1, 2, 3]) {
      context.github.run_attempt = attempt;
      assert.equal(condition(jobs.get("release"), context), attempt === 1, "reruns must not consume another release");
    }
  }
} finally {
  fs.rmSync(scratch, { recursive: true, force: true });
}
console.log("ci-workflow-test: ok");
