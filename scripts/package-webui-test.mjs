#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { init, parse } from "../tools/webui/node_modules/es-module-lexer/dist/lexer.js";
import { packageWebui } from "./package-webui.mjs";

await init;

const repo = path.resolve(import.meta.dirname, "..");
const sha = "a".repeat(40);

test("WebUI package has immutable references and both entry pages", () => {
  const scratch = fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), "jmodguard-webui-package-"));
  try {
    fs.mkdirSync(path.join(scratch, "dist", "frontend", "administration"), { recursive: true });
    for (const name of ["index.html", "app.js", "style.css", "administration/index.html", "administration.js"]) {
      fs.copyFileSync(path.join(repo, "dist", "frontend", name), path.join(scratch, "dist", "frontend", name));
    }
    packageWebui(scratch, sha);
    const out = path.join(scratch, "dist", "webui");
    const root = fs.readFileSync(path.join(out, "index.html"), "utf8");
    const admin = fs.readFileSync(path.join(out, "administration/index.html"), "utf8");
    const prefix = `/assets/releases/${sha}`;
    assert.match(root, new RegExp(`${prefix}/app.js`));
    assert.match(root, new RegExp(`${prefix}/style.css`));
    assert.match(admin, new RegExp(`${prefix}/administration.js`));
    assert.match(admin, new RegExp(`${prefix}/style.css`));
    assert.match(fs.readFileSync(path.join(out, "assets", "releases", sha, "app.js"), "utf8"), new RegExp(sha));
    assert.ok(fs.statSync(path.join(out, "assets", "releases", sha, "administration.js")).isFile());
    // Check the real packaged pages and every referenced asset, not a mocked bundle.
    for (const html of [root, admin]) {
      assert.doesNotMatch(html, /https?:\/\/|type=["']importmap["']|\bimport\s*\(/);
      for (const match of html.matchAll(/(?:src|href)=["']([^"']+)["']/g)) {
        const url = match[1];
        if (!/\.(?:js|css)$/.test(url)) continue;
        assert.ok(url.startsWith(prefix + "/"), url);
        assert.ok(fs.statSync(path.join(out, url)).isFile(), url);
      }
    }
    for (const name of ["app.js", "administration.js"]) {
      const js = fs.readFileSync(path.join(out, "assets", "releases", sha, name), "utf8");
      // An IIFE bundle has no remaining module imports, including computed URLs.
      assert.deepEqual(parse(js)[0], [], name + " must not import external or additional modules");
      assert.doesNotMatch(js, /https?:\/\/(?:esm\.sh|cdn\.jsdelivr\.net)/);
    }
    const css = fs.readFileSync(path.join(out, "assets", "releases", sha, "style.css"), "utf8");
    assert.doesNotMatch(css, /@import\b/);
    assert.match(css, /daisyUI/);
    assert.throws(() => packageWebui(scratch, "bad"));
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
});

// Inspect emitted contributions, not just named imports in the source adapter.
test("WebUI excludes unused Headless components and popup dependency families", () => {
  const { js } = JSON.parse(fs.readFileSync(path.join(repo, "dist/webui-metafile.json"), "utf8"));
  const components = new Set();
  for (const output of Object.values(js.outputs)) {
    for (const [input, { bytesInOutput }] of Object.entries(output.inputs)) {
      if (bytesInOutput > 0) assert.doesNotMatch(input, /node_modules\/(?:@tanstack|@floating-ui)\//);
      const match = input.match(/@headlessui\/react\/dist\/components\/([^/]+)\//);
      if (match && bytesInOutput > 0) components.add(match[1]);
    }
  }
  // These are the controls used by the React UI plus their accessibility/lifecycle helpers.
  assert.deepEqual([...components].sort(), ["description", "dialog", "field", "focus-trap", "input", "label", "portal", "select", "textarea", "transition"]);
});

test("WebUI CSS retains used widgets and excludes unused component families", () => {
  const css = fs.readFileSync(path.join(repo, "dist/frontend/style.css"), "utf8");
  for (const name of ["alert", "badge", "btn", "card", "checkbox", "hero", "input", "menu", "modal", "navbar", "select", "stat", "table", "textarea"]) {
    assert.ok(new RegExp("\\." + name + "(?=[^a-zA-Z0-9_-])").test(css), "missing " + name);
  }
  for (const name of ["carousel", "chat", "countdown", "dock", "drawer", "rating", "timeline"]) {
    assert.ok(!new RegExp("\\." + name + "(?=[^a-zA-Z0-9_-])").test(css), "unused " + name);
  }
  for (const name of ["bg-base-100", "bg-base-200", "text-warning", "rounded-box"]) {
    assert.ok(css.includes("." + name + "{"), "missing theme utility " + name);
  }
  // Optional utility sheets contain hundreds of unused colors/opacity/radius variants.
  const { css: metadata } = JSON.parse(fs.readFileSync(path.join(repo, "dist/webui-metafile.json"), "utf8"));
  assert.ok(!Object.keys(metadata.inputs).some((name) => /daisyui\/(colors\/|utilities\/radius\.css)/.test(name)));
  for (const name of ["bg-base-300", "text-secondary", "rounded-t-box", "rounded-selector"]) {
    assert.ok(!css.includes("." + name), "unused theme utility " + name);
  }
});
