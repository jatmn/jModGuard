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
    this.classList = { add() {}, remove() {} };
  }
  append(...children) { this.children.push(...children); }
  replaceChildren(...children) { this.children = children; }
  addEventListener() {}
  setAttribute() {}
}

const source = fs.readFileSync(path.resolve(import.meta.dirname, "../frontend/administration.js"), "utf8");

function page() {
  const elements = new Map();
  const document = {
    createElement: (tag) => new Element(tag),
    getElementById(id) {
      if (!elements.has(id)) elements.set(id, new Element());
      return elements.get(id);
    },
  };
  const context = vm.createContext({
    document,
    window: { JMODGUARD_WEBUI_SHA: "a".repeat(40) },
    fetch: () => new Promise(() => {}),
    setInterval() {},
    Date,
  });
  vm.runInContext(source, context);
  return { context, elements };
}

function find(element, predicate) {
  if (predicate(element)) return element;
  for (const child of element.children) {
    const result = find(child, predicate);
    if (result) return result;
  }
  return null;
}

function record(component = "api") {
  const sha = "a".repeat(40);
  return {
    component, channel: "stable", cadence: "off",
    candidate: { component, channel: "stable", source_sha: "b".repeat(40),
      digest: "sha256:" + "b".repeat(64), release_id: "v1", published_at: "2026-09-23T00:00:00Z" },
    installed: null, checked_at: Math.floor(Date.now() / 1000), check_error: null,
    observed_at: Math.floor(Date.now() / 1000), state: "running", last_error: null,
    observed: { source_sha: sha, base_image_sha: sha, executable_sha256: "c".repeat(64),
      started_at: Math.floor(Date.now() / 1000), self_update_enabled: true },
  };
}

function updateButton(pageState, component) {
  const cards = pageState.elements.get("update-cards");
  const card = cards.children.find((item) =>
    find(item, (child) => child.tag === "h2")?.textContent === component.toUpperCase());
  assert.ok(card);
  return find(card, (item) => item.tag === "button" && item.textContent.startsWith("Update application")
    || item.tag === "button" && item.textContent === "Update WebUI");
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

  const webui = record("webui");
  webui.installed = { source_sha: webui.candidate.source_sha, digest: "sha256:" + "d".repeat(64) };
  render(view, webui);
  assert.equal(updateButton(view, "webui").disabled, true);
});
