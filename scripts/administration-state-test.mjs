#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import vm from "node:vm";

class Element {
  constructor(tag = "div") {
    this.tag = tag;
    this.children = [];
    this.textContent = "";
    this.disabled = false;
    const classes = new Set();
    this.classList = {
      add(name) { classes.add(name); },
      remove(name) { classes.delete(name); },
      toggle(name, forced) { if (forced) classes.add(name); else classes.delete(name); },
      contains(name) { return classes.has(name); },
    };
  }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  replaceWith() {}
  addEventListener() {}
  setAttribute() {}
}

const source = fs.readFileSync(path.resolve(import.meta.dirname, "../frontend/administration.js"), "utf8");

function page(fetchImpl = () => new Promise(() => {}), { apiBase = "", origin = "https://ui.example.test", visibility = "visible" } = {}) {
  const elements = new Map();
  let interval;
  const document = {
    visibilityState: visibility,
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
  const context = vm.createContext({
    document,
    window: { JMODGUARD_WEBUI_SHA: "a".repeat(40), JMODGUARD_API_BASE: apiBase, location: { origin } },
    fetch: fetchImpl,
    setInterval(callback) { interval = callback; },
    Date,
  });
  vm.runInContext(source, context);
  return { context, elements, interval };
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
  return find(card, (item) => item.tag === "button" && item.textContent === (launcher ? "Update launcher" : component === "webui" ? "Update WebUI" : "Update application"));
}

function render(pageState, item) {
  const snapshot = {
    components: [item], compatibility: { [item.component]: { installable: true } },
    apply_enabled: true, unattended_enabled: false, webui_storage_configured: true,
  };
  vm.runInContext("snapshot = " + JSON.stringify(snapshot) + "; render();", pageState.context);
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

test("Administration polling stops while the tab is hidden", async () => {
  const calls = [];
  const view = page((url) => { calls.push(url); return new Promise(() => {}); }, { visibility: "hidden" });
  await vm.runInContext('selectView("usage")', view.context);
  view.interval();
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
