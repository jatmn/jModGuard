#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import vm from "node:vm";

class Element {
  constructor(tag = "div") {
    this.tag = tag;
    this.textContent = "";
    this.disabled = false;
    this.listeners = {};
    const classes = new Set();
    this.classList = {
      add(name) { classes.add(name); },
      remove(name) { classes.delete(name); },
      contains(name) { return classes.has(name); },
      toggle(name, forced) {
        if (forced === undefined) forced = !classes.has(name);
        if (forced) classes.add(name); else classes.delete(name);
        return forced;
      },
    };
    const childNodes = [];
    this.children = new Proxy(childNodes, {
      get(target, prop) {
        if (prop === "unshift") return undefined;
        const value = target[prop];
        return typeof value === "function" ? value.bind(target) : value;
      },
    });
    this._childNodes = childNodes;
    const dataset = {};
    Object.defineProperty(this, "dataset", {
      get: () => dataset,
      set() {
        throw new TypeError("Cannot set property dataset which has only a getter");
      },
      enumerable: true,
      configurable: true,
    });
  }
  append(...children) { this._childNodes.push(...children); }
  prepend(...children) { this._childNodes.unshift(...children); }
  replaceChildren(...children) { this._childNodes.length = 0; this._childNodes.push(...children); }
  replaceWith(next) { this.replacedWith = next; }
  addEventListener(name, callback) { this.listeners[name] = callback; }
  setAttribute(name, value) { this.attributes = { ...(this.attributes || {}), [name]: value }; }
}

const source = fs.readFileSync(path.resolve(import.meta.dirname, "../frontend/administration.js"), "utf8");

function page(fetchImpl = () => new Promise(() => {}), { apiBase = "", origin = "https://ui.example.test", visibility = "visible" } = {}) {
  const elements = new Map();
  const document = {
    visibilityState: visibility,
    documentElement: { dataset: {} },
    querySelector: () => ({}),
    addEventListener() {},
    createElement: (tag) => new Element(tag),
    getElementById(id) {
      if (!elements.has(id)) {
        const element = new Element();
        if (["sign-in", "updates", "database-usage"].includes(id)) element.classList.add("hidden");
        elements.set(id, element);
      }
      return elements.get(id);
    },
  };
  const timers = new Map();
  let nextTimer = 0;
  const context = vm.createContext({
    document,
    window: { JMODGUARD_WEBUI_SHA: "a".repeat(40), JMODGUARD_API_BASE: apiBase, location: { origin }, matchMedia: () => ({ matches: false, addEventListener() {} }) },
    fetch: fetchImpl,
    setTimeout(callback, delay) { timers.set(++nextTimer, { callback, delay }); return nextTimer; },
    clearTimeout(id) { timers.delete(id); },
    Date,
  });
  vm.runInContext(source, context);
  return { context, elements, timers };
}

function find(element, predicate) {
  if (predicate(element)) return element;
  for (const child of element.children) {
    const result = find(child, predicate);
    if (result) return result;
  }
  return null;
}

function textOf(element) {
  return element.textContent + element.children.map(textOf).join("");
}

function record(component = "api") {
  const sha = "a".repeat(40);
  return {
    component, channel: "latest", cadence: "off",
    candidate: { component, channel: "latest", source_sha: "b".repeat(40),
      digest: "sha256:" + "b".repeat(64), release_id: "v1", published_at: "2026-09-23T00:00:00Z" },
    installed: null, checked_at: Math.floor(Date.now() / 1000), check_error: null,
    observed_at: Math.floor(Date.now() / 1000), state: "running", last_error: null,
    observed: { source_sha: sha, base_image_sha: sha, executable_sha256: "c".repeat(64),
      started_at: Math.floor(Date.now() / 1000), self_update_enabled: true },
  };
}

function updateButton(pageState, component, launcher = false) {
  const cards = pageState.elements.get("update-cards");
  const card = cards.children.find((item) =>
    find(item, (child) => child.tag === "h2")?.textContent === component.toUpperCase());
  assert.ok(card);
  return find(card, (item) => item.tag === "button" && item.textContent === (launcher ? "Update launcher" : component === "webui" ? "Update WebUI" : "Update"));
}

function render(pageState, item) {
  const snapshot = {
    launchers: { [item.component]: { state: "optional", installable: true, reason: "Optional launcher update" } },
    components: [item], compatibility: { [item.component]: { installable: true } },
    apply_enabled: true, unattended_enabled: false, webui_storage_configured: true,
  };
  vm.runInContext("snapshot = " + JSON.stringify(snapshot) + "; stale = false; authorized = true; render();", pageState.context);
}

test("owner update controls require a current installable candidate", () => {
  const view = page();
  const api = record();
  render(view, api);
  assert.equal(updateButton(view, "api").disabled, false);

  api.check_error = "registry unavailable";
  render(view, api);
  assert.equal(updateButton(view, "api").disabled, true);

  api.check_error = null;
  api.channel = "nightly";
  render(view, api);
  assert.equal(updateButton(view, "api").disabled, true);

  api.channel = "latest";
  api.checked_at = Math.floor(Date.now() / 1000) - 7201;
  render(view, api);
  assert.equal(updateButton(view, "api").disabled, true);

  const webui = record("webui");
  webui.installed = { source_sha: webui.candidate.source_sha, digest: "sha256:" + "d".repeat(64) };
  render(view, webui);
  assert.equal(updateButton(view, "webui").disabled, true);
  const warning = textOf(view.elements.get("update-cards"));
  assert.match(warning, /different image digest/);
  assert.doesNotMatch(warning, /has different bytes/);
  webui.installed = { source_sha: "d".repeat(40), digest: "sha256:" + "d".repeat(64) };
  webui.previous = { source_sha: webui.candidate.source_sha, digest: "sha256:" + "e".repeat(64) };
  render(view, webui);
  assert.equal(updateButton(view, "webui").disabled, true);
  webui.state = "drift";
  webui.last_error = "Storage files differ";
  render(view, webui);
  assert.match(textOf(view.elements.get("update-cards")), /Storage drift: Storage files differ/);
});

test("signed-out Administration offers Discord sign-in on the existing API response", async () => {
  const view = page(async () => ({
    status: 400, ok: false, json: async () => ({ error: "not authenticated" }),
  }));
  await vm.runInContext("refresh()", view.context);
  assert.equal(view.elements.get("sign-in").classList.contains("hidden"), false);
  assert.equal(view.elements.get("updates").classList.contains("hidden"), true);
  assert.match(view.elements.get("update-status").textContent, /Sign in again/);
});

test("Administration carries the configured API origin through reads, controls, and sign-in", () => {
  const calls = [];
  const fetchImpl = (url, options) => {
    calls.push({ url, method: options.method, credentials: options.credentials });
    return new Promise(() => {});
  };
  const remote = page(fetchImpl, { apiBase: "https://api.example.test/" });
  for (const path of ["/api/updates", "/api/updates/check", "/api/updates/install", "/api/updates/policy"]) {
    vm.runInContext(`request(${JSON.stringify(path)}, {})`, remote.context);
  }
  assert.deepEqual(calls, ["/api/me", "/api/updates", "/api/updates/check", "/api/updates/install", "/api/updates/policy"].map((path) => ({
    url: `https://api.example.test${path}`, method: "POST", credentials: "include",
  })));
  assert.equal(remote.elements.get("sign-in-link").href,
    "https://api.example.test/login?return_to=https%3A%2F%2Fui.example.test");

  const localCalls = [];
  const local = page((url, options) => {
    localCalls.push({ url, credentials: options.credentials });
    return new Promise(() => {});
  });
  assert.deepEqual(localCalls, [{ url: "/api/me", credentials: "include" }]);
  assert.equal(local.elements.get("sign-in-link").href, "/login?return_to=%2Fadministration%2Findex.html");
  assert.match(fs.readFileSync(path.resolve(import.meta.dirname, "../frontend/administration/index.html"), "utf8"),
    /id="sign-in-link"/);
});

test("owner availability distinguishes baked source parity from channel mismatch", () => {
  const view = page();
  const api = record();
  api.candidate.source_sha = api.observed.source_sha;
  render(view, api);
  const availability = () => {
    const card = view.elements.get("update-cards").children[0];
    return textOf(find(card, (element) => element.tag === "p" && textOf(element).startsWith("Availability:")));
  };
  assert.match(availability(), /no source update \(image digest unverified\)/);
  api.channel = "nightly";
  render(view, api);
  assert.match(availability(), /unknown/);
});

test("owner cannot queue an already installed service release", () => {
  const view = page();
  const api = record();
  api.installed = { ...api.candidate };
  api.installed_launcher = { ...api.candidate };
  render(view, api);
  assert.equal(updateButton(view, "api").disabled, true);
  assert.equal(updateButton(view, "api", true).disabled, true);

  // A baked process with the same source SHA has no verified install digest.
  api.installed = null;
  api.installed_launcher = null;
  api.candidate.source_sha = api.observed.source_sha;
  render(view, api);
  assert.equal(updateButton(view, "api").disabled, false);
  assert.equal(updateButton(view, "api", true).disabled, false);

  // A WebUI owner can retry a same-digest publication to repair Storage.
  const webui = record("webui");
  webui.installed = { ...webui.candidate };
  render(view, webui);
  assert.equal(updateButton(view, "webui").disabled, false);
});

function response(value, status = 200) {
  return { status, ok: status === 200, json: async () => value };
}
function state(items) {
  return { launchers: Object.fromEntries(items.map((item) => [item.component, { state: "optional", installable: true, reason: "Optional launcher update" }])), components: items, compatibility: Object.fromEntries(items.map((item) => [item.component, { installable: true }])),
    apply_enabled: true, unattended_enabled: true, webui_storage_configured: true };
}

async function ready(fetchImpl) {
  const view = page(fetchImpl);
  await vm.runInContext("refresh()", view.context);
  return view;
}

test("check applies its returned snapshot and reports failures without leaving enabled stale controls", async () => {
  const api = record();
  let unavailable = false;
  const view = await ready(async (url) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/check") {
      api.candidate.source_sha = api.observed.source_sha;
      return response(state([api]));
    }
    if (unavailable) throw new Error("offline");
    return response(state([api]));
  });
  const check = find(view.elements.get("update-cards"), (item) => item.tag === "button" && item.textContent === "Check now");
  await check.listeners.click();
  assert.match(textOf(view.elements.get("update-cards")), /no source update/);
  unavailable = true;
  await vm.runInContext("refresh()", view.context);
  assert.equal(updateButton(view, "api").disabled, true);
  assert.match(view.elements.get("update-status").textContent, /offline/);
  unavailable = false;
  await vm.runInContext("refresh()", view.context);
  assert.equal(updateButton(view, "api").disabled, false);
});

test("application, launcher and WebUI installs show pending and automatically reconcile completion", async () => {
  for (const [component, launcher] of [["api", false], ["bot", true], ["webui", false]]) {
    const item = record(component);
    const calls = [];
    const view = await ready(async (url, options) => {
      calls.push({ url, body: JSON.parse(options.body) });
      if (url === "/api/me") return response({ is_bot_owner: true });
      if (url === "/api/updates/install") {
        if (launcher) item.desired_launcher = item.candidate;
        else { item.state = "queued"; item.desired = item.candidate; }
        return response({ queued: true });
      }
      return response(state([item]));
    });
    await updateButton(view, component, launcher).listeners.click();
    assert.match(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.equal(updateButton(view, component, launcher).disabled, true);
    assert.equal([...view.timers.values()][0].delay, 2000);
    if (launcher) item.installed_launcher = item.candidate;
    else { item.state = "running"; item.installed = item.candidate; item.observed.source_sha = item.candidate.source_sha; }
    await [...view.timers.values()][0].callback();
    assert.doesNotMatch(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.doesNotMatch(view.elements.get("update-status").textContent, /Waiting for the component/);
    assert.equal([...view.timers.values()][0].delay, 30000);
    assert.equal(calls.filter((call) => call.url === "/api/me").length, 1);
    assert.equal(calls.find((call) => call.url === "/api/updates/install").body.launcher, launcher || undefined);
  }
});

test("queued acknowledgement uses the immediate status snapshot for api, bot, WebUI, and launcher", async () => {
  const targets = [["api", false], ["bot", false], ["bot", true], ["webui", false]];
  for (const outcome of ["pending", "success", "failed"]) {
    for (const [component, launcher] of targets) {
      const item = record(component);
      let acknowledged = false;
      const view = await ready(async (url) => {
        if (url === "/api/me") return response({ is_bot_owner: true });
        if (url === "/api/updates/install") { acknowledged = true; return response({ queued: true }); }
        if (!acknowledged) return response(state([item]));
        if (outcome === "pending") {
          if (launcher) item.desired_launcher = item.candidate;
          else { item.state = "queued"; item.desired = item.candidate; }
        } else if (outcome === "success") {
          if (launcher) item.installed_launcher = item.candidate;
          else {
            item.state = "running";
            item.installed = item.candidate;
            item.observed.source_sha = item.candidate.source_sha;
          }
        } else {
          item.state = "failed";
          item.last_error = "Storage authentication failed";
        }
        return response(state([item]));
      });
      await updateButton(view, component, launcher).listeners.click();
      const banner = view.elements.get("update-status").textContent;
      const card = textOf(view.elements.get("update-cards"));
      assert.match(banner, /update request accepted/);
      if (outcome === "pending") {
        assert.match(banner, /Waiting for the component/);
        assert.match(card, /Update pending/);
      } else {
        assert.doesNotMatch(banner, /Waiting for the component/);
        if (outcome === "failed") assert.match(card, /Install failed: Storage authentication failed/);
        else assert.doesNotMatch(card, /Update pending/);
      }
    }
  }
});

test("a failed status read after a queued acknowledgement keeps the status error", async () => {
  const item = record("webui");
  let installed = false;
  const view = await ready(async (url) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/install") { installed = true; return response({ queued: true }); }
    if (installed) throw new Error("status offline");
    return response(state([item]));
  });
  await updateButton(view, "webui").listeners.click();
  const banner = view.elements.get("update-status").textContent;
  assert.match(banner, /status offline/);
  assert.doesNotMatch(banner, /Waiting for the component/);
  assert.doesNotMatch(banner, /update request accepted/);
});

test("overlapping refreshes share one read and mutations wait for that read", async () => {
  const item = record();
  let releaseRead;
  let block = false;
  let installs = 0;
  let reads = 0;
  const view = await ready(async (url) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/install") { installs++; return response({ queued: true }); }
    reads++;
    if (block) { block = false; return new Promise((resolve) => { releaseRead = resolve; }); }
    return response(state([item]));
  });
  block = true;
  const first = vm.runInContext("refresh()", view.context);
  const second = vm.runInContext("refresh()", view.context);
  assert.equal(first, second);
  const action = updateButton(view, "api").listeners.click();
  assert.equal(installs, 0);
  releaseRead(response(state([item])));
  await action;
  assert.equal(installs, 1);
  assert.equal(reads, 3);
});

test("authorization loss hides the controls and stops automatic polling", async () => {
  let lost = false;
  const view = await ready(async (url) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    return lost ? response({ error: "bot owner required" }, 403) : response(state([record()]));
  });
  lost = true;
  await vm.runInContext("refresh()", view.context);
  assert.equal(view.elements.get("updates").classList.contains("hidden"), true);
  assert.equal(view.timers.size, 0);
});

function findAll(element, predicate, found = []) {
  if (predicate(element)) found.push(element);
  for (const child of element.children || []) findAll(child, predicate, found);
  return found;
}

test("a policy save keeps the sibling field from the refresh it waits for", async () => {
  const item = record("bot");
  item.channel = "stable";
  item.cadence = "daily";
  let releaseRead;
  let block = false;
  let saved;
  const view = await ready(async (url, options) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/policy") {
      saved = JSON.parse(options.body);
      Object.assign(item, saved);
      return response(state([item]));
    }
    if (block) { block = false; return new Promise((resolve) => { releaseRead = resolve; }); }
    return response(state([item]));
  });
  const [channel, cadence] = findAll(view.elements.get("update-cards"), (element) => element.tag === "select");
  block = true;
  const reading = vm.runInContext("refresh()", view.context);
  channel.value = "nightly";
  const savingChannel = channel.listeners.change();
  item.cadence = "weekly";
  releaseRead(response(state([item])));
  await Promise.all([reading, savingChannel]);
  assert.deepEqual(saved, { component: "bot", channel: "nightly", cadence: "weekly" });

  block = true;
  const readingAgain = vm.runInContext("refresh()", view.context);
  cadence.value = "monthly";
  const savingCadence = cadence.listeners.change();
  item.channel = "stable";
  releaseRead(response(state([item])));
  await Promise.all([readingAgain, savingCadence]);
  assert.deepEqual(saved, { component: "bot", channel: "stable", cadence: "monthly" });
});

test("an install uses the candidate from the refresh it waits for", async () => {
  let item = record("api");
  let releaseRead;
  let block = false;
  let submitted;
  const view = await ready(async (url, options) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/install") {
      submitted = JSON.parse(options.body);
      return response({ queued: true });
    }
    if (block) { block = false; return new Promise((resolve) => { releaseRead = resolve; }); }
    return response(state([item]));
  });
  block = true;
  const reading = vm.runInContext("refresh()", view.context);
  const saving = updateButton(view, "api").listeners.click();
  item = { ...item, candidate: { ...item.candidate, digest: "sha256:" + "f".repeat(64) } };
  releaseRead(response(state([item])));
  await Promise.all([reading, saving]);
  assert.equal(submitted.digest, item.candidate.digest);
});

test("policy changes use authoritative responses and latest switches automatic installation off", async () => {
  const item = record("bot");
  item.channel = "stable";
  item.cadence = "daily";
  let saved;
  const view = await ready(async (url, options) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/policy") {
      saved = JSON.parse(options.body);
      Object.assign(item, saved);
    }
    return response(state([item]));
  });
  const channel = find(view.elements.get("update-cards"), (element) => element.tag === "select");
  channel.value = "latest";
  await channel.listeners.change();
  assert.deepEqual(saved, { component: "bot", channel: "latest", cadence: "off" });
});

test("pending installs surface terminal failures and leave Check now available for recovery", async () => {
  const item = record("webui");
  item.state = "queued";
  const view = await ready(async (url) => response(url === "/api/me" ? { is_bot_owner: true } : state([item])));
  const check = find(view.elements.get("update-cards"), (element) => element.tag === "button" && element.textContent === "Check now");
  assert.equal(check.disabled, false);
  item.state = "failed";
  item.last_error = "Storage installation interrupted";
  await [...view.timers.values()][0].callback();
  assert.match(textOf(view.elements.get("update-cards")), /Needs attention/);
  assert.match(textOf(view.elements.get("update-cards")), /Install failed: Storage installation interrupted/);
  assert.equal(updateButton(view, "webui").disabled, false);
});

test("a failed policy save restores the server's channel and displays the error", async () => {
  const item = record("api");
  const view = await ready(async (url) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/policy") return response({ error: "policy rejected" }, 400);
    return response(state([item]));
  });
  const select = find(view.elements.get("update-cards"), (element) => element.tag === "select");
  select.value = "nightly";
  await select.listeners.change();
  assert.equal(find(view.elements.get("update-cards"), (element) => element.tag === "select").value, "latest");
  assert.match(view.elements.get("update-status").textContent, /policy rejected/);
});

test("appearance changes even when browser storage is unavailable", () => {
  const view = page();
  const select = view.elements.get("appearance");
  select.value = "dark";
  select.listeners.change();
  assert.equal(vm.runInContext("document.documentElement.dataset.theme", view.context), "dark");
  select.value = "light";
  select.listeners.change();
  assert.equal(vm.runInContext("document.documentElement.dataset.theme", view.context), "light");
});


test("a queued older digest can still be superseded by the owner", () => {
  const view = page();
  const item = record("bot");
  item.state = "queued";
  item.desired = { ...item.candidate, digest: "sha256:" + "e".repeat(64) };
  item.desired_launcher = { ...item.desired };
  render(view, item);
  assert.equal(updateButton(view, "bot").disabled, false);
  assert.equal(updateButton(view, "bot", true).disabled, false);
  item.desired = { ...item.candidate };
  item.desired_launcher = { ...item.candidate };
  render(view, item);
  assert.equal(updateButton(view, "bot").disabled, true);
  assert.equal(updateButton(view, "bot", true).disabled, true);
});


test("uptime uses compact day/hour/minute/second units without leading zero units", () => {
  const view = page();
  const now = 1800000000;
  vm.runInContext(`Date = { now: () => ${now} * 1000 };`, view.context);
  for (const [seconds, expected] of [[0, "0s"], [38, "38s"], [818, "13m 38s"],
    [3608, "1h 0m 8s"]]) {
    assert.equal(vm.runInContext(`uptime(${now - seconds})`, view.context), expected);
  }
  assert.equal(vm.runInContext(`uptime(${now - 94418})`, view.context), "1d 2h 13m 38s");
  assert.equal(vm.runInContext("uptime(null)", view.context), "unknown");
});


test("service application and launcher requests reconcile independently after a sibling failure", async () => {
  for (const component of ["api", "bot"]) {
    const item = record(component);
    item.desired = { ...item.candidate };
    item.desired_launcher = { ...item.candidate };
    item.state = "queued";
    const view = await ready(async (url) => response(url === "/api/me" ? { is_bot_owner: true } : state([item])));
    item.desired_launcher = null;
    item.state = "failed";
    item.last_error = "launcher update: download failed";
    await vm.runInContext("refresh()", view.context);
    assert.match(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.match(textOf(view.elements.get("update-cards")), /launcher update: download failed/);
    assert.equal([...view.timers.values()][0].delay, 2000);
    assert.equal(updateButton(view, component).disabled, true);
    assert.equal(updateButton(view, component, true).disabled, false);
    // Owner can supersede a still-active application request with a different digest.
    item.candidate = { ...item.candidate, digest: "sha256:" + "e".repeat(64) };
    await vm.runInContext("refresh()", view.context);
    assert.equal(updateButton(view, component).disabled, false);
    item.installed = { ...item.desired };
    item.state = "running";
    item.last_error = null;
    await vm.runInContext("refresh()", view.context);
    assert.doesNotMatch(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.equal([...view.timers.values()][0].delay, 30000);
    // The inverse outcome keeps a launcher pending after application failure.
    item.desired = null;
    item.desired_launcher = { ...item.candidate };
    item.state = "failed";
    item.last_error = "application download failed";
    await vm.runInContext("refresh()", view.context);
    assert.match(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.match(textOf(view.elements.get("update-cards")), /application download failed/);
    assert.equal([...view.timers.values()][0].delay, 2000);
    item.installed_launcher = { ...item.desired_launcher };
    await vm.runInContext("refresh()", view.context);
    assert.doesNotMatch(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.equal([...view.timers.values()][0].delay, 30000);
  }
});

test("failed WebUI publication remains terminal even with a retained desired digest", async () => {
  const item = record("webui");
  item.desired = { ...item.candidate };
  item.state = "failed";
  item.last_error = "publication interrupted";
  const view = await ready(async (url) => response(url === "/api/me" ? { is_bot_owner: true } : state([item])));
  assert.doesNotMatch(textOf(view.elements.get("update-cards")), /Update pending/);
  assert.equal([...view.timers.values()][0].delay, 30000);
  assert.equal(updateButton(view, "webui").disabled, false);
});

test("Browser version remains expanded or collapsed across status refreshes", async () => {
  const view = await ready(async (url) => response(url === "/api/me" ? { is_bot_owner: true } : state([record()])));
  const details = () => find(view.elements.get("browser-version"), (element) => element.tag === "details");
  details().open = true;
  await vm.runInContext("refresh()", view.context);
  assert.equal(details().open, true);
  details().open = false;
  await vm.runInContext("refresh()", view.context);
  assert.equal(details().open, false);
});

test("transient initial access checks retry automatically but confirmed denials do not", async () => {
  for (const failure of ["network", "server"]) {
    let calls = 0;
    const view = await ready(async (url) => {
      if (url === "/api/me" && ++calls === 1) {
        if (failure === "network") throw new Error("offline");
        return response({ error: "starting" }, 503);
      }
      return response(url === "/api/me" ? { is_bot_owner: true } : state([record()]));
    });
    assert.equal([...view.timers.values()][0].delay, 30000);
    await [...view.timers.values()][0].callback();
    assert.equal(view.elements.get("updates").classList.contains("hidden"), false);
    assert.equal(updateButton(view, "api").disabled, false);
  }
  for (const denial of [response({}, 401), response({}, 403), response({ is_bot_owner: false })]) {
    let denied = true;
    const view = await ready(async (url) => denied ? denial : response(url === "/api/me" ? { is_bot_owner: true } : state([record()])));
    assert.equal(view.timers.size, 0);
    assert.equal(view.elements.get("updates").classList.contains("hidden"), true);
    denied = false;
    await view.elements.get("refresh-status").listeners.click();
    assert.equal(view.elements.get("updates").classList.contains("hidden"), false);
    assert.equal(view.timers.size, 1);
  }
});

test("refresh retains controls and disclosures while actions use the newest component record", async () => {
  for (const component of ["api", "bot", "webui"]) {
    let item = record(component);
    let submitted;
    const view = await ready(async (url, options) => {
      if (url === "/api/me") return response({ is_bot_owner: true });
      if (url === "/api/updates/install") {
        submitted = JSON.parse(options.body);
        return response({ queued: true });
      }
      return response(state([item]));
    });
    const card = view.elements.get("update-cards");
    const install = updateButton(view, component);
    const select = find(card, (el) => el.tag === "select");
    const summary = find(card, (el) => el.tag === "summary");
    const browserSummary = find(view.elements.get("browser-version"), (el) => el.tag === "summary");
    item = { ...item, candidate: { ...item.candidate, digest: "sha256:" + "f".repeat(64) }, state: "queued", desired: { ...item.candidate } };
    await [...view.timers.values()][0].callback();
    assert.equal(updateButton(view, component), install);
    assert.equal(find(card, (el) => el.tag === "select"), select);
    assert.equal(find(card, (el) => el.tag === "summary"), summary);
    assert.equal(find(view.elements.get("browser-version"), (el) => el.tag === "summary"), browserSummary);
    assert.match(textOf(card), /Update pending/);
    await install.listeners.click();
    assert.equal(submitted.digest, item.candidate.digest);
    assert.equal(submitted.component, component);
  }
});

test("a policy edit abandoned after a failed prior read restores the recorded selection", async () => {
  let failRead;
  let block = false;
  let writes = 0;
  const view = await ready(async (url) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/policy") writes++;
    if (block) { block = false; return new Promise((_, reject) => { failRead = reject; }); }
    return response(state([record()]));
  });
  block = true;
  const reading = vm.runInContext("refresh()", view.context);
  const select = find(view.elements.get("update-cards"), (el) => el.tag === "select");
  select.value = "stable";
  const saving = select.listeners.change();
  failRead(new Error("offline"));
  await Promise.all([reading, saving]);
  assert.equal(writes, 0);
  assert.equal(select.value, "latest");
  await vm.runInContext("refresh()", view.context);
  assert.equal(select.value, "latest");
});

test("Database Usage shows independent physical databases and unknown allocation honestly", async () => {
  const calls = [];
  const view = page(async (url) => {
    calls.push(url);
    if (url === "/api/me") return { status: 200, ok: true, json: async () => ({ is_bot_owner: true }) };
    return { status: 200, ok: true, json: async () => ({ generated_at: Math.floor(Date.now() / 1000), databases: [
      { id: "core", kind: "core", usage: { allocated_bytes: 750000000, sampled_at: Math.floor(Date.now() / 1000) },
        percent_of_limit: 75, headroom_bytes: 250000000, status: "pressure" },
      { id: "a".repeat(32), kind: "guild", policy: { kind: "shared" }, usage: null,
        percent_of_limit: null, headroom_bytes: null, status: "unavailable" },
    ] }) };
  });
  await vm.runInContext('selectView("usage")', view.context);
  const text = textOf(view.elements.get("usage-cards"));
  assert.match(text, /750\.00 MB/);
  assert.match(text, /75\.00% of 1,000 MB/);
  assert.match(text, /Shared guild database/);
  assert.match(text, /Measurement unavailable/);
  assert.match(text, /Headroom: unavailable/);
  assert.ok(calls.includes("/api/database-usage"));
});

test("Database Usage does not paint cards if the tab hides during the fetch", async () => {
  let release;
  const usage = new Promise((resolve) => { release = resolve; });
  const view = page(async (url) => {
    if (url === "/api/me") return { status: 200, ok: true, json: async () => ({ is_bot_owner: true }) };
    await usage;
    return { status: 200, ok: true, json: async () => ({ databases: [{ id: "core", kind: "core", usage: { allocated_bytes: 1 }, percent_of_limit: 0, status: "normal" }], generated_at: 1 }) };
  });
  const pending = vm.runInContext('selectView("usage")', view.context);
  await new Promise((resolve) => setTimeout(resolve, 0));
  view.context.document.visibilityState = "hidden";
  release();
  await pending;
  assert.equal(textOf(view.elements.get("usage-cards")), "");
});

test("Database Usage marks retained cards stale when refresh fails", async () => {
  let fail = false;
  const view = page(async (url) => {
    if (url === "/api/me") return { status: 200, ok: true, json: async () => ({ is_bot_owner: true }) };
    if (fail) throw new Error("usage unavailable");
    return { status: 200, ok: true, json: async () => ({ databases: [{ id: "core", kind: "core", usage: { allocated_bytes: 1000000 }, percent_of_limit: 0.1, status: "normal" }], generated_at: 1 }) };
  });
  await vm.runInContext('selectView("usage")', view.context);
  fail = true;
  await vm.runInContext('refreshUsage()', view.context);
  const text = textOf(view.elements.get("usage-cards"));
  assert.match(text, /Stale: the latest usage refresh failed/);
  assert.match(view.elements.get("usage-status").textContent, /usage unavailable/);
});

test("Administration usage polling does not fetch while the tab is hidden", async () => {
  const calls = [];
  const view = page((url) => { calls.push(url); return new Promise(() => {}); }, { visibility: "hidden" });
  await vm.runInContext('selectView("usage")', view.context);
  assert.deepEqual(calls, []);
});

test("Database Usage coalesces overlapping visible refreshes", async () => {
  const calls = [];
  let release;
  const usage = new Promise((resolve) => { release = resolve; });
  const view = page(async (url) => {
    calls.push(url);
    if (url === "/api/me") return { status: 200, ok: true, json: async () => ({ is_bot_owner: true }) };
    await usage;
    return { status: 200, ok: true, json: async () => ({ databases: [], generated_at: 1 }) };
  });
  const first = vm.runInContext('selectView("usage")', view.context);
  const second = vm.runInContext('refreshUsage()', view.context);
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(calls.filter((url) => url === "/api/database-usage").length, 1);
  release();
  await Promise.all([first, second]);
});

test("Database Usage renders disjoint physical groups and unknown guard status", () => {
  const view = page();
  const card = vm.runInContext(`renderUsage(${JSON.stringify({
    id: "a".repeat(32), kind: "guild", role: "guild", availability: "available",
    policy: { kind: "shared" }, status: "normal", percent_of_limit: 1,
    usage: { allocated_bytes: 10000000, breakdown_quality: "measured", category_sampled_at: 1,
      categories: { snapshot_data_bytes: 1000000, moderation_evidence_data_bytes: 2000000,
        configuration_data_bytes: 1000000, operational_data_bytes: 2000000,
        index_bytes: 1000000, reusable_bytes: 1000000, system_unattributed_bytes: 2000000 } },
    capture: { status: "unknown", configured_retention_hours: 24 },
    application_admission: { bot_cross_process_guard: "unknown" },
  })})`, view.context);
  const content = textOf(card);
  for (const label of ["Snapshot table", "Moderation and evidence tables", "Configuration tables",
    "Operational tables", "Indexes", "Reusable pages", "System / unattributed",
    "Bot growth guard: unknown", "External evidence file bytes: unavailable"]) {
    assert.match(content, new RegExp(label.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
  }
});

test("schema upgrades expose the paired action and active pairs block independent installs", () => {
  const view = page();
  const api = record();
  api.observed.max_schema = 16;
  api.observed.guild_schema = 14;
  api.candidate.result_schema = 17;
  api.candidate.upgrade = { guild_schema: 15 };
  render(view, api);
  vm.runInContext(`snapshot.coordinated_upgrade_path = "/updates/recovery";
    snapshot.compatibility.api = { installable: false, reason: "schema changes require pairing" }; render();`, view.context);
  assert.equal(updateButton(view, "api").disabled, true);
  const link = find(view.elements.get("update-cards"), item => item.tag === "a" && item.textContent.includes("together"));
  assert.ok(link?.href.endsWith("/updates/recovery"));
  assert.ok(find(view.elements.get("update-cards"), item => item.textContent.includes("both services will briefly stop")));
  vm.runInContext(`snapshot.compatibility.api.installable = true;
    snapshot.service_upgrade = { phase: "preparing", error: null }; render();`, view.context);
  assert.equal(updateButton(view, "api").disabled, true);
  assert.equal(updateButton(view, "api", true).disabled, true);
});

test("normal update explains a required launcher step and separates optional launcher updates", () => {
  const view = page();
  render(view, record());
  vm.runInContext(`snapshot.launchers = { api: { state: "required_first", installable: false,
    reason: "Update will install the required launcher first, then the application." } }; render();`, view.context);
  const card = view.elements.get("update-cards");
  assert.match(textOf(card), /required launcher first/);
  assert.ok(find(card, (node) => node.tag === "button" && node.textContent === "Update"));
  const advanced = find(card, (node) => node.tag === "details" && textOf(node).includes("Advanced launcher controls"));
  assert.ok(advanced);
  assert.equal(find(advanced, (node) => node.tag === "button").disabled, true);
  vm.runInContext(`snapshot.launchers.api = { state: "satisfied", installable: false,
    reason: "Current launcher supports this application; no launcher update is required." }; render();`, view.context);
  assert.match(textOf(card), /no launcher update is required/);
  assert.equal(find(advanced, (node) => node.tag === "button").disabled, true);
  vm.runInContext(`snapshot.compatibility.api.installable = false;
    snapshot.launchers.api = { state: "optional", installable: true,
    reason: "Current launcher supports this application. A newer launcher is optional." }; render();`, view.context);
  assert.equal(find(advanced, (node) => node.tag === "button").disabled, false,
    "launcher-only admission is independent of the bundled application's schema");
});
