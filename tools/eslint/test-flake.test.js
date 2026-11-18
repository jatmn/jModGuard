import assert from "node:assert/strict";
import test from "node:test";
import { ESLint } from "eslint";

const eslint = new ESLint({ overrideConfigFile: "tools/eslint/eslint.config.js" });
async function lint(body) {
    const [result] = await eslint.lintText(body, { filePath: "scripts/frontend-eslint-probe-test.mjs" });
    return result.messages.map(({ ruleId, messageId }) => [ruleId, messageId]);
}

test("rejects real timer and clock calls and caught assertion failures in a loop", async () => {
    const messages = await lint(`
        import test from "node:test";
        import assert from "node:assert/strict";
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
