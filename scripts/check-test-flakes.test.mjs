import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const script = fileURLToPath(new URL("./check-test-flakes.mjs", import.meta.url));
const allowlist = "tools/test-flake-allowlist.tsv";
const entry = "clock\tsrc/test.rs\ttests::legacy\t2\t#666\n";
const sample = (duration) => `
pub async fn production() {
    timeout(Duration::from_secs(90), x()).await;
}
#[tokio::test]
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
    write("src/test.rs", "fn first() {}\nfn legacy() {}\n");
    git("add", ".");
    git("-c", "user.name=Fixture", "-c", "user.email=fixture", "commit", "-qm", "base");
    git("update-ref", "refs/remotes/origin/main", "HEAD");
    return { write, run: (mode, options) => run(process.execPath, [script, mode], options) };
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

test("the compiler gate distinguishes a new finding from inherited unlisted debt", (t) => {
    const { write, run } = fixture(t);
    write("src/test.rs", "fn first() {}\nfn legacy() {}\nfn added() {}\n");
    const record = (line, name) => JSON.stringify({
        reason: "compiler-message",
        message: {
            code: { code: "test_flake" },
            message: `clock in test ${name}: use an injected or paused clock`,
            spans: [{ file_name: "src/test.rs", is_primary: true, column_start: 1, line_start: line, line_end: line }],
        },
    });
    const result = run("rust", { input: [record(1, "first"), record(2, "tests::legacy"), record(3, "added")].join("\n") });
    assert.equal(result.status, 1);
    assert.match(result.stderr, /Inherited test flake: .*clock in test first/);
    assert.match(result.stderr, /src\/test.rs:3:1: clock in test added/);
    assert.doesNotMatch(result.stderr, /tests::legacy/);
});
