#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import vm from "node:vm";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const src = fs.readFileSync(path.join(root, "frontend/app.js"), "utf8");

function loadFrontend(fetchImpl, clock, timers) {
  const document = {
    getElementById() {
      return null;
    },
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
    AbortController,
    setTimeout: timers?.setTimeout || setTimeout,
    clearTimeout: timers?.clearTimeout || clearTimeout,
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

test("waitForApiMe bootstraps with POST /api/me", async () => {
  let method;
  const fetchImpl = async (url, opts) => {
    method = opts && opts.method;
    return {
      ok: true,
      status: 200,
      headers: { get: () => null },
      text: async () => JSON.stringify({ user: { username: "ok" }, guilds: [] }),
    };
  };
  const { waitForApiMe } = loadFrontend(fetchImpl);
  await waitForApiMe();
  assert.equal(method, "POST");
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

test("waitForApiMe does not shrink abort to leftover reachability before class is known", async () => {
  const clock = { ms: 0 };
  let calls = 0;
  let jumpedToDeadline = false;
  const pending = [];
  const fetchImpl = async (_url, opts) => {
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
      const abortTimer = pending.find((t) => t.ms === 90 * 1000);
      assert.ok(
        abortTimer,
        "attempt abort must stay the full 90s budget after prior 502s, not leftover consecutive time",
      );
      assert.ok(opts && opts.signal && !opts.signal.aborted);
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
  const timers = {
    setTimeout(fn, ms) {
      // Retry sleeps are short. Abort uses the full 90s budget.
      if (ms < 90 * 1000) {
        queueMicrotask(() => {
          if (!jumpedToDeadline) {
            jumpedToDeadline = true;
            clock.ms = 89 * 1000;
          }
          fn();
        });
        return 0;
      }
      const id = pending.length + 1;
      pending.push({ id, fn, ms });
      return id;
    },
    clearTimeout(id) {
      const i = pending.findIndex((t) => t.id === id);
      if (i >= 0) pending.splice(i, 1);
    },
  };
  const { waitForApiMe } = loadFrontend(fetchImpl, clock, timers);
  const me = await waitForApiMe();
  assert.equal(calls, 3);
  assert.equal(me.user.username, "ok");
  assert.equal(clock.ms, 89 * 1000);
});

test("waitForApiMe aborts a fetch that never settles after the reachability budget", async () => {
  const clock = { ms: 0 };
  let calls = 0;
  const fetchImpl = (_url, opts) => new Promise((_, reject) => {
    calls += 1;
    assert.ok(opts && opts.signal, "waitForApiMe must pass an AbortSignal");
    const abort = () => {
      const err = new Error("The operation was aborted");
      err.name = "AbortError";
      reject(err);
    };
    if (opts.signal.aborted) abort();
    else opts.signal.addEventListener("abort", abort);
  });
  const pending = [];
  const timers = {
    setTimeout(fn, ms) {
      const id = pending.length + 1;
      pending.push({ id, fn, ms });
      return id;
    },
    clearTimeout(id) {
      const i = pending.findIndex((t) => t.id === id);
      if (i >= 0) pending.splice(i, 1);
    },
  };
  const { waitForApiMe, originGaveUpMessage, apiError, ORIGIN_REACHABILITY_BUDGET_MS } =
    loadFrontend(fetchImpl, clock, timers);
  const done = waitForApiMe();
  const abortTimer = pending.find((t) => t.ms === ORIGIN_REACHABILITY_BUDGET_MS);
  assert.ok(abortTimer, "waitForApiMe must schedule an abort for a pending origin pull");
  clock.ms = ORIGIN_REACHABILITY_BUDGET_MS;
  abortTimer.fn();
  await assert.rejects(done, (err) => {
    assert.equal(calls, 1);
    assert.equal(err.message, originGaveUpMessage(apiError("Failed to fetch")));
    return true;
  });
});
