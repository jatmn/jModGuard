import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { timeoutBudgets } from "./check-test-flakes.mjs";

const script = fileURLToPath(new URL("./check-test-flakes.mjs", import.meta.url));
const allowlist = "tools/test-flake-allowlist.tsv";
const entry = "clock\tsrc/test.rs\ttests::legacy\t2\t#666\n";
const compilerSource = "fn first() {}\nfn legacy() {}\nfn unpaused() {}\nfn retrying() {}\n";
const sample = (duration) => `
pub async fn production() {
    timeout(Duration::from_secs(90), x()).await;
}
#[tokio::test]
#[allow(unused_variables)]
#[cfg(any(test, feature = "test-support"))]
async fn example() {
    timeout(
        Duration::from_millis(${duration}),
        fixture(),
    ).await;
}
`;

function fixture(t) {
    const cwd = mkdtempSync(join(tmpdir(), "test-flake-policy-"));
    t.after(() => rmSync(cwd, { recursive: true, force: true }));
    const run = (bin, args, options) => spawnSync(bin, args, {
        cwd, encoding: "utf8", ...options,
        env: {
            ...process.env,
            GITHUB_EVENT_NAME: "",
            JMODGUARD_DIFF_BASE: "",
            JMODGUARD_DIFF_HEAD: "",
            JMODGUARD_PREFLIGHT_BASE: "",
            ...options?.env,
        },
    });
    function git(...args) {
        const result = run("git", args);
        assert.equal(result.status, 0, result.stderr);
        return result.stdout.trim();
    }
    function write(file, content) {
        if (file.includes("/")) mkdirSync(join(cwd, file.split("/").slice(0, -1).join("/")), { recursive: true });
        writeFileSync(join(cwd, file), content);
    }
    git("init", "-q");
    write(allowlist, entry);
    write("src/service.rs", sample("1_000"));
    write("src/test.rs", compilerSource);
    write("scripts/frontend-budget-test.mjs", 'test("frontend", { timeout: 1_000 }, run);\n');
    git("add", ".");
    git("-c", "user.name=Fixture", "-c", "user.email=fixture", "commit", "-qm", "base");
    git("update-ref", "refs/remotes/origin/main", "HEAD");
    return { cwd, git, write, run: (mode, options, ...args) => run(process.execPath, [script, mode, ...args], options) };
}

test("the contracts gate rejects a budget increase, runner retries, and allowlist growth", (t) => {
    const { write, run } = fixture(t);
    write("src/service.rs", sample("1_001"));
    write(".config/nextest.toml", "[profile.default]\nretries = { backoff = \"fixed\", count = 2 }\n");
    write(allowlist, entry.replace("\t2\t", "\t3\t"));
    const result = run("contracts");
    assert.equal(result.status, 1);
    assert.match(result.stderr, /timeout increased/);
    assert.match(result.stderr, /nextest retries mask test failures/);
    assert.match(result.stderr, /exception must shrink/);
});

test("a shorter test budget and allowlist and ordinary production budget changes pass", (t) => {
    const { write, run } = fixture(t);
    write("src/service.rs", sample("999").replace("from_secs(90)", "from_secs(120)"));
    write(".config/nextest.toml", "[profile.default]\nretries = 0\n");
    write(allowlist, entry.replace("\t2\t", "\t1\t"));
    const result = run("contracts");
    assert.equal(result.status, 0, result.stderr);
});

test("the contracts gate rejects a frontend budget increase", (t) => {
    const { write, run } = fixture(t);
    write("scripts/frontend-budget-test.mjs", 'test("frontend", { timeout: 1_001 }, run);\n');
    const result = run("contracts");
    assert.equal(result.status, 1, result.stderr);
    assert.match(result.stderr, /^scripts\/frontend-budget-test\.mjs:1: timeout increased;/m);
});

test("the contracts gate rejects nonzero nextest retries in a workflow", (t) => {
    const { write, run } = fixture(t);
    const path = ".github/workflows/test.yml";
    write(path, "jobs:\n  check:\n    steps:\n      - run: cargo nextest run --retries 0\n");
    const disabled = run("contracts");
    assert.equal(disabled.status, 0, disabled.stderr);
    write(path, "jobs:\n  check:\n    steps:\n      - run: cargo nextest run --retries 2\n");
    const enabled = run("contracts");
    assert.equal(enabled.status, 1, enabled.stderr);
    assert.match(enabled.stderr, /^\.github\/workflows\/test\.yml:4: nextest retries mask test failures$/m);
});

test("a verified GitHub merge checkout uses its main parent even with a stale event base", (t) => {
    const { git, write, run } = fixture(t);
    const base = git("rev-parse", "HEAD");
    const asAuthor = (...args) => git("-c", "user.name=Fixture", "-c", "user.email=fixture", ...args);
    git("switch", "-qc", "updated-main");
    write("src/service.rs", sample("1_001"));
    asAuthor("commit", "-qam", "main updates timeout");
    git("switch", "-qc", "feature", base);
    write("feature.txt", "feature change\n");
    git("add", "feature.txt");
    asAuthor("commit", "-qm", "feature change");
    const feature = git("rev-parse", "HEAD");
    git("switch", "-q", "updated-main");
    asAuthor("merge", "--no-ff", "-q", "-m", "GitHub merge checkout", feature);
    const env = {
        GITHUB_EVENT_NAME: "pull_request",
        GITHUB_REF: "refs/pull/1/merge",
        GITHUB_SHA: git("rev-parse", "HEAD"),
        JMODGUARD_DIFF_BASE: base,
        JMODGUARD_DIFF_HEAD: feature,
    };
    const inherited = run("contracts", { env });
    assert.equal(inherited.status, 0, inherited.stderr);
    const ordinary = run("contracts", { env: { ...env, GITHUB_REF: "refs/heads/feature" } });
    assert.equal(ordinary.status, 1);
    assert.match(ordinary.stderr, /timeout increased/);

    write("src/service.rs", sample("1_002"));
    const introduced = run("contracts", { env });
    assert.equal(introduced.status, 1);
    assert.match(introduced.stderr, /timeout increased/);
});

test("inline Rust test ranges follow attributes and use the marker's closing indent", () => {
    const source = `
#[cfg(test)]
mod fixtures {
    const TEST_TIMEOUT: Duration = Duration::from_secs(1);
    #[tokio::test(start_paused = true)] // trailing comment
    #[allow(unused_variables)]
    async fn example() {
        let read_deadline = Duration::from_millis(20);
        let not_a_budget = Duration::from_millis(50);
        timeout(Duration::from_millis(100), fixture()).await;
    }
}
fn production() {
    let read_deadline = Duration::from_secs(90);
}
`;
    assert.deepEqual(timeoutBudgets("src/service.rs", source).map(({ name, value }) => [name, value]), [
        ["timeout", 100], ["TEST_TIMEOUT", 1000], ["read_deadline", 20],
    ]);
});

test("the contracts gate finishes on a long malformed Rust attribute run", (t) => {
    const { write, run } = fixture(t);
    const attributes = "#[test]\n#[" + "]]\n#[".repeat(4096) + "\n";
    write("src/service.rs", attributes + sample("1_001"));
    const result = run("contracts", { timeout: 5000 });
    assert.ifError(result.error);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /timeout increased/);
});

test("the compiler gate requires a matching base diagnostic and always rejects unlisted retries", (t) => {
    const { cwd, write, run } = fixture(t);
    write("src/test.rs", "\n" + compilerSource + "fn added() {}\n");
    const clock = (name) => `clock in test ${name}: use an injected or paused clock (tokio::time::Instant::now)`;
    const retry = "retry in test retrying: do not mask assertion failures";
    const record = (line, message) => JSON.stringify({
        reason: "compiler-message",
        message: {
            code: { code: "test_flake" },
            message,
            spans: [{ file_name: "src/test.rs", is_primary: true, column_start: 1, line_start: line, line_end: line }],
        },
    });
    write("bin/cargo", `#!/usr/bin/env node
const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
assert.equal(readFileSync("src/test.rs", "utf8"), ${JSON.stringify(compilerSource)});
assert.deepEqual(process.argv.slice(-2), ["-p", "fixture"]);
console.log(${JSON.stringify(record(1, clock("first")))});
console.log(${JSON.stringify(record(4, retry))});
`);
    chmodSync(join(cwd, "bin/cargo"), 0o755);
    const result = run("rust", {
        input: [
            record(2, clock("first")), record(3, clock("tests::legacy")), record(4, clock("unpaused")),
            record(5, retry), record(6, clock("added")),
        ].join("\n"),
        env: { PATH: `${join(cwd, "bin")}:${process.env.PATH}` },
    }, "test-flake-fixture.so", "-p", "fixture");
    assert.equal(result.status, 1, result.stderr);
    assert.match(result.stderr, /Inherited test flake: .*src\/test.rs:2:1: clock in test first/);
    assert.match(result.stderr, /^src\/test.rs:4:1: clock in test unpaused/m);
    assert.match(result.stderr, /^src\/test.rs:5:1: retry in test retrying/m);
    assert.match(result.stderr, /^src\/test.rs:6:1: clock in test added/m);
    assert.doesNotMatch(result.stderr, /tests::legacy/);
});
