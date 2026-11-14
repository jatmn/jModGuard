#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import test from "node:test";
import vm from "node:vm";

import { JSDOM } from "../tools/webui/node_modules/jsdom/lib/api.js";
import React, { useSyncExternalStore, useEffect, useState } from "../tools/webui/node_modules/react/index.js";
import { createRoot as reactRoot } from "../tools/webui/node_modules/react-dom/client.js";
import { flushSync } from "../tools/webui/node_modules/react-dom/index.js";
import { afterEach } from "node:test";

const stateSource = fs.readFileSync(new URL("../frontend/state.js", import.meta.url), "utf8")
  .replace(/^import .*;$/gm, "").replaceAll("export function", "function");
const appearanceSource = fs.readFileSync(new URL('../frontend/appearance.js', import.meta.url), 'utf8')
  .replace(/^import .*;$/gm, '').replaceAll('export function', 'function');
const chartsSource = fs.readFileSync(new URL("../frontend/charts.js", import.meta.url), "utf8")
  .replace(/^import .*;$/gm, "").replace(/^const h = React\.createElement;$/m, "")
  .replaceAll("export const", "const").replaceAll("export function", "function");
const iconsSource = fs.readFileSync(new URL("../frontend/icons.js", import.meta.url), "utf8")
  .replace(/^import .*;$/gm, "").replaceAll("export const", "const").replaceAll("export function", "function");
const source = fs.readFileSync(path.resolve(import.meta.dirname, "../frontend/administration.js"), "utf8")
  .replace(/^import .*;$/gm, "");
const NOW_SECONDS = 1800000000;
const pages = [];
afterEach(() => {
  for (const { dom, roots } of pages.splice(0)) {
    globalThis.window = dom.window; globalThis.document = dom.window.document;
    for (const root of roots) flushSync(() => root.unmount());
    dom.window.close();
  }
});

function page(fetchImpl = () => new Promise(() => {}), { apiBase = "", origin = "https://ui.example.test", visibility = "visible", now = () => NOW_SECONDS * 1000 } = {}) {
  const dom = new JSDOM('<!doctype html><html><head><meta name="theme-color"></head><body><main id="content"></main></body></html>', { url: origin });
  const { window } = dom, { document } = window;
  Object.defineProperty(document, "visibilityState", { value: visibility, writable: true });
  Object.defineProperty(window, "localStorage", { get() { throw new Error("storage disabled"); } });
  window.JMODGUARD_WEBUI_SHA = "a".repeat(40);
  window.JMODGUARD_API_BASE = apiBase;
  window.matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {} });
  const roots = [];
  const timers = new Map();
  let nextTimer = 0;
  const context = vm.createContext({
    document, window, React, useSyncExternalStore, useEffect, useState,
    createRoot(container) {
      globalThis.window = window; globalThis.document = document;
      const root = reactRoot(container); roots.push(root);
      return { render(element) {
        globalThis.window = window; globalThis.document = document;
        flushSync(() => root.render(element));
      } };
    },
    fetch: fetchImpl,
    setTimeout(callback, delay) { timers.set(++nextTimer, { callback, delay }); return nextTimer; },
    clearTimeout(id) { timers.delete(id); },
    Date: { now },
  });
  pages.push({ dom, roots });
  vm.runInContext(stateSource + "\n" + appearanceSource + "\n" + chartsSource + "\n" + iconsSource + "\n" + source, context);
  return { context, elements: { get: id => document.getElementById(id) }, timers };
}

function run(script, context) {
  let result;
  flushSync(() => { result = vm.runInContext(script, context); });
  return result;
}

function fire(element, type) {
  globalThis.window = element.ownerDocument.defaultView;
  globalThis.document = element.ownerDocument;
  flushSync(() => element.dispatchEvent(new window.Event(type, { bubbles: true })));
  return new Promise(resolve => setImmediate(resolve));
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
  return element.textContent;
}

function record(component = "api") {
  const sha = "a".repeat(40);
  return {
    component, channel: "latest", cadence: "off",
    candidate: { component, channel: "latest", source_sha: "b".repeat(40),
      digest: "sha256:" + "b".repeat(64), release_id: "v1", published_at: "2026-09-23T00:00:00Z" },
    installed: null, checked_at: NOW_SECONDS, check_error: null,
    observed_at: NOW_SECONDS, state: "running", last_error: null,
    observed: { source_sha: sha, base_image_sha: sha, executable_sha256: "c".repeat(64),
      started_at: NOW_SECONDS, self_update_enabled: true },
  };
}

function componentCard(pageState, component) {
  const cards = pageState.elements.get("update-cards");
  const card = Array.from(cards.children).find((item) =>
    find(item, (child) => child.localName === "h2")?.textContent === component.toUpperCase());
  assert.ok(card);
  return card;
}

function updateButton(pageState, component, launcher = false) {
  const card = componentCard(pageState, component);
  return find(card, (item) => item.localName === "button" && item.textContent === (launcher ? "Update launcher" : component === "webui" ? "Update WebUI" : "Update"));
}

// The card's status badge (its SVG icon name, then its text), primary information, and technical details.
function badgeParts(badge) {
  const icon = find(badge, (item) => item.localName === "svg");
  assert.equal(icon?.getAttribute("aria-hidden"), "true", "status badge icons are decorative SVGs");
  return `${icon.getAttribute("data-icon")} ${textOf(badge)}`;
}
function cardParts(pageState, component) {
  const card = componentCard(pageState, component);
  return {
    badge: badgeParts(find(card, (item) => item.classList?.contains("administration-badge"))),
    content: textOf(find(card, (item) => item.classList?.contains("administration-info"))),
    details: textOf(find(card, (item) => item.localName === "details")),
  };
}

function render(pageState, item) {
  const snapshot = {
    launchers: { [item.component]: { state: "optional", installable: true, reason: "Optional launcher update" } },
    components: [item], compatibility: { [item.component]: { installable: true } },
    apply_enabled: true, unattended_enabled: false, webui_storage_configured: true,
  };
  run("ui.set({ snapshot: " + JSON.stringify(snapshot) + ", freshness: 'fresh', access: 'allowed' });", pageState.context);
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
  api.checked_at = NOW_SECONDS - 7201;
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
  await run("refresh()", view.context);
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
    run(`request(${JSON.stringify(path)}, {})`, remote.context);
  }
  assert.deepEqual(calls, ["/api/updates", "/api/updates", "/api/updates/check", "/api/updates/install", "/api/updates/policy"].map((path) => ({
    url: `https://api.example.test${path}`, method: "POST", credentials: "include",
  })));
  assert.equal(remote.elements.get("sign-in-link").href,
    "https://api.example.test/login?return_to=https%3A%2F%2Fui.example.test");

  const localCalls = [];
  const local = page((url, options) => {
    localCalls.push({ url, credentials: options.credentials });
    return new Promise(() => {});
  });
  assert.deepEqual(localCalls, [{ url: "/api/updates", credentials: "include" }]);
  assert.equal(local.elements.get("sign-in-link").href, "https://ui.example.test/login?return_to=%2Fadministration%2Findex.html");
  assert.ok(local.elements.get("sign-in-link"));
});

test("owner availability distinguishes baked source parity from channel mismatch", () => {
  const view = page();
  const api = record();
  api.candidate.source_sha = api.observed.source_sha;
  render(view, api);
  const availability = () => {
    const card = view.elements.get("update-cards").children[0];
    return textOf(find(card, (element) => element.localName === "p" && textOf(element).startsWith("Availability:")));
  };
  assert.match(availability(), /up to date/);
  assert.match(cardParts(view, "api").details, /Image digest: unverified/);
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

  // A baked process with the same source SHA has no verified install digest,
  // but it already runs the available source; only the optional launcher remains.
  api.installed = null;
  api.installed_launcher = null;
  api.candidate.source_sha = api.observed.source_sha;
  render(view, api);
  assert.equal(updateButton(view, "api").disabled, true);
  assert.equal(updateButton(view, "api", true).disabled, false);

  // A WebUI owner can retry a same-digest publication to repair Storage.
  const webui = record("webui");
  webui.installed = { ...webui.candidate };
  render(view, webui);
  assert.equal(updateButton(view, "webui").disabled, false);
});

test("a directly deployed bot or API on the available source is up to date and cannot be updated again", () => {
  for (const component of ["bot", "api"]) {
    const view = page();
    const item = record(component);
    // A direct image deploy runs the base image's baked build and records no managed install.
    item.candidate.source_sha = item.observed.source_sha;
    render(view, item);
    const { badge, content, details } = cardParts(view, component);
    assert.equal(badge, "check-circle Up to date", component);
    assert.match(content, /Availability: up to date/, component);
    assert.doesNotMatch(content, /unverified/, component);
    assert.match(details, /Image digest: unverified/, component);
    assert.equal(updateButton(view, component).disabled, true, component);
  }
});

test("a directly deployed bot or API on the available source stays updatable while pending or failed", () => {
  for (const component of ["bot", "api"]) {
    const view = page();
    const queued = record(component);
    queued.candidate.source_sha = queued.observed.source_sha;
    // The owner can supersede a queued request for another digest.
    queued.state = "queued";
    queued.desired = { ...queued.candidate, digest: "sha256:" + "e".repeat(64) };
    render(view, queued);
    assert.equal(cardParts(view, component).badge, "refresh Update pending", component);
    assert.equal(updateButton(view, component).disabled, false, component);

    const failed = record(component);
    failed.candidate.source_sha = failed.observed.source_sha;
    failed.state = "failed";
    render(view, failed);
    assert.equal(cardParts(view, component).badge, "alert-circle Needs attention", component);
    assert.equal(updateButton(view, component).disabled, false, component);
  }
});

test("owner update badges keep real and unknown service updates distinct from current releases", () => {
  const view = page();
  const bot = record("bot");
  // Self-updated: the managed install digest matches the candidate.
  bot.installed = { ...bot.candidate };
  bot.observed.source_sha = bot.candidate.source_sha;
  render(view, bot);
  assert.equal(cardParts(view, "bot").badge, "check-circle Up to date");
  assert.doesNotMatch(cardParts(view, "bot").details, /Image digest/);
  assert.equal(updateButton(view, "bot").disabled, true);

  // Direct deploy of an older source: a real update.
  const direct = record("bot");
  render(view, direct);
  assert.equal(cardParts(view, "bot").badge, "arrow-up-circle Update available");
  assert.doesNotMatch(cardParts(view, "bot").details, /Image digest/);
  assert.equal(updateButton(view, "bot").disabled, false);

  // Unknown running source or an unconfirmed candidate never claims up to date.
  const unobserved = record("bot");
  unobserved.candidate.source_sha = unobserved.observed.source_sha;
  unobserved.observed_at = NOW_SECONDS - 7201;
  render(view, unobserved);
  assert.equal(cardParts(view, "bot").badge, "arrow-up-circle Check status");
  assert.match(cardParts(view, "bot").content, /Availability: unknown/);
  assert.equal(updateButton(view, "bot").disabled, true);
  const unchecked = record("bot");
  unchecked.candidate.source_sha = unchecked.observed.source_sha;
  unchecked.checked_at = NOW_SECONDS - 7201;
  render(view, unchecked);
  assert.equal(cardParts(view, "bot").badge, "arrow-up-circle Check status");
  assert.equal(updateButton(view, "bot").disabled, true);
  const missing = record("bot");
  missing.candidate = null;
  render(view, missing);
  assert.equal(cardParts(view, "bot").badge, "arrow-up-circle Check status");
  assert.equal(updateButton(view, "bot").hidden, true);
});

test("Update WebUI is disabled only while the WebUI card is up to date", () => {
  const view = page();
  const webui = record("webui");
  webui.installed = { ...webui.candidate };
  webui.observed.source_sha = webui.candidate.source_sha;
  render(view, webui);
  assert.equal(cardParts(view, "webui").badge, "check-circle Up to date");
  assert.equal(updateButton(view, "webui").disabled, true);

  // #578: Storage still serves an older release than the candidate.
  const stale = record("webui");
  stale.installed = { source_sha: stale.observed.source_sha, digest: "sha256:" + "d".repeat(64) };
  render(view, stale);
  assert.equal(cardParts(view, "webui").badge, "arrow-up-circle Update available");
  assert.equal(updateButton(view, "webui").disabled, false);
});

function response(value, status = 200) {
  return { status, ok: status === 200, json: async () => value };
}
function state(items) {
  return { launchers: Object.fromEntries(items.map((item) => [item.component, { state: "optional", installable: true, reason: "Optional launcher update" }])), components: items, compatibility: Object.fromEntries(items.map((item) => [item.component, { installable: true }])),
    apply_enabled: true, unattended_enabled: true, webui_storage_configured: true };
}

async function ready(fetchImpl, options) {
  const view = page(fetchImpl, options);
  await run("refresh()", view.context);
  return view;
}

test("check applies its returned snapshot and reports failures without leaving enabled stale controls", async () => {
  const api = record();
  let unavailable = false;
  const view = await ready(async (url) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/check") {
      api.candidate = { ...api.candidate, source_sha: "d".repeat(40), release_id: "v2" };
      return response(state([api]));
    }
    if (unavailable) throw new Error("offline");
    return response(state([api]));
  });
  const check = find(view.elements.get("update-cards"), (item) => item.localName === "button" && item.textContent === "Check now");
  await fire(check, "click");
  assert.match(textOf(view.elements.get("update-cards")), /Available release: v2/);
  unavailable = true;
  await run("refresh()", view.context);
  assert.equal(updateButton(view, "api").disabled, true);
  assert.match(view.elements.get("update-status").textContent, /offline/);
  unavailable = false;
  await run("refresh()", view.context);
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
    await fire(updateButton(view, component, launcher), "click");
    assert.match(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.equal(updateButton(view, component, launcher).disabled, true);
    assert.equal([...view.timers.values()][0].delay, 2000);
    if (launcher) item.installed_launcher = item.candidate;
    else { item.state = "running"; item.installed = item.candidate; item.observed.source_sha = item.candidate.source_sha; }
    await [...view.timers.values()][0].callback();
    assert.doesNotMatch(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.doesNotMatch(view.elements.get("update-status").textContent, /Waiting for the component/);
    assert.equal([...view.timers.values()][0].delay, 30000);
    assert.equal(calls.filter((call) => call.url === "/api/me").length, 0);
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
      await fire(updateButton(view, component, launcher), "click");
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
  await fire(updateButton(view, "webui"), "click");
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
  const first = run("refresh()", view.context);
  const second = run("refresh()", view.context);
  assert.equal(first, second);
  const action = fire(updateButton(view, "api"), "click");
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
  await run("refresh()", view.context);
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
  const [channel, cadence] = findAll(view.elements.get("update-cards"), (element) => element.localName === "select");
  block = true;
  const reading = run("refresh()", view.context);
  channel.value = "nightly";
  const savingChannel = fire(channel, "change");
  item.cadence = "weekly";
  releaseRead(response(state([item])));
  await Promise.all([reading, savingChannel]);
  assert.deepEqual(saved, { component: "bot", channel: "nightly", cadence: "weekly" });

  block = true;
  const readingAgain = run("refresh()", view.context);
  cadence.value = "monthly";
  const savingCadence = fire(cadence, "change");
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
  const reading = run("refresh()", view.context);
  const saving = fire(updateButton(view, "api"), "click");
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
  const channel = find(view.elements.get("update-cards"), (element) => element.localName === "select");
  channel.value = "latest";
  await fire(channel, "change");
  assert.deepEqual(saved, { component: "bot", channel: "latest", cadence: "off" });
});

test("pending installs surface terminal failures and leave Check now available for recovery", async () => {
  const item = record("webui");
  item.state = "queued";
  const view = await ready(async (url) => response(url === "/api/me" ? { is_bot_owner: true } : state([item])));
  const check = find(view.elements.get("update-cards"), (element) => element.localName === "button" && element.textContent === "Check now");
  assert.equal(check.disabled, false);
  item.state = "failed";
  item.last_error = "Storage installation interrupted";
  await [...view.timers.values()][0].callback();
  assert.match(textOf(view.elements.get("update-cards")), /Needs attention/);
  assert.match(textOf(view.elements.get("update-cards")), /Install failed: Storage installation interrupted/);
  assert.equal(updateButton(view, "webui").disabled, false);
});

test("a pending CDN purge keeps a reconciled WebUI release in Needs attention", () => {
  const view = page();
  const webui = record("webui");
  webui.installed = { ...webui.candidate };
  webui.observed = { ...webui.observed, source_sha: webui.candidate.source_sha };
  const show = (pending) => run("ui.set({ snapshot: " + JSON.stringify({
    components: [webui], compatibility: { webui: { installable: true } },
    apply_enabled: true, unattended_enabled: false, webui_storage_configured: true,
    webui_purge_pending: pending,
  }) + ", freshness: 'fresh', access: 'allowed' });", view.context);
  show(null);
  assert.match(textOf(view.elements.get("update-cards")), /Up to date/);
  assert.equal(updateButton(view, "webui").disabled, true);
  show("Bunny Pull Zone purge failed after 6 attempts");
  const text = textOf(view.elements.get("update-cards"));
  assert.match(text, /Needs attention/);
  assert.match(text, /CDN purge.*Pending: Bunny Pull Zone purge failed after 6 attempts\. Check now retries the purge\./);
  assert.equal(updateButton(view, "webui").disabled, false);
});

test("a failed policy save restores the server's channel and displays the error", async () => {
  const item = record("api");
  const view = await ready(async (url) => {
    if (url === "/api/me") return response({ is_bot_owner: true });
    if (url === "/api/updates/policy") return response({ error: "policy rejected" }, 400);
    return response(state([item]));
  });
  const select = find(view.elements.get("update-cards"), (element) => element.localName === "select");
  select.value = "nightly";
  await fire(select, "change");
  assert.equal(find(view.elements.get("update-cards"), (element) => element.localName === "select").value, "latest");
  assert.match(view.elements.get("update-status").textContent, /policy rejected/);
});

test("appearance changes even when browser storage is unavailable", () => {
  const view = page();
  const select = run(`document.querySelector('select[aria-label="Appearance"]')`, view.context);
  select.value = "dark";
  fire(select, "change");
  assert.equal(run("document.documentElement.dataset.theme", view.context), "dark");
  select.value = "light";
  fire(select, "change");
  assert.equal(run("document.documentElement.dataset.theme", view.context), "light");
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
  const now = NOW_SECONDS;
  const view = page(undefined, { now: () => now * 1000 });
  for (const [seconds, expected] of [[0, "0s"], [38, "38s"], [818, "13m 38s"],
    [3608, "1h 0m 8s"]]) {
    assert.equal(run(`uptime(${now - seconds})`, view.context), expected);
  }
  assert.equal(run(`uptime(${now - 94418})`, view.context), "1d 2h 13m 38s");
  assert.equal(run("uptime(null)", view.context), "unknown");
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
    await run("refresh()", view.context);
    assert.match(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.match(textOf(view.elements.get("update-cards")), /launcher update: download failed/);
    assert.equal([...view.timers.values()][0].delay, 2000);
    assert.equal(updateButton(view, component).disabled, true);
    assert.equal(updateButton(view, component, true).disabled, false);
    // Owner can supersede a still-active application request with a different digest.
    item.candidate = { ...item.candidate, digest: "sha256:" + "e".repeat(64) };
    await run("refresh()", view.context);
    assert.equal(updateButton(view, component).disabled, false);
    item.installed = { ...item.desired };
    item.state = "running";
    item.last_error = null;
    await run("refresh()", view.context);
    assert.doesNotMatch(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.equal([...view.timers.values()][0].delay, 30000);
    // The inverse outcome keeps a launcher pending after application failure.
    item.desired = null;
    item.desired_launcher = { ...item.candidate };
    item.state = "failed";
    item.last_error = "application download failed";
    await run("refresh()", view.context);
    assert.match(textOf(view.elements.get("update-cards")), /Update pending/);
    assert.match(textOf(view.elements.get("update-cards")), /application download failed/);
    assert.equal([...view.timers.values()][0].delay, 2000);
    item.installed_launcher = { ...item.desired_launcher };
    await run("refresh()", view.context);
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
  const details = () => find(view.elements.get("browser-version"), (element) => element.localName === "details");
  details().open = true;
  await run("refresh()", view.context);
  assert.equal(details().open, true);
  details().open = false;
  await run("refresh()", view.context);
  assert.equal(details().open, false);
});

test("transient initial access checks retry automatically but confirmed denials do not", async () => {
  for (const failure of ["network", "server"]) {
    let calls = 0;
    const view = await ready(async (url) => {
      if (url === "/api/updates" && ++calls === 1) {
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
  for (const denial of [response({}, 401), response({}, 403), response({ error: "bot owner required" }, 403)]) {
    let denied = true;
    const view = await ready(async (url) => denied ? denial : response(url === "/api/me" ? { is_bot_owner: true } : state([record()])));
    assert.equal(view.timers.size, 0);
    assert.equal(view.elements.get("updates").classList.contains("hidden"), true);
    denied = false;
    await fire(view.elements.get("refresh-status"), "click");
    assert.equal(view.elements.get("updates").classList.contains("hidden"), false);
    assert.equal(view.timers.size, 1);
  }
});

test("Discord rate limits delay the next administration refresh", async () => {
  let now = NOW_SECONDS * 1000;
  const view = await ready(async (url) => {
    if (url === "/api/updates") {
      return {
        status: 429,
        ok: false,
        headers: { get: (name) => (name === "retry-after" ? "45" : null) },
        json: async () => ({ error: "limited", code: "discord_rate_limited" }),
      };
    }
    return response(state([record()]));
  }, { now: () => now });
  assert.equal(view.elements.get("updates").classList.contains("hidden"), true);
  assert.equal(view.elements.get("sign-in").classList.contains("hidden"), true);
  assert.equal(view.timers.size, 1);
  assert.equal([...view.timers.values()][0].delay, 45000);
  now += 1;
  run("scheduleRefresh()", view.context);
  assert.equal([...view.timers.values()][0].delay, 44999);
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
    const select = find(card, (el) => el.localName === "select");
    const summary = find(card, (el) => el.localName === "summary");
    const browserSummary = find(view.elements.get("browser-version"), (el) => el.localName === "summary");
    item = { ...item, candidate: { ...item.candidate, source_sha: "f".repeat(40), digest: "sha256:" + "f".repeat(64) }, state: "queued", desired: { ...item.candidate } };
    await [...view.timers.values()][0].callback();
    assert.equal(updateButton(view, component), install);
    assert.equal(find(card, (el) => el.localName === "select"), select);
    assert.equal(find(card, (el) => el.localName === "summary"), summary);
    assert.equal(find(view.elements.get("browser-version"), (el) => el.localName === "summary"), browserSummary);
    assert.match(textOf(card), /Update pending/);
    assert.equal(install.disabled, false);
    await fire(install, "click");
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
  const reading = run("refresh()", view.context);
  const select = find(view.elements.get("update-cards"), (el) => el.localName === "select");
  select.value = "stable";
  const saving = fire(select, "change");
  failRead(new Error("offline"));
  await Promise.all([reading, saving]);
  assert.equal(writes, 0);
  assert.equal(select.value, "latest");
  await run("refresh()", view.context);
  assert.equal(select.value, "latest");
});

test("schema upgrades expose the paired action and active pairs block independent installs", () => {
  const view = page();
  const api = record();
  api.observed.max_schema = 16;
  api.observed.guild_schema = 14;
  api.candidate.result_schema = 17;
  api.candidate.upgrade = { guild_schema: 15 };
  render(view, api);
  run(`ui.getSnapshot().snapshot.coordinated_upgrade_path = "/api/service-upgrade/recovery";
    ui.getSnapshot().snapshot.compatibility.api = { installable: false, reason: "schema changes require pairing" }; ui.set({ ...ui.getSnapshot() });`, view.context);
  assert.equal(updateButton(view, "api").disabled, true);
  const link = find(view.elements.get("update-cards"), item => item.localName === "a" && item.textContent.includes("together"));
  assert.ok(link?.href.endsWith("/api/service-upgrade/recovery"));
  assert.ok(find(view.elements.get("update-cards"), item => item.textContent.includes("both services will briefly stop")));
  run(`ui.getSnapshot().snapshot.compatibility.api.installable = true;
    ui.getSnapshot().snapshot.service_upgrade = { phase: "preparing", error: null }; ui.set({ ...ui.getSnapshot() });`, view.context);
  assert.equal(updateButton(view, "api").disabled, true);
  assert.equal(updateButton(view, "api", true).disabled, true);
});

test("normal update explains a required launcher step and separates optional launcher updates", () => {
  const view = page();
  render(view, record());
  run(`ui.getSnapshot().snapshot.launchers = { api: { state: "required_first", installable: false,
    reason: "Update will install the required launcher first, then the application." } }; ui.set({ ...ui.getSnapshot() });`, view.context);
  const card = view.elements.get("update-cards");
  assert.match(textOf(card), /required launcher first/);
  assert.ok(find(card, (node) => node.localName === "button" && node.textContent === "Update"));
  const advanced = find(card, (node) => node.localName === "details" && textOf(node).includes("Advanced launcher controls"));
  assert.ok(advanced);
  assert.equal(find(advanced, (node) => node.localName === "button").disabled, true);
  run(`ui.getSnapshot().snapshot.launchers.api = { state: "satisfied", installable: false,
    reason: "Current launcher supports this application; no launcher update is required." }; ui.set({ ...ui.getSnapshot() });`, view.context);
  assert.match(textOf(card), /no launcher update is required/);
  assert.equal(find(advanced, (node) => node.localName === "button").disabled, true);
  run(`ui.getSnapshot().snapshot.compatibility.api.installable = false;
    ui.getSnapshot().snapshot.launchers.api = { state: "optional", installable: true,
    reason: "Current launcher supports this application. A newer launcher is optional." }; ui.set({ ...ui.getSnapshot() });`, view.context);
  assert.equal(find(advanced, (node) => node.localName === "button").disabled, false,
    "launcher-only admission is independent of the bundled application's schema");
});


test("boot mismatch shows expected and actual startup independently of installed state", () => {
  const view = page();
  render(view, record());
  run(`ui.getSnapshot().snapshot.boot_mismatches = { api: {
    component: "api", running: "", expected: { release_id: "retained-release", source_sha: "expected-sha" },
    reason: "image signature rejected"
  } }; ui.set({ ...ui.getSnapshot() });`, view.context);
  let text = textOf(view.elements.get("update-cards"));
  assert.match(text, /Boot mismatch/);
  assert.match(text, /retained-release/);
  assert.match(text, /expected-sha/);
  assert.match(text, /Application stopped/);
  assert.match(text, /image signature rejected/);
  run(`ui.getSnapshot().snapshot.boot_mismatches.api.running = "baked-sha"; ui.set({ ...ui.getSnapshot() });`, view.context);
  text = textOf(view.elements.get("update-cards"));
  assert.match(text, /baked-sha/);
  assert.doesNotMatch(text, /Application stopped/);
  run(`ui.getSnapshot().snapshot.boot_mismatches = {}; ui.set({ ...ui.getSnapshot() });`, view.context);
  assert.doesNotMatch(textOf(view.elements.get("update-cards")), /Boot mismatch/);
});

test("owner saves the boot repair alert channel as a string ID through the API", async () => {
  let alertChannel = null;
  const saved = [];
  const view = await ready(async (url, options) => {
    if (url === "/api/updates/alert-channel") {
      saved.push(JSON.parse(options.body));
      alertChannel = saved.at(-1).channel_id || null;
    }
    return response({ ...state([record("bot")]), alert_channel: alertChannel });
  });
  assert.match(view.elements.get("alert-channel-unset").textContent, /No alert channel is set/);
  // #620: one short sentence explains the card.
  const intro = view.elements.get("alert-channel-unset").previousElementSibling.textContent;
  assert.equal(intro, "The bot posts here once when boot repair can't restore the recorded bot or API release.");
  const input = view.elements.get("alert-channel-id");
  // React loads before this harness creates a DOM, so it cannot observe native input events.
  const setValue = (element, value) => flushSync(() => element[Object.keys(element).find(key => key.startsWith("__reactProps"))].onChange({ target: { value } }));
  setValue(input, " 1234567890123456789 ");
  await fire(view.elements.get("save-alert-channel"), "click");
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(saved, [{ channel_id: "1234567890123456789" }]);
  assert.equal(view.elements.get("alert-channel-unset"), null);
  assert.equal(view.elements.get("alert-channel-id").value, "1234567890123456789");
  assert.equal(view.elements.get("save-alert-channel").disabled, true);
  setValue(input, "");
  await fire(view.elements.get("save-alert-channel"), "click");
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(saved.at(-1), { channel_id: "" });
  assert.ok(view.elements.get("alert-channel-unset"));
});

// #613: Administration is for update processing only. Database Usage and Platform
// analytics moved to the workspace Owner section.
test("Administration offers only Updates and never reads usage or platform analytics", async () => {
  const calls = [];
  const view = await ready(async (url) => {
    calls.push(url);
    return response(url === "/api/me" ? { is_bot_owner: true } : state([record()]));
  });
  const body = view.elements.get("content");
  assert.equal(view.elements.get("admin-title").textContent, "Updates");
  assert.equal(body.querySelector("nav"), null, "no Administration page navigation remains");
  for (const id of ["show-updates", "show-database-usage", "show-analytics", "database-usage", "platform-analytics"])
    assert.equal(view.elements.get(id), null, id);
  assert.doesNotMatch(textOf(body), /Database Usage|Analytics/);
  assert.deepEqual([...new Set(calls)], ["/api/updates"]);
});

// #588: the Administration page controls draw the shared SVG icon set.
test("Administration page controls show colored SVG icons, never glyphs", async () => {
  const view = await ready(async (url) => response(url === "/api/me" ? { is_bot_owner: true } : state([record()])));
  for (const [id, label, icon, intent] of [
    ["refresh-status", "Refresh status", "refresh", "info"],
  ]) {
    const control = view.elements.get(id);
    assert.equal(textOf(control), label, `${id} keeps its label without a glyph`);
    const svg = find(control, (item) => item.localName === "svg");
    assert.equal(svg?.getAttribute("aria-hidden"), "true", id);
    assert.equal(svg.getAttribute("data-icon"), icon, id);
    assert.ok(svg.classList.contains(`icon-${intent}`), `${id} is drawn in its ${intent} color`);
  }
  const back = find(view.elements.get("refresh-status").parentElement, (item) => item.localName === "a" && textOf(item) === "Back to workspace");
  assert.equal(find(back, (item) => item.localName === "svg")?.getAttribute("data-icon"), "arrow-left");
});
