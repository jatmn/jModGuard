#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const src = fs.readFileSync(path.join(root, "frontend/app.js"), "utf8");

function loadFrontend(fetchImpl) {
  const document = {
    querySelector() {
      return {
        classList: { toggle() {} },
        textContent: "",
        className: "",
        addEventListener() {},
      };
    },
    querySelectorAll() {
      return [];
    },
  };
  const context = vm.createContext({
    fetch: fetchImpl,
    document,
    window: { JMODGUARD_API_BASE: "" },
    Date,
    Number,
    String,
    Math,
    JSON,
    Error,
    setTimeout,
    clearTimeout,
    console,
  });
  return vm.runInContext(
    `${src}\n({ isApiWarmup, isUnauthenticated, parseRetryAfterMs, waitForApiMe, apiError })`,
    context,
  );
}

test("isApiWarmup matches only the JSON not-ready contract", () => {
  const { isApiWarmup, apiError } = loadFrontend(async () => {});
  assert.equal(isApiWarmup(apiError("not ready", 503, 1000)), true);
  assert.equal(isApiWarmup(apiError("HTTP 503", 503, 0)), false);
  assert.equal(isApiWarmup(apiError("not ready", 500, 0)), false);
  assert.equal(isApiWarmup(apiError("not authenticated", 400, 0)), false);
});

test("waitForApiMe keeps retrying warmup until axum answers", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    if (calls < 3) {
      return {
        ok: false,
        status: 503,
        headers: { get: (name) => (name === "retry-after" ? "0" : null) },
        text: async () => JSON.stringify({ error: "not ready" }),
      };
    }
    return {
      ok: true,
      status: 200,
      headers: { get: () => null },
      text: async () => JSON.stringify({ user: { username: "ok" }, guilds: [] }),
    };
  };
  const { waitForApiMe } = loadFrontend(fetchImpl);
  const me = await waitForApiMe();
  assert.equal(calls, 3);
  assert.equal(me.user.username, "ok");
});

test("waitForApiMe does not treat unauthenticated as warmup", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    return {
      ok: false,
      status: 400,
      headers: { get: () => null },
      text: async () => JSON.stringify({ error: "not authenticated" }),
    };
  };
  const { waitForApiMe, isUnauthenticated } = loadFrontend(fetchImpl);
  await assert.rejects(waitForApiMe, (err) => {
    assert.equal(calls, 1);
    assert.equal(isUnauthenticated(err), true);
    return true;
  });
});

test("waitForApiMe has no parallel client deadline", () => {
  assert.equal(src.includes("6 * 60 * 1000"), false);
  assert.equal(src.includes("Date.now() +"), false);
});

test("parseRetryAfterMs caps advertised delay", () => {
  const { parseRetryAfterMs } = loadFrontend(async () => {});
  assert.equal(parseRetryAfterMs("1"), 1000);
  assert.equal(parseRetryAfterMs("99"), 30000);
  assert.equal(parseRetryAfterMs("nope"), 0);
});
