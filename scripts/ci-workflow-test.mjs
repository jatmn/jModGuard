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
for (const name of ["plan", "checks", "ci", "release", "prepare-publication", "publish-build", "publish-promote"]) {
  assert.ok(jobs.has(name), "missing job: " + name);
}
const publishBuild = jobs.get("publish-build");
const publishPromote = jobs.get("publish-promote");
assert.equal(jobs.has("publish-sign"), false, "promotion must not stay in the per-component sign matrix");
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
assert.match(publishPromote, /needs: \[.*publish-build.*\]/);
assert.doesNotMatch(publishPromote, /should_sign/);
assert.match(publishPromote, /download-artifact@/);
assert.match(publishPromote, /JMODGUARD_UPDATE_SIGNING_KEY/);
assert.match(publishPromote, /JMODGUARD_PUBLISH_PHASE: sign/);
assert.doesNotMatch(publishPromote, /JMODGUARD_PUBLISH_PHASE: build/);
assert.match(publishPromote, /packages: write/);
assert.doesNotMatch(publishPromote, /contents: write/);
assert.doesNotMatch(publishPromote, /continue-on-error|fail-fast:|matrix:|^\s+strategy:/m);
assert.match(publishPromote, /needs\.publish-build\.result == 'success'/);
assert.doesNotMatch(publishBuild, /JMODGUARD_PUBLISH_PHASE: sign/);
assert.match(jobs.get("release"), /github\.event_name == 'push' && github\.ref == 'refs\/heads\/main'/);
assert.match(jobs.get("release"), /sha: \$\{\{ steps\.release\.outputs\.sha \}\}/);
assert.match(jobs.get("prepare-publication"), /JMODGUARD_RELEASE_SHA: \$\{\{ needs\.release\.outputs\.created == 'true' && needs\.release\.outputs\.sha \|\| '' \}\}/);
assert.match(jobs.get("prepare-publication"), /release_sha: \$\{\{ steps\.prepare\.outputs\.release_sha \}\}/);
assert.match(publishBuild, /JMODGUARD_RELEASE_SHA: \$\{\{ needs\.prepare-publication\.outputs\.release_sha \}\}/);
assert.match(publishPromote, /JMODGUARD_RELEASE_SHA: \$\{\{ needs\.prepare-publication\.outputs\.release_sha \}\}/);
function stepScript(job, stepName) {
  const block = job.match(new RegExp(`^      - name: ${stepName}\\n([\\s\\S]*?)(?=^      - |$(?![\\s\\S]))`, "m"))?.[1];
  assert.ok(block, "missing step: " + stepName);
  const body = block.match(/^        run: \|\n([\s\S]*)/m)?.[1];
  assert.ok(body, "missing run script: " + stepName);
  return body.replace(/^ {10}/gm, "");
}
const matrixComponents = publishBuild.match(/component: \[([^\]]+)\]/)[1].split(",").map(item => item.trim());
const requireScript = stepScript(publishPromote, "Require every component build");
const promoteScript = stepScript(publishPromote, "Sign and promote every component");
const requiredComponents = requireScript.match(/for component in ([^;\n]+)/)[1].trim().split(/\s+/);
const promotedComponents = promoteScript.match(/for component in ([^;\n]+)/)[1].trim().split(/\s+/);
assert.deepEqual(matrixComponents, ["bot", "api", "webui"]);
assert.deepEqual(requiredComponents, matrixComponents, "promotion must require every matrix build");
assert.deepEqual(promotedComponents, matrixComponents, "promotion must cover every matrix build");
const buildSteps = [...publishBuild.matchAll(/^      - ([\s\S]*?)(?=^      - |$(?![\s\S]))/gm)].map(match => match[1]);
const recordStep = buildSteps.find(step => step.startsWith("name: Record successful component build"));
const okStep = buildSteps.find(step => step.includes("publish-build-ok-${{ matrix.component }}"));
assert.ok(recordStep && okStep);
assert.ok(publishBuild.indexOf("JMODGUARD_PUBLISH_PHASE: build") < publishBuild.indexOf(recordStep));
assert.ok(publishBuild.indexOf(recordStep) < publishBuild.indexOf(okStep));
assert.doesNotMatch(recordStep, /\bif:/);
assert.doesNotMatch(okStep, /if:\s*always\(\)/);
const promoteAt = publishPromote.indexOf("JMODGUARD_PUBLISH_PHASE: sign");
const beforePromote = publishPromote.slice(0, promoteAt);
const promoteSteps = [...publishPromote.matchAll(/^      - ([\s\S]*?)(?=^      - |$(?![\s\S]))/gm)].map(match => match[1]);
const buildOkDownload = promoteSteps.find(step => /^          pattern: publish-build-ok-\*$/m.test(step));
assert.ok(buildOkDownload, "promotion must download the build-success artifacts");
assert.match(buildOkDownload, /^uses: actions\/download-artifact@[0-9a-f]{40}/);
assert.match(buildOkDownload, /^          path: \$\{\{ runner.temp \}\}\/publish-build-ok$/m);
assert.doesNotMatch(buildOkDownload, /\bif:|merge-multiple:\s*true/);
assert.ok(beforePromote.indexOf(buildOkDownload) >= 0);
assert.ok(beforePromote.indexOf(buildOkDownload) < beforePromote.indexOf("Require every component build"));
assert.ok(beforePromote.includes("Require every component build"));
function promotionGate(present) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "publish-ok-"));
  try {
    for (const component of present) {
      const marker = path.join(dir, "publish-build-ok", `publish-build-ok-${component}`);
      fs.mkdirSync(marker, { recursive: true });
      fs.writeFileSync(path.join(marker, "ok"), `${component}\n`);
    }
    return spawnSync("bash", ["-c", requireScript], {
      encoding: "utf8", env: { ...process.env, RUNNER_TEMP: dir },
    }).status;
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
}
assert.equal(promotionGate(matrixComponents), 0, "every component build must allow the promotion gate");
for (const missing of matrixComponents) {
  assert.notEqual(promotionGate(matrixComponents.filter(component => component !== missing)), 0,
    `missing ${missing} build must block promotion`);
}
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
// actions/cache never re-saves a primary-key hit, so the pinned tool cache key
// must hash every file that decides what the cache holds, including each
// supply-chain tool version ensure_cargo_tool installs.
const toolCache = jobs.get("checks").match(/^      - name: Cache pinned lint and audit tools\n([\s\S]*?)(?=^      - |$(?![\s\S]))/m)?.[1];
assert.ok(toolCache, "missing pinned tool cache");
const toolKeyFiles = [...(toolCache.match(/hashFiles\(([^)]*)\)/)?.[1] ?? "").matchAll(/'([^']+)'/g)].map(match => match[1]);
for (const file of ["rust-toolchain.toml", "dylint.toml", "scripts/dylint.sh", "scripts/ci-dylint-libs.sh", "scripts/ci-ensure-cargo-tool.sh"]) {
  assert.ok(toolKeyFiles.includes(file), `pinned tool cache key must hash ${file}`);
}
const ensureHelper = fs.readFileSync(path.join(root, "scripts/ci-ensure-cargo-tool.sh"), "utf8");
const toolVersions = [...fs.readFileSync(path.join(root, "scripts/ci-preflight.sh"), "utf8")
  .matchAll(/^\s*ensure_cargo_tool \S+ \S+ (\S+)$/gm)].map(match => match[1]);
assert.ok(toolVersions.length > 0, "missing ensure_cargo_tool calls");
for (const version of toolVersions) {
  const name = version.match(/^"\$([a-z_]+)"$/)?.[1];
  assert.ok(name, `ensure_cargo_tool version ${version} must be pinned in scripts/ci-ensure-cargo-tool.sh`);
  assert.match(ensureHelper, new RegExp(`^${name}=\\S+$`, "m"), `${name} must be pinned in scripts/ci-ensure-cargo-tool.sh`);
}
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
for (const file of fs.readdirSync(path.join(root, ".github/workflows"))) {
  const text = fs.readFileSync(path.join(root, ".github/workflows", file), "utf8");
  assert.doesNotMatch(text, /runs-on: ubuntu-latest/, file + " must pin its runner image");
}
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
  assert.match(publishPromote, /needs: \[.*release-ci.*\]/);
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
          condition(publishPromote, context),
          result === "success",
          "promotion must follow a successful build matrix",
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
    assert.equal(condition(publishPromote, context), false, "failed build must not promote");
    context.needs["publish-build"].result = "success";
    assert.ok(condition(publishPromote, context), "successful builds must allow promotion");
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
const publisher = fs.readFileSync(path.join(root, "scripts/publish-image.sh"), "utf8");
const signFn = publisher.slice(
  publisher.indexOf("publish_component_sign() {"),
  publisher.indexOf("\npublish_component_phase()"),
);
assert.match(signFn, /resolve_promotion_target/);
assert.match(signFn, /JMODGUARD_DEFER_PROMOTION/);
assert.doesNotMatch(signFn, /promote_image_tag|registry_promote_tag/);
assert.ok(signFn.indexOf("resolve_promotion_target") < signFn.indexOf("JMODGUARD_DEFER_PROMOTION"));
assert.ok(signFn.indexOf("JMODGUARD_DEFER_PROMOTION") < signFn.indexOf("commit_queued_promotions"));
assert.match(promoteScript, /export JMODGUARD_DEFER_PROMOTION=1/);
assert.match(promoteScript, /finish_deferred_promotions/);
assert.doesNotMatch(promoteScript, /bash scripts\/publish-image\.sh/);
const deferAt = promoteScript.indexOf("export JMODGUARD_DEFER_PROMOTION=1");
const loopAt = promoteScript.indexOf("for component in");
const finishAt = promoteScript.indexOf("finish_deferred_promotions");
assert.ok(deferAt < loopAt && loopAt < finishAt, "promotion must resolve every component before it commits tags");

const oldBot = "sha256:" + "a".repeat(64);
const oldWeb = "sha256:" + "b".repeat(64);
const nextDigest = "sha256:" + "c".repeat(64);
const botRef = "ghcr.io/jatmn/jmodguard-bot:" + "a".repeat(40);
const apiRef = "ghcr.io/jatmn/jmodguard-api:" + "b".repeat(40);
const webRef = "ghcr.io/jatmn/jmodguard-webui:latest";
const atomicDir = fs.mkdtempSync(path.join(os.tmpdir(), "promote-atomic-"));
const trace = path.join(atomicDir, "trace");
const atomic = [
  "set -euo pipefail",
  "export JMODGUARD_PUBLISH_IMAGE_LIB=1",
  "source scripts/publish-image.sh",
  "declare -A CUR=()",
  'CUR["$BOT_REF"]="$OLD_BOT"',
  'CUR["$WEB_REF"]="$OLD_WEB"',
  "PHASE=forward",
  "inspect_digest() {",
  "  printf 'inspect %s\\n' \"$1\" >>\"$TRACE\"",
  "  if [[ -n \"${CUR[$1]+x}\" ]]; then",
  "    printf '%s\\t10\\n' \"${CUR[$1]}\"",
  "    return 0",
  "  fi",
  "  return 2",
  "}",
  "registry_promote_tag() {",
  "  local tag=\"\" digest=\"\" prev=\"\" arg",
  "  for arg in \"$@\"; do",
  "    if [[ \"$prev\" == --tag ]]; then tag=\"$arg\"; fi",
  "    if [[ \"$arg\" == *@sha256:* ]]; then digest=\"${arg#*@}\"; fi",
  "    prev=\"$arg\"",
  "  done",
  "  if [[ \"$PHASE\" == rollback ]]; then",
  "    printf 'restore %s %s\\n' \"$tag\" \"$digest\" >>\"$TRACE\"",
  "    CUR[\"$tag\"]=\"$digest\"",
  "    return 0",
  "  fi",
  "  if [[ \"$tag\" == \"$WEB_REF\" ]]; then",
  "    printf 'promote-fail %s\\n' \"$tag\" >>\"$TRACE\"",
  "    PHASE=rollback",
  "    return 1",
  "  fi",
  "  printf 'promote %s %s\\n' \"$tag\" \"$digest\" >>\"$TRACE\"",
  "  CUR[\"$tag\"]=\"$digest\"",
  "  return 0",
  "}",
  "registry_delete_tag() {",
  "  printf 'delete %s\\n' \"$1\" >>\"$TRACE\"",
  "  unset \"CUR[$1]\"",
  "  return 0",
  "}",
  "resolve_promotion_target \"${BOT_REF%:*}\" \"${BOT_REF##*:}\" \"$NEW\"",
  "resolve_promotion_target \"${API_REF%:*}\" \"${API_REF##*:}\" \"$NEW\"",
  "resolve_promotion_target \"${WEB_REF%:*}\" \"${WEB_REF##*:}\" \"$NEW\"",
  "finish_deferred_promotions",
].join("\n");
let atomicResult;
try {
  atomicResult = spawnSync("bash", ["-c", atomic], {
    cwd: root,
    encoding: "utf8",
    env: {
      ...process.env,
      TRACE: trace,
      BOT_REF: botRef,
      API_REF: apiRef,
      WEB_REF: webRef,
      OLD_BOT: oldBot,
      OLD_WEB: oldWeb,
      NEW: nextDigest,
    },
  });
  assert.notEqual(atomicResult.status, 0, atomicResult.stderr || atomicResult.stdout);
  const lines = fs.readFileSync(trace, "utf8").trim().split("\n");
  const firstWrite = lines.findIndex(line => /^(promote |promote-fail |restore |delete )/.test(line));
  assert.equal(firstWrite, 3, "resolving tags mutated the registry before every image was checked: " + lines.join(" | "));
  assert.ok(lines.slice(0, 3).every(line => line.startsWith("inspect ")));
  assert.ok(lines.includes("promote " + botRef + " " + nextDigest));
  assert.ok(lines.includes("promote " + apiRef + " " + nextDigest));
  assert.ok(lines.includes("promote-fail " + webRef));
  assert.equal(lines.includes("restore " + webRef + " " + oldWeb), false,
    "a failed write that left the original digest must not rewrite that tag");
  const apiDelete = lines.indexOf("delete " + apiRef);
  const botRestore = lines.indexOf("restore " + botRef + " " + oldBot);
  assert.ok(apiDelete !== -1 && botRestore !== -1, lines.join(" | "));
  assert.ok(apiDelete < botRestore, "rollback must undo the newest changed tag first");
} finally {
  fs.rmSync(atomicDir, { recursive: true, force: true });
}

function runPromotionScript(script, env = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "promote-inspect-"));
  const trace = path.join(dir, "trace");
  const result = spawnSync("bash", ["-c", script], {
    cwd: root,
    encoding: "utf8",
    env: { ...process.env, TRACE: trace, ...env, RUNNER_TEMP: dir },
  });
  const traceText = fs.existsSync(trace) ? fs.readFileSync(trace, "utf8") : "";
  fs.rmSync(dir, { recursive: true, force: true });
  return { status: result.status, stdout: result.stdout, stderr: result.stderr, trace: traceText };
}

// Run the actual workflow shell and publisher entry points. Only external signing,
// request metadata and registry I/O are simulated; queue/commit/rollback stay real.
function runPromoteJob(mode, retry = false) {
  return runPromotionScript(`
set -euo pipefail
export JMODGUARD_PUBLISH_PHASE=sign
source() {
  builtin source "$@"
  checked_request() { printf 'main\\t\\t%s\\n' "$SHA"; }
  read_plan() { printf '%s\\t\\n' "$SHA"; }
  read_publish_handoff() { return 1; }
  read_publish_state() { return 1; }
  require_signing_secrets() { :; }
  validate_signing_key_pair() { :; }
  capture_signing_secrets() { :; }
  ensure_signing_secrets_captured() { :; }
  persist_signing_secrets_to_files() { :; }
  redact_tokens() { :; }
  run_docker() { cat >/dev/null; }
  sign_published_digest() {
    printf 'sign %s\\n' "$1" >>"$TRACE"
    [[ "$MODE" != sign-fail || "$1" != webui ]] || return 1
  }
  clear_publish_state() { printf 'clear %s\\n' "$1" >>"$TRACE"; }
  inspect_digest() {
    printf 'inspect %s\\n' "$1" >>"$TRACE"
    if [[ "$1" == *:_publish-* ]]; then printf '%s\\t10\\n' "$NEW"; return; fi
    local state="$RUNNER_TEMP/\${1##*/}"
    if [[ "$MODE" == newer-absent && "$1" == *jmodguard-bot:"$SHA" && ! -f "$state" ]]; then return 2; fi
    if [[ -f "$state" ]]; then
      printf '%s\\t10\\n' "$(cat "$state")"
    else
      printf '%s\\t10\\n' "$OLD"
    fi
  }
  registry_delete_tag() { printf 'delete %s\\n' "$1" >>"$TRACE"; }
  registry_promote_tag() {
    local ref="$3" digest="\${4#*@}"
    printf 'write %s %s\\n' "$ref" "$digest" >>"$TRACE"
    printf '%s\\n' "$digest" >"$RUNNER_TEMP/\${ref##*/}"
    if [[ "$digest" == "$NEW" && "$ref" == *jmodguard-api:* ]]; then
      case "$MODE" in
        INT|TERM) kill -s "$MODE" "$BASHPID" ;;
        exit) exit 7 ;;
        newer|newer-absent)
          printf '%s\\n' "$OTHER" >"$RUNNER_TEMP/jmodguard-bot:$SHA"
          return 1 ;;
      esac
    fi
  }
}
${retry ? `
set +e
(
${promoteScript}
)
status=$?
set -e
printf 'interrupted %s\\n' "$status" >>"$TRACE"
[[ "$status" == 143 ]]
printf 'retry\\n' >>"$TRACE"
MODE=success
` : ""}
${promoteScript}
`, {
    // Exercise every workflow path without the optional ambient temp variable.
    TMPDIR: undefined,
    MODE: mode, SHA: "a".repeat(40), OLD: oldBot, NEW: nextDigest, OTHER: oldWeb,
    GHCR_USERNAME: "test-publisher", GHCR_TOKEN: "test-token",
  });
}
for (const mode of ["success", "sign-fail", "INT", "TERM", "exit", "newer", "newer-absent"]) {
  const result = runPromoteJob(mode);
  const lines = result.trace.trim().split("\n");
  const writes = lines.filter(line => line.startsWith("write "));
  if (mode === "sign-fail") {
    assert.notEqual(result.status, 0, result.stderr);
    assert.deepEqual(writes, [], "a later signing failure must leave every tag unchanged");
    continue;
  }
  const firstWrite = lines.findIndex(line => line.startsWith("write "));
  for (const component of matrixComponents) {
    const signedAt = lines.indexOf(`sign ${component}`);
    const resolvedAt = lines.indexOf(`inspect ghcr.io/jatmn/jmodguard-${component}:latest`);
    assert.ok(signedAt >= 0 && signedAt < firstWrite, result.trace);
    assert.ok(resolvedAt >= 0 && resolvedAt < firstWrite, result.trace);
  }
  if (mode === "success") {
    assert.equal(result.status, 0, result.stderr);
    assert.deepEqual(writes, matrixComponents.flatMap(component => ["a".repeat(40), "latest"]
      .map(tag => `write ghcr.io/jatmn/jmodguard-${component}:${tag} ${nextDigest}`)));
    assert.equal(lines.filter(line => line.startsWith("clear ")).length, 3);
  } else {
    assert.notEqual(result.status, 0, result.stderr);
    const restored = writes.filter(line => line.endsWith(oldBot));
    const expected = [
      `write ghcr.io/jatmn/jmodguard-api:${"a".repeat(40)} ${oldBot}`,
      `write ghcr.io/jatmn/jmodguard-bot:latest ${oldBot}`,
      `write ghcr.io/jatmn/jmodguard-bot:${"a".repeat(40)} ${oldBot}`,
    ];
    assert.deepEqual(restored, mode.startsWith("newer") ? expected.slice(0, 2) : expected, result.trace);
    assert.equal(lines.some(line => line.startsWith("clear ")), false);
    if (mode.startsWith("newer")) {
      assert.match(result.stderr, /rollback skipped .* changed since promotion/);
      assert.equal(lines.some(line => line.startsWith("delete ")), false,
        "rollback must not delete a newer tag even when the original tag was absent");
    }
  }
}

// Retry the interrupted three-component workflow in a fresh shell with the same
// simulated registry. The production queue and rollback traps run on each attempt.
const retryResult = runPromoteJob("TERM", true);
assert.equal(retryResult.status, 0, retryResult.stderr);
const retryLines = retryResult.trace.trim().split("\n");
const retryAt = retryLines.indexOf("retry");
assert.ok(retryAt > 0, retryResult.trace);
const interruptedLines = retryLines.slice(0, retryAt);
assert.equal(interruptedLines.at(-1), "interrupted 143");
const retryRefs = matrixComponents.flatMap(component => ["a".repeat(40), "latest"]
  .map(tag => `${botRef.split("-bot:")[0]}-${component}:${tag}`));
const changedRefs = retryRefs.slice(0, 3);
assert.deepEqual(interruptedLines.filter(line => line.startsWith("write ")), [
  ...changedRefs.map(ref => `write ${ref} ${nextDigest}`),
  ...changedRefs.toReversed().map(ref => `write ${ref} ${oldBot}`),
]);
assert.equal(interruptedLines.some(line => line.startsWith("clear ")), false,
  "interrupted finish must retain every component's build state");
const retriedLines = retryLines.slice(retryAt + 1);
const retryWritesAt = retriedLines.findIndex(line => line.startsWith("write "));
assert.ok(retryWritesAt >= 0, retryResult.trace);
assert.deepEqual(retriedLines.slice(0, retryWritesAt).filter(line => line.startsWith("sign ")),
  matrixComponents.map(component => `sign ${component}`), "retry must sign all three components before promotion");
const retryWrites = retryRefs.map(ref => `write ${ref} ${nextDigest}`);
assert.deepEqual(retriedLines.filter(line => line.startsWith("write ")), retryWrites);
const lastRetryWriteAt = retriedLines.indexOf(retryWrites.at(-1));
assert.equal(retriedLines.slice(0, lastRetryWriteAt).some(line => line.startsWith("clear ")), false,
  "retry must retain build state until every tag is promoted");
assert.deepEqual(retriedLines.slice(lastRetryWriteAt + 1).filter(line => line.startsWith("clear ")),
  matrixComponents.map(component => `clear ${component}`));

const keptDigest = "sha256:" + "a".repeat(64);
const nextKept = "sha256:" + "c".repeat(64);
const inspectError = runPromotionScript([
  "set -euo pipefail",
  "export JMODGUARD_PUBLISH_IMAGE_LIB=1",
  "source scripts/publish-image.sh",
  "inspect_digest() { return 1; }",
  "registry_promote_tag() { printf 'promote\\n' >>\"$TRACE\"; return 1; }",
  "registry_delete_tag() { printf 'delete %s\\n' \"$1\" >>\"$TRACE\"; return 0; }",
  "resolve_promotion_target ghcr.io/jatmn/jmodguard-bot latest \"$NEW\"",
  "printf 'queue %s\\n' \"${PROMOTION_QUEUE[0]}\" >>\"$TRACE\"",
  "finish_deferred_promotions",
].join("\n"), { NEW: nextKept });
assert.notEqual(inspectError.status, 0, inspectError.stdout);
assert.match(inspectError.stderr, /cannot inspect ghcr\.io\/jatmn\/jmodguard-bot:latest before promotion/);
assert.equal(inspectError.trace, "", "inspection failure must not promote or delete: " + inspectError.trace);
const confirmedAbsent = runPromotionScript([
  "set -euo pipefail",
  "export JMODGUARD_PUBLISH_IMAGE_LIB=1",
  "source scripts/publish-image.sh",
  "docker() { printf 'ERROR: ghcr.io/jatmn/jmodguard-bot:latest: not found\\n' >&2; return 1; }",
  "registry_promote_tag() { printf 'promote\\n' >>\"$TRACE\"; return 0; }",
  "registry_delete_tag() { printf 'delete %s\\n' \"$1\" >>\"$TRACE\"; return 0; }",
  "resolve_promotion_target ghcr.io/jatmn/jmodguard-bot latest \"$NEW\"",
  "printf 'queue %s\\n' \"${PROMOTION_QUEUE[0]}\" >>\"$TRACE\"",
].join("\n"), { NEW: nextKept });
assert.equal(confirmedAbsent.status, 0, confirmedAbsent.stderr);
assert.equal(confirmedAbsent.trace, `queue ghcr.io/jatmn/jmodguard-bot|latest|${nextKept}|\n`);
assert.doesNotMatch(confirmedAbsent.trace, /delete |promote/);
const transportError = runPromotionScript([
  "set -euo pipefail",
  "export JMODGUARD_PUBLISH_IMAGE_LIB=1",
  "source scripts/publish-image.sh",
  "docker() { printf 'ERROR: failed to do request: dial tcp: lookup ghcr.io: no such host\\n' >&2; return 1; }",
  "registry_promote_tag() { printf 'promote\\n' >>\"$TRACE\"; return 0; }",
  "registry_delete_tag() { printf 'delete %s\\n' \"$1\" >>\"$TRACE\"; return 0; }",
  "resolve_promotion_target ghcr.io/jatmn/jmodguard-bot latest \"$NEW\"",
  "printf 'queue %s\\n' \"${PROMOTION_QUEUE[0]}\" >>\"$TRACE\"",
  "finish_deferred_promotions",
].join("\n"), { NEW: nextKept });
assert.notEqual(transportError.status, 0, transportError.stdout);
assert.match(transportError.stderr, /cannot inspect ghcr\.io\/jatmn\/jmodguard-bot:latest before promotion/);
assert.equal(transportError.trace, "", "a transport error must not promote or delete: " + transportError.trace);
const recordedDigest = runPromotionScript([
  "set -euo pipefail",
  "export JMODGUARD_PUBLISH_IMAGE_LIB=1",
  "source scripts/publish-image.sh",
  "docker() { printf '{\"digest\":\"%s\",\"size\":10}\\n' \"$OLD\"; }",
  "resolve_promotion_target ghcr.io/jatmn/jmodguard-bot latest \"$NEW\"",
  "printf 'queue %s\\n' \"${PROMOTION_QUEUE[0]}\" >>\"$TRACE\"",
].join("\n"), { NEW: nextKept, OLD: keptDigest });
assert.equal(recordedDigest.status, 0, recordedDigest.stderr);
assert.equal(recordedDigest.trace, `queue ghcr.io/jatmn/jmodguard-bot|latest|${nextKept}|${keptDigest}\n`);

function packageVersion(id, tags, name = `sha256:${id.toString(16).padStart(64, "0")}`) {
  const version = { id, metadata: { package_type: "container", container: { tags } } };
  if (name) version.name = name;
  return version;
}
function runRegistryDelete(tag, responses, options = {}) {
  const deleteStatus = Number.isInteger(options.deleteStatus) ? options.deleteStatus : 0;
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "registry-delete-"));
  const bin = path.join(dir, "bin");
  const ghLog = path.join(dir, "gh.log");
  const dockerLog = path.join(dir, "docker.log");
  const timeoutLog = path.join(dir, "timeout.log");
  const pagesLog = path.join(dir, "pages.log");
  const statusFile = path.join(dir, "status");
  const responsesFile = path.join(dir, "responses.json");
  const stateFile = path.join(dir, "state");
  fs.mkdirSync(bin);
  fs.writeFileSync(responsesFile, JSON.stringify(responses));
  const timeoutCommand = spawnSync("bash", ["-c", "command -v timeout"], { encoding: "utf8" });
  assert.equal(timeoutCommand.status, 0, timeoutCommand.stderr);
  fs.symlinkSync(timeoutCommand.stdout.trim(), path.join(bin, "real-timeout"));
  // Record production arguments in every case; shorten only the stalling cases.
  fs.writeFileSync(path.join(bin, "timeout"), `#!/bin/bash
set -euo pipefail
printf '%s\\n' "$*" >>${JSON.stringify(timeoutLog)}
${options.stallFrom ? 'shift 2\nexec real-timeout --kill-after=1s 1s "$@"' : 'exec real-timeout "$@"'}
`);
  fs.chmodSync(path.join(bin, "timeout"), 0o755);
  fs.writeFileSync(path.join(dir, "emit-pages.mjs"), [
    'import { appendFileSync, readFileSync } from "node:fs";',
    'import { setTimeout } from "node:timers/promises";',
    'const responses = JSON.parse(readFileSync(process.argv[2], "utf8"));',
    'const index = Number(process.argv[3]);',
    'const emitAll = process.argv[4] === "1";',
    'const entry = responses[Math.min(index, responses.length) - 1];',
    'const multi = Array.isArray(entry) && entry.length > 0 && entry.every((item) => Array.isArray(item));',
    'const pages = multi ? entry : [entry];',
    'const stall = Number(process.argv[5]) > 0 && index >= Number(process.argv[5]);',
    'for (const [pageIndex, page] of (emitAll ? pages : pages.slice(0, 1)).entries()) {',
    '  process.stdout.write(JSON.stringify(page) + "\\n");',
    '  appendFileSync(process.argv[6], `${index}:${pageIndex + 1}\\n`);',
    '  if (stall && emitAll && pageIndex === 0 && pages.length > 1) await setTimeout(5000);',
    '}',
    '',
  ].join("\n"));
  fs.writeFileSync(path.join(bin, "gh"), `#!/bin/bash
set -euo pipefail
printf 'token=%s\\n' "\${GH_TOKEN-}" >>${JSON.stringify(ghLog)}
printf 'args=%s\\n' "$*" >>${JSON.stringify(ghLog)}
method=GET
req_path=""
prev=""
has_paginate=0
has_per_page=0
for arg in "$@"; do
  if [[ "\$prev" == --method ]]; then method="\$arg"; fi
  if [[ "\$arg" == --paginate ]]; then has_paginate=1; fi
  if [[ "\$arg" =~ (^|[?&])per_page=100(\$|&) ]]; then has_per_page=1; fi
  case "\$arg" in
    /users/*|/user/*|/orgs/*) req_path="\$arg" ;;
  esac
  prev="\$arg"
done
printf 'method=%s path=%s\\n' "\$method" "\$req_path" >>${JSON.stringify(ghLog)}
if [[ "\$method" == DELETE ]]; then
  exit ${deleteStatus}
fi
n=0
if [[ -f ${JSON.stringify(stateFile)} ]]; then n=\$(cat ${JSON.stringify(stateFile)}); fi
n=\$((n + 1))
printf '%s' "\$n" >${JSON.stringify(stateFile)}
emit_all=0
if [[ "\$has_paginate" == 1 && "\$has_per_page" == 1 ]]; then emit_all=1; fi
node ${JSON.stringify(path.join(dir, "emit-pages.mjs"))} ${JSON.stringify(responsesFile)} "\$n" "\$emit_all" ${options.stallFrom || 0} ${JSON.stringify(pagesLog)}
`);
  fs.writeFileSync(path.join(bin, "docker"), `#!/bin/bash
printf '%s\\n' "\$*" >>${JSON.stringify(dockerLog)}
if [[ "$1 $2" == "buildx build" ]]; then exit ${options.pushStatus || 0}; fi
exit 0
`);
  fs.chmodSync(path.join(bin, "gh"), 0o755);
  fs.chmodSync(path.join(bin, "docker"), 0o755);
  const result = spawnSync("bash", ["-c", [
    "set -euo pipefail",
    "export JMODGUARD_PUBLISH_IMAGE_LIB=1",
    "source scripts/publish-image.sh",
    "set +e",
    "registry_delete_tag \"$IMAGE_TAG\"",
    "printf '%s\\n' \"$?\" >\"$STATUS\"",
  ].join("\n")], {
    cwd: root,
    encoding: "utf8",
    env: {
      ...process.env,
      PATH: `${bin}${path.delimiter}${process.env.PATH}`,
      IMAGE_TAG: `ghcr.io/jatmn/jmodguard-bot:${tag}`,
      STATUS: statusFile,
      GHCR_TOKEN: "promote-job-token",
      GITHUB_TOKEN: "not-the-promote-token",
      GH_TOKEN: "ambient-token",
      TMPDIR: dir,
    },
  });
  const ghText = fs.readFileSync(ghLog, "utf8");
  const calls = [...ghText.matchAll(/^method=(\S+) path=(\S*)$/gm)].map(match => ({
    method: match[1],
    path: match[2],
  }));
  const args = [...ghText.matchAll(/^args=(.*)$/gm)].map(match => match[1]);
  const tokens = [...ghText.matchAll(/^token=(.*)$/gm)].map(match => match[1]);
  const dockerText = fs.existsSync(dockerLog) ? fs.readFileSync(dockerLog, "utf8") : "";
  const status = Number(fs.readFileSync(statusFile, "utf8").trim());
  const timeoutCalls = fs.existsSync(timeoutLog) ? fs.readFileSync(timeoutLog, "utf8").trim().split("\n") : [];
  const pages = fs.readFileSync(pagesLog, "utf8").trim().split("\n");
  assert.equal(timeoutCalls.length, calls.filter(call => call.method === "GET").length);
  for (const call of timeoutCalls) {
    assert.match(call, /^--kill-after=5s 60s gh api --paginate /);
  }
  fs.rmSync(dir, { recursive: true, force: true });
  assert.equal(result.error, undefined, result.error && result.error.message);
  return { status, calls, args, tokens, pages, dockerText, stdout: result.stdout, stderr: result.stderr };
}
function assertUserPackageRoute(outcome) {
  assert.ok(outcome.calls.length > 0, outcome.stderr || outcome.stdout);
  assert.equal(outcome.args.length, outcome.calls.length);
  assert.ok(outcome.tokens.every(token => token === "promote-job-token"), outcome.tokens.join(","));
  outcome.calls.forEach((call, index) => {
    assert.equal(call.path.startsWith("/users/jatmn/packages/container/jmodguard-bot/versions"), true, call.path);
    assert.equal(call.path.includes("/orgs/"), false, call.path);
    if (call.method !== "GET") return;
    assert.equal(call.path, "/users/jatmn/packages/container/jmodguard-bot/versions?per_page=100", call.path);
    assert.match(outcome.args[index], /(^| )--paginate( |$)/);
    assert.match(outcome.args[index], /(?:^|[?&])per_page=100(?:&|$)/);
  });
}
const versionsRoot = "/users/jatmn/packages/container/jmodguard-bot/versions";
const versionsList = `${versionsRoot}?per_page=100`;
const singleTag = "a".repeat(40);
const stagedTag = "b".repeat(40);
const absentTag = "c".repeat(40);
const single = runRegistryDelete(singleTag, [[
  packageVersion(11, [singleTag]),
  packageVersion(12, [`_publish-${"b".repeat(12)}-1`, "latest"]),
]]);
assertUserPackageRoute(single);
assert.equal(single.status, 0, single.stderr);
assert.deepEqual(single.calls, [
  { method: "GET", path: versionsList },
  { method: "DELETE", path: `${versionsRoot}/11` },
]);
assert.equal(single.dockerText, "");
const timedOutInitial = runRegistryDelete(singleTag, [[
  [packageVersion(11, [singleTag])],
  [packageVersion(12, [singleTag])],
]], { stallFrom: 1 });
assertUserPackageRoute(timedOutInitial);
assert.deepEqual(timedOutInitial.pages, ["1:1"], "the same paginate invocation must stall before its second page");
assert.notEqual(timedOutInitial.status, 0, "a timed-out partial list must fail");
assert.deepEqual(timedOutInitial.calls, [{ method: "GET", path: versionsList }]);
assert.equal(timedOutInitial.dockerText, "", "a partial list must not mutate registry tags");
const staged = runRegistryDelete(stagedTag, [
  [packageVersion(21, [`_publish-${"b".repeat(12)}-7`, stagedTag])],
  [
    packageVersion(21, [`_publish-${"b".repeat(12)}-7`]),
    packageVersion(99, [stagedTag]),
  ],
]);
assertUserPackageRoute(staged);
assert.equal(staged.status, 0, staged.stderr);
assert.deepEqual(staged.calls, [
  { method: "GET", path: versionsList },
  { method: "GET", path: versionsList },
  { method: "DELETE", path: `${versionsRoot}/99` },
]);
assert.doesNotMatch(staged.dockerText, /imagetools/);
assert.equal(staged.calls.some(call => call.path.endsWith("/21")), false);
assert.match(staged.dockerText, new RegExp(`--tag ghcr\\.io/jatmn/jmodguard-bot:${stagedTag}`));
assert.match(staged.dockerText, /--push/);
const absent = runRegistryDelete(absentTag, [[
  packageVersion(31, [`_publish-${"c".repeat(12)}-3`, "latest"]),
]]);
assertUserPackageRoute(absent);
assert.notEqual(absent.status, 0);
assert.deepEqual(absent.calls, [{ method: "GET", path: versionsList }]);
assert.equal(absent.dockerText, "");
assert.match(absent.stderr, /registry tag .* matched no package version/);
const preservedDigest = "sha256:" + "d".repeat(64);
const stagingName = `_publish-${"b".repeat(12)}-7`;
function sharedThen(pages) {
  return [
    [packageVersion(21, [stagingName, stagedTag], preservedDigest)],
    pages,
  ];
}
function assertSplitDelete(outcome) {
  assertUserPackageRoute(outcome);
  assert.equal(outcome.status, 0, outcome.stderr);
  assert.deepEqual(outcome.calls, [
    { method: "GET", path: versionsList },
    { method: "GET", path: versionsList },
    { method: "DELETE", path: `${versionsRoot}/99` },
  ]);
  assert.equal(outcome.calls.some(call => call.path.endsWith("/21")), false);
  assert.match(outcome.dockerText, new RegExp(`--tag ghcr\\.io/jatmn/jmodguard-bot:${stagedTag}`));
  assert.match(outcome.dockerText, /--push/);
  assert.doesNotMatch(outcome.dockerText, /imagetools/);
}
const preservedFirst = runRegistryDelete(stagedTag, sharedThen([
  [packageVersion(21, [stagingName], preservedDigest)],
  [packageVersion(99, [stagedTag])],
]));
assertSplitDelete(preservedFirst);
const disposableFirst = runRegistryDelete(stagedTag, sharedThen([
  [packageVersion(99, [stagedTag])],
  [packageVersion(21, [stagingName], preservedDigest)],
]));
assertSplitDelete(disposableFirst);
function assertNoPartialUntag(outcome, message) {
  assertUserPackageRoute(outcome);
  assert.notEqual(outcome.status, 0);
  assert.match(outcome.stderr, message);
  assert.match(outcome.stderr, new RegExp(`restored to ${preservedDigest}`));
  assert.equal(outcome.calls.some(call => call.method === "DELETE"), false, JSON.stringify(outcome.calls));
  const pushed = outcome.dockerText.indexOf("--push");
  const restored = outcome.dockerText.indexOf("imagetools create");
  assert.ok(pushed !== -1 && restored > pushed, outcome.dockerText);
  assert.match(outcome.dockerText, new RegExp(`--tag ghcr\\.io/jatmn/jmodguard-bot:${stagedTag} ghcr\\.io/jatmn/jmodguard-bot@${preservedDigest}`));
}
const timedOutDetached = runRegistryDelete(stagedTag, sharedThen([
  [packageVersion(21, [stagingName], preservedDigest), packageVersion(99, [stagedTag])],
  [packageVersion(100, [stagedTag])],
]), { stallFrom: 2 });
assertNoPartialUntag(timedOutDetached, /package version list failed/);
assert.deepEqual(timedOutDetached.pages, ["1:1", "2:1", "3:1", "4:1"], "each retry must stall before its second page");
assert.equal(timedOutDetached.calls.filter(call => call.method === "GET").length, 4);
const noDisposable = runRegistryDelete(stagedTag, sharedThen([
  [packageVersion(21, [stagingName], preservedDigest)],
  [packageVersion(50, ["latest"])],
]));
assertNoPartialUntag(noDisposable, /matched no disposable package version/);
const manyDisposable = runRegistryDelete(stagedTag, sharedThen([
  [packageVersion(21, [stagingName], preservedDigest), packageVersion(99, [stagedTag])],
  [packageVersion(100, [stagedTag])],
]));
assertNoPartialUntag(manyDisposable, /matched 2 disposable package versions/);
const deleteFailed = runRegistryDelete(stagedTag, sharedThen([
  [packageVersion(21, [stagingName], preservedDigest)],
  [packageVersion(99, [stagedTag])],
]), { deleteStatus: 1 });
assertUserPackageRoute(deleteFailed);
assert.notEqual(deleteFailed.status, 0);
assert.match(deleteFailed.stderr, /disposable package version 99 could not be deleted/);
assert.match(deleteFailed.stderr, new RegExp(`restored to ${preservedDigest}`));
assert.deepEqual(deleteFailed.calls, [
  { method: "GET", path: versionsList },
  ...Array.from({ length: 3 }, () => [
    { method: "GET", path: versionsList },
    { method: "DELETE", path: `${versionsRoot}/99` },
  ]).flat(),
]);
const deletePushed = deleteFailed.dockerText.indexOf("--push");
const deleteRestored = deleteFailed.dockerText.indexOf("imagetools create");
assert.ok(deletePushed !== -1 && deleteRestored > deletePushed, deleteFailed.dockerText);
assert.match(deleteFailed.dockerText, new RegExp(`--tag ghcr\\.io/jatmn/jmodguard-bot:${stagedTag} ghcr\\.io/jatmn/jmodguard-bot@${preservedDigest}`));
const pushFailed = runRegistryDelete(stagedTag, sharedThen([
  packageVersion(21, [stagingName], preservedDigest),
  packageVersion(99, [stagedTag]),
]), { pushStatus: 1 });
assert.notEqual(pushFailed.status, 0);
assert.match(pushFailed.dockerText, new RegExp(`imagetools create --prefer-index=false --tag ghcr\\.io/jatmn/jmodguard-bot:${stagedTag} ghcr\\.io/jatmn/jmodguard-bot@${preservedDigest}`));
assert.equal(pushFailed.calls.some(call => call.method === "DELETE"), false);
const manyVersions = runRegistryDelete(singleTag, [[
  [packageVersion(11, [singleTag])],
  [packageVersion(13, [singleTag])],
]]);
assertUserPackageRoute(manyVersions);
assert.notEqual(manyVersions.status, 0);
assert.match(manyVersions.stderr, /matched 2 package versions/);
assert.equal(manyVersions.dockerText, "");
assert.equal(manyVersions.calls.some(call => call.method === "DELETE"), false, JSON.stringify(manyVersions.calls));
const noDigest = runRegistryDelete(stagedTag, [[
  packageVersion(21, [stagingName, stagedTag], null),
]]);
assertUserPackageRoute(noDigest);
assert.notEqual(noDigest.status, 0);
assert.match(noDigest.stderr, /preserved version 21 has no digest to restore/);
assert.equal(noDigest.dockerText, "");
assert.equal(noDigest.calls.some(call => call.method === "DELETE"), false);
// Exercise the installed hooks through Git: commits check the index; pushes run
// contracts once. The preflight substitute records selection and failure flow,
// not application-test success (covered by the real final preflight).
const hookDir = fs.mkdtempSync(path.join(os.tmpdir(), "jmodguard-hooks-"));
try {
  const repo = path.join(hookDir, "repo");
  const remote = path.join(hookDir, "remote.git");
  const calls = path.join(hookDir, "calls");
  const globalConfig = path.join(hookDir, "gitconfig");
  const inheritedHooks = path.join(hookDir, "global-hooks");
  fs.mkdirSync(inheritedHooks);
  for (const hook of ["pre-commit", "pre-push"]) {
    fs.writeFileSync(path.join(inheritedHooks, hook),
      '#!/bin/sh\necho "unexpected inherited hook" >&2\nexit 97\n', { mode: 0o755 });
  }
  run("git", ["config", "--file", globalConfig, "core.hooksPath", inheritedHooks]);
  const env = { ...process.env, GIT_CONFIG_GLOBAL: globalConfig, HOOK_CALLS: calls };
  run("git", ["init", "-q", "--initial-branch=main", repo], { env });
  run("git", ["init", "-q", "--bare", remote], { env });
  const git = (...args) => run("git", ["-C", repo, ...args], { env });
  git("config", "user.name", "Hook test");
  git("config", "user.email", "hook@example.invalid");
  git("config", "commit.gpgsign", "false");
  git("remote", "add", "origin", remote);
  fs.mkdirSync(path.join(repo, "scripts"));
  fs.cpSync(path.join(root, ".githooks"), path.join(repo, ".githooks"), { recursive: true });
  fs.copyFileSync(path.join(root, "scripts/install-git-hooks.sh"), path.join(repo, "scripts/install-git-hooks.sh"));
  fs.writeFileSync(path.join(repo, "scripts/ci-preflight.sh"),
    '#!/usr/bin/env bash\nprintf "%s\\n" "$*" >>"$HOOK_CALLS"\nexit "${HOOK_RESULT:-0}"\n');
  fs.writeFileSync(path.join(repo, "notes.md"), "base\n");
  git("add", ".");
  // Bootstrap without user hooks; subsequent operations exercise installed hooks.
  git("-c", "core.hooksPath=/dev/null", "commit", "-qm", "base");
  git("-c", "core.hooksPath=/dev/null", "push", "-qu", "origin", "main");
  run("bash", ["scripts/install-git-hooks.sh", "--base", "origin/main"], { cwd: repo, env });
  fs.writeFileSync(path.join(repo, "notes.md"), "clean staged change\n");
  git("add", "notes.md");
  // Unstaged whitespace is not part of this commit.
  fs.appendFileSync(path.join(repo, "notes.md"), "unstaged trailing space \n");
  run("git", ["commit", "-qm", "clean index"], { cwd: repo, env });
  assert.equal(fs.existsSync(calls), false, "commit must not run preflight");
  git("add", "notes.md");
  const bad = spawnSync("git", ["commit", "-qm", "bad index"], { cwd: repo, env, encoding: "utf8" });
  assert.notEqual(bad.status, 0, "staged whitespace must block commit");
  assert.match(bad.stderr + bad.stdout, /trailing whitespace/);
  git("restore", "--staged", "--worktree", "notes.md");
  run("git", ["push", "origin", "main"], { cwd: repo, env });
  assert.deepEqual(fs.readFileSync(calls, "utf8").trim().split("\n"), ["--base origin/main --only contracts"]);
  fs.writeFileSync(path.join(repo, "notes.md"), "another change\n");
  git("add", "notes.md");
  run("git", ["commit", "-qm", "next"], { cwd: repo, env });
  const remoteBefore = run("git", ["--git-dir", remote, "rev-parse", "main"]);
  const refused = spawnSync("git", ["push", "origin", "main"], {
    cwd: repo, env: { ...env, HOOK_RESULT: "7" }, encoding: "utf8",
  });
  assert.notEqual(refused.status, 0, "failed contracts must block push");
  assert.equal(run("git", ["--git-dir", remote, "rev-parse", "main"]), remoteBefore);
} finally {
  fs.rmSync(hookDir, { recursive: true, force: true });
}
console.log("ci-workflow-test: ok");
