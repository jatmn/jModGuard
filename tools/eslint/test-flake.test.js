import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";
import { ESLint } from "eslint";

const eslint = new ESLint({ overrideConfigFile: "tools/eslint/eslint.config.js" });
async function lint(body) {
    const [result] = await eslint.lintText(body, { filePath: "scripts/frontend-eslint-probe-test.mjs" });
    return result.messages.map(({ ruleId, messageId }) => [ruleId, messageId]);
}

test("rejects real timer and clock calls and caught assertion failures in a loop", async () => {
    const messages = await lint(`
        import test from "node:test";
import { fileURLToPath } from "node:url";
        import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
        import { setTimeout as wait } from "node:timers/promises";
        test("uncontrolled", async () => {
            await wait(10);
            await new Promise(resolve => globalThis["setTimeout"](resolve, 10));
            Date.now();
            new globalThis.Date();
            for (let i = 0; i < 2; i++) {
                try { assert.equal(false, true); }
                catch { continue; }
            }
        });
    `);
    assert.deepEqual(messages, ["sleep", "sleep", "clock", "clock", "retry"].map((id) => ["jmodguard/test-flake", id]));
});

test("accepts fixed dates, fixture timers, and a scoped fake clock", async () => {
    assert.deepEqual(await lint(`
        import { test } from "node:test";
        test("controlled", (t) => {
            t.mock.timers.enable({ apis: ["Date", "setTimeout"], now: 0 });
            Date.now();
            new Date();
            setTimeout(() => {}, 10);
            t.mock.timers.tick(10);
        });
        function fixture(Date, setTimeout) {
            setTimeout(() => {}, 10);
            return new Date();
        }
        new Date(0);
    `), []);
});

test("a fake clock in a sibling or conditional setup leaves host calls detectable", async () => {
    const messages = await lint(`
        import test from "node:test";
import { fileURLToPath } from "node:url";
        test("controlled", (t) => {
            t.mock.timers.enable({ apis: ["Date"] });
            Date.now();
        });
        test("uncontrolled", (t) => {
            if (false) t.mock.timers.enable({ apis: ["Date"] });
            Date.now();
            t.mock.timers.enable({ apis: ["setTimeout"] });
            new Date();
            t.mock.timers.reset();
            setTimeout(() => {}, 10);
        });
    `);
    assert.deepEqual(messages, ["clock", "clock", "sleep"].map((id) => ["jmodguard/test-flake", id]));
});

test("captured timer imports still report with mock timers enabled", async () => {
    assert.deepEqual(await lint(`
        import test from 'node:test';
        import { setTimeout as delay } from 'node:timers/promises';
        test('wait', async (t) => {
            t.mock.timers.enable({ apis: ['setTimeout'] });
            await delay(10);
        });
    `), [["jmodguard/test-flake", "sleep"]]);
    assert.deepEqual(await lint(`
        import test from "node:test";
import { fileURLToPath } from "node:url";
        import { setTimeout } from "node:timers";
        import * as timers from "node:timers";
        import * as promises from "node:timers/promises";
        test("captured exports", async (t) => {
            t.mock.timers.enable({ apis: ["setTimeout"] });
            setTimeout(() => {}, 10);
            timers.setTimeout(() => {}, 10);
            await promises.setTimeout(10);
        });
    `), Array.from({ length: 3 }, () => ["jmodguard/test-flake", "sleep"]));
});

test("mock timers still exempt globals and default timer objects", async () => {
    assert.deepEqual(await lint(`
        import test from "node:test";
import { fileURLToPath } from "node:url";
        import timers from "node:timers";
        import promises from "node:timers/promises";
        test("replaceable timer bindings", async (t) => {
            t.mock.timers.enable({ apis: ["setTimeout"] });
            setTimeout(() => {}, 10);
            globalThis.setTimeout(() => {}, 10);
            timers.setTimeout(() => {}, 10);
            await promises.setTimeout(10);
        });
    `), []);
});

test("resetting either mock tracker restores host clocks and timers", async () => {
    assert.deepEqual(await lint(`
        import test from 'node:test';
        test('restored host clock', (t) => {
            t.mock.timers.enable({ apis: ['Date'], now: 123 });
            t.mock.reset();
            Date.now();
            new Date();
        });
    `), ["clock", "clock"].map((id) => ["jmodguard/test-flake", id]));
    assert.deepEqual(await lint(`
        import { mock, test } from "node:test";
        import timers from "node:timers";
        test("restored imported tracker", () => {
            mock.timers.enable({ apis: ["Date", "setTimeout"] });
            mock.reset();
            Date.now();
            new Date();
            setTimeout(() => {}, 10);
            timers.setTimeout(() => {}, 10);
        });
        test("restored context timers", (t) => {
            t.mock.timers.enable({ apis: ["setTimeout"] });
            t.mock.reset();
            setTimeout(() => {}, 10);
        });
    `), ["clock", "clock", "sleep", "sleep", "sleep"].map((id) => ["jmodguard/test-flake", id]));
});

test("enabling timers again restores the exemption after tracker reset", async () => {
    assert.deepEqual(await lint(`
        import { mock, test } from "node:test";
        test("re-enabled context tracker", (t) => {
            t.mock.timers.enable({ apis: ["Date", "setTimeout"] });
            t.mock.reset();
            t.mock.timers.enable({ apis: ["Date", "setTimeout"] });
            Date.now();
            new Date();
            setTimeout(() => {}, 10);
        });
        test("re-enabled imported tracker", () => {
            mock.timers.enable({ apis: ["Date", "setTimeout"] });
            mock.reset();
            mock.timers.enable({ apis: ["Date", "setTimeout"] });
            Date.now();
            new Date();
            setTimeout(() => {}, 10);
        });
    `), []);
});

test("loop catches that always propagate assertion failures are allowed", async () => {
    assert.deepEqual(await lint(`
        import test from 'node:test';
        import assert from 'node:assert/strict';
        test('annotated cases', () => {
            for (const value of [1, 2]) {
                try {
                    assert.equal(value, 1);
                } catch (error) {
                    throw new Error('case ' + value, { cause: error });
                }
            }
        });
        test("direct rethrow and conditional context", () => {
            for (const value of [1, 2]) {
                try {
                    assert.equal(value, 1);
                } catch (error) {
                    const label = String(value);
                    if (label) {
                        throw new Error(label, { cause: error });
                    }
                    throw error;
                }
                try {
                    assert.equal(value, 1);
                } catch (error) {
                    switch (value) {
                    case 1: break;
                    default: throw error;
                    }
                    throw error;
                }
            }
        });
    `), []);
});

test("loop catches that can swallow or continue still report", async () => {
    assert.deepEqual(await lint(`
        import test from "node:test";
import { fileURLToPath } from "node:url";
        import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
        test("retry", () => {
            for (const value of [1, 2]) {
                try { assert.equal(value, 1); }
                catch (error) { if (value === 1) throw error; }
                try { assert.equal(value, 1); }
                catch (error) {
                    if (value === 2) continue;
                    throw error;
                }
                try { assert.equal(value, 1); }
                catch { return; }
                try { assert.equal(value, 1); }
                catch { break; }
            }
        });
    `), Array.from({ length: 4 }, () => ["jmodguard/test-flake", "retry"]));
});

test("the frontend gate rejects deleting only the mock setup and preserves shifted base findings", (t) => {
    const cwd = mkdtempSync(join(tmpdir(), "test-flake-eslint-"));
    t.after(() => rmSync(cwd, { recursive: true, force: true }));
    const git = (...args) => execFileSync("git", args, { cwd });
    const write = (file, source) => writeFileSync(join(cwd, file), source);
    const file = "scripts/frontend-clock-test.mjs";
    const mock = "    t.mock.timers.enable({ apis: ['Date'], now: 0 });\n";
    const source = [
        "import test from 'node:test';",
        "test('clock', (t) => {",
        mock.trimEnd(),
        "    Date.now();",
        "});",
        "test('legacy', () => {",
        "    Date.now();",
        "});",
        "",
    ].join("\n");
    const script = fileURLToPath(new URL("../../scripts/check-test-flakes.mjs", import.meta.url));
    const check = () => spawnSync(process.execPath, [script, "eslint"], {
        cwd,
        encoding: "utf8",
        env: {
            ...process.env,
            GITHUB_EVENT_NAME: "pull_request",
            JMODGUARD_DIFF_BASE: "",
            JMODGUARD_DIFF_HEAD: "",
            JMODGUARD_PREFLIGHT_BASE: "origin/main",
        },
    });
    git("init", "-q");
    mkdirSync(join(cwd, "scripts"));
    mkdirSync(join(cwd, "tools"));
    write("tools/test-flake-allowlist.tsv", "");
    write(file, source);
    write("scripts/administration-state-test.mjs", "");
    write("scripts/package-webui-test.mjs", "");
    git("add", ".");
    git("-c", "user.name=Fixture", "-c", "user.email=fixture", "commit", "-qm", "base");
    git("branch", "review-head");
    write(file, source.replace(mock, ""));
    git("add", file);
    git("-c", "user.name=Fixture", "-c", "user.email=fixture", "commit", "-qm", "main removes the mock");
    git("update-ref", "refs/remotes/origin/main", "HEAD");
    git("checkout", "-q", "review-head");

    write(file, "\n" + source);
    const baseline = check();
    assert.equal(baseline.status, 0, baseline.stderr);
    assert.match(baseline.stderr, /Inherited test flake: .*clock in test legacy/);
    assert.doesNotMatch(baseline.stderr, /clock in test clock/);

    write(file, source.replace(mock, ""));
    const removed = check();
    assert.equal(removed.status, 1, removed.stderr);
    assert.match(removed.stderr, /^scripts\/frontend-clock-test\.mjs:3:5: clock in test clock/m);
    assert.match(removed.stderr, /Inherited test flake: .*clock in test legacy/);
});
