#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const src = fs.readFileSync(path.join(root, "frontend/app.js"), "utf8");

function loadFrontend(fetchImpl, clock) {
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
  const now = clock || { ms: 0 };
  const context = vm.createContext({
    fetch: fetchImpl,
    document,
    window: { JMODGUARD_API_BASE: "" },
    Date: { now: () => now.ms },
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
    `${src}\n({ isApiWarmup, isOriginStarting, isUnauthenticated, parseRetryAfterMs, waitForApiMe, apiError, API_WARMUP_BUDGET_MS, ORIGIN_REACHABILITY_BUDGET_MS, originGaveUpMessage })`,
    context,
  );
}

test("isOriginStarting retries proxy and network failures until axum answers", () => {
  const { isOriginStarting, isApiWarmup, apiError } = loadFrontend(async () => {});
  assert.equal(isApiWarmup(apiError("not ready", 503, 0)), true);
  assert.equal(isOriginStarting(apiError("not ready", 503, 0)), true);
  assert.equal(isOriginStarting(apiError("HTTP 502", 502, 0)), true);
  assert.equal(isOriginStarting(apiError("HTTP 504", 504, 0)), true);
  assert.equal(isOriginStarting(apiError("HTTP 503", 503, 0)), true);
  assert.equal(isOriginStarting(apiError("Failed to fetch")), true);
  assert.equal(isOriginStarting(apiError("not authenticated", 400, 0)), false);
  assert.equal(isOriginStarting(apiError("rate limited", 503, 0)), false);
});

test("waitForApiMe does not retry application JSON 503", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    return {
      ok: false,
      status: 503,
      headers: { get: () => null },
      text: async () => JSON.stringify({ error: "rate limited" }),
    };
  };
  const { waitForApiMe } = loadFrontend(fetchImpl);
  await assert.rejects(waitForApiMe, (err) => {
    assert.equal(calls, 1);
    assert.equal(err.message, "rate limited");
    return true;
  });
});

test("waitForApiMe keeps retrying origin-start failures until axum answers", async () => {
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    if (calls === 1) {
      return {
        ok: false,
        status: 502,
        headers: { get: () => null },
        text: async () => "Bad Gateway",
      };
    }
    if (calls === 2) {
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

test("waitForApiMe gives up on persistent origin failures", async () => {
  const clock = { ms: 0 };
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    if (calls > 1) clock.ms = 90 * 1000;
    return {
      ok: false,
      status: 502,
      headers: { get: () => null },
      text: async () => "Bad Gateway",
    };
  };
  const { waitForApiMe, originGaveUpMessage, apiError } = loadFrontend(fetchImpl, clock);
  await assert.rejects(waitForApiMe, (err) => {
    assert.equal(calls, 2);
    assert.equal(err.message, originGaveUpMessage(apiError("HTTP 502", 502, 0)));
    return true;
  });
});

test("waitForApiMe does not spend warmup time on a late 502", async () => {
  const clock = { ms: 0 };
  let calls = 0;
  const fetchImpl = async () => {
    calls += 1;
    if (calls <= 2) {
      clock.ms = calls === 1 ? 0 : 91 * 1000;
      return {
        ok: false,
        status: 503,
        headers: { get: (name) => (name === "retry-after" ? "0" : null) },
        text: async () => JSON.stringify({ error: "not ready" }),
      };
    }
    if (calls === 3) {
      clock.ms = 92 * 1000;
      return {
        ok: false,
        status: 502,
        headers: { get: () => null },
        text: async () => "Bad Gateway",
      };
    }
    clock.ms = 93 * 1000;
    return {
      ok: true,
      status: 200,
      headers: { get: () => null },
      text: async () => JSON.stringify({ user: { username: "ok" }, guilds: [] }),
    };
  };
  const { waitForApiMe } = loadFrontend(fetchImpl, clock);
  const me = await waitForApiMe();
  assert.equal(calls, 4);
  assert.equal(me.user.username, "ok");
});

test("waitForApiMe warmup budget is longer than origin reachability", () => {
  const { API_WARMUP_BUDGET_MS, ORIGIN_REACHABILITY_BUDGET_MS } = loadFrontend(async () => {});
  assert.equal(API_WARMUP_BUDGET_MS, 8 * 60 * 1000);
  assert.equal(ORIGIN_REACHABILITY_BUDGET_MS, 90 * 1000);
  assert.ok(API_WARMUP_BUDGET_MS > ORIGIN_REACHABILITY_BUDGET_MS);
});

test("parseRetryAfterMs caps advertised delay", () => {
  const { parseRetryAfterMs } = loadFrontend(async () => {});
  assert.equal(parseRetryAfterMs("1"), 1000);
  assert.equal(parseRetryAfterMs("99"), 30000);
  assert.equal(parseRetryAfterMs("nope"), 0);
});
