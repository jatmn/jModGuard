#!/usr/bin/env node
// Lightweight CI routing/trust contracts; actionlint validates YAML/expression syntax.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
const root = path.resolve(import.meta.dirname, "..");
const workflow = fs.readFileSync(path.join(root, ".github/workflows/ci.yml"), "utf8");
const publication = fs.readFileSync(path.join(root, ".github/workflows/publish.yml"), "utf8");
const combined = workflow + "\n" + publication.replace(/^  ci:\n[\s\S]*?(?=^  release:)/m, "");
const jobs = new Map([...combined.matchAll(/^  ([a-z][a-z-]*):\n([\s\S]*?)(?=^  [a-z][a-z-]*:\n|$(?![\s\S]))/gm)]
  .map(match => [match[1], match[2]]));
for (const name of ["plan", "checks", "ci", "release", "prepare-publication", "publish"]) {
  assert.ok(jobs.has(name), "missing job: " + name);
}
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
assert.match(jobs.get("publish"), /needs\.prepare-publication\.result == 'success'/);
assert.match(jobs.get("publish"), /packages: write/);
assert.doesNotMatch(jobs.get("publish"), /contents: write/);
assert.match(jobs.get("release"), /github\.event_name == 'push' && github\.ref == 'refs\/heads\/main'/);
assert.match(jobs.get("release"), /sha: \$\{\{ steps\.release\.outputs\.sha \}\}/);
assert.match(jobs.get("prepare-publication"), /JMODGUARD_RELEASE_SHA: \$\{\{ needs\.release\.outputs\.created == 'true' && needs\.release\.outputs\.sha \|\| '' \}\}/);
assert.match(jobs.get("prepare-publication"), /release_sha: \$\{\{ steps\.prepare\.outputs\.release_sha \}\}/);
assert.match(jobs.get("publish"), /JMODGUARD_RELEASE_SHA: \$\{\{ needs\.prepare-publication\.outputs\.release_sha \}\}/);
assert.doesNotMatch(combined, /gh pr merge|enablePullRequestAutoMerge|auto-merge/);
for (const match of combined.matchAll(/^\s+(?:- )?uses: (\S+)/gm)) {
  if (match[1].startsWith("./")) continue;
  assert.match(match[1], /@[0-9a-f]{40}$/, "action must be pinned to a commit");
}
for (const name of ["plan", "ci"]) {
  assert.match(jobs.get(name), /!github\.event\.repository\.private \|\| vars\.JMODGUARD_ENABLE_PRIVATE_CI == 'true'/);
}
assert.match(workflow, /^  pull_request:/m);
assert.match(workflow, /^  workflow_call:/m);
assert.doesNotMatch(workflow, /(?:contents|packages|pull-requests): write/);
assert.match(workflow, /cancel-in-progress: \$\{\{ github.event_name == 'pull_request' \}\}/);
assert.match(publication, /^  queue: max$/m);
assert.match(publication, /^  cancel-in-progress: false$/m);
assert.match(publication, /uses: \.\/\.github\/workflows\/ci\.yml/);
assert.doesNotMatch(publication, /secrets: inherit/);
console.log("ci-workflow-test: ok");
