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
    fs.cpSync(path.join(repo, "dist", "frontend"), path.join(scratch, "dist", "frontend"), { recursive: true });
    packageWebui(scratch, sha);
    const out = path.join(scratch, "dist", "webui");
    const assets = path.join(out, "assets", "releases", sha);
    const root = fs.readFileSync(path.join(out, "index.html"), "utf8");
    const admin = fs.readFileSync(path.join(out, "administration/index.html"), "utf8");
    const prefix = `/assets/releases/${sha}`;
    assert.deepEqual(fs.readdirSync(assets).sort(), ["administration.js", "style.css", "webui-runtime.js", "workspace.js"]);
    for (const [html, entry] of [[root, "workspace.js"], [admin, "administration.js"]]) {
      assert.doesNotMatch(html, /https?:\/\/|type=["']importmap["']|\bimport\s*\(/);
      assert.ok(html.includes(`<script type="module" src="${prefix}/${entry}">`));
      assert.ok(html.includes(`<link rel="stylesheet" href="${prefix}/style.css">`));
      assert.ok(html.split("</head>")[0].includes(`<link rel="modulepreload" href="${prefix}/webui-runtime.js">`));
      for (const match of html.matchAll(/(?:src|href)=["']([^"']+)["']/g)) {
        const url = match[1];
        if (!/\.(?:js|css)$/.test(url)) continue;
        assert.ok(url.startsWith(prefix + "/"), url);
        assert.ok(fs.statSync(path.join(out, url)).isFile(), url);
      }
    }
    for (const name of ["workspace.js", "administration.js", "webui-runtime.js"]) {
      const js = fs.readFileSync(path.join(assets, name), "utf8");
      const dependencies = parse(js)[0];
      if (name === "webui-runtime.js") assert.deepEqual(dependencies, []);
      else {
        assert.match(js, new RegExp(sha));
        assert.deepEqual(dependencies.map(({ type, specifier }) => [type, specifier]), [["static", "./webui-runtime.js"]]);
      }
      assert.doesNotMatch(js, /https?:\/\/(?:esm\.sh|cdn\.jsdelivr\.net)/);
    }
    const css = fs.readFileSync(path.join(assets, "style.css"), "utf8");
    assert.doesNotMatch(css, /@import\b/);
    assert.match(css, /daisyUI/);
    const projectLicense = fs.readFileSync(path.join(repo, "LICENSE.md"), "utf8");
    for (const name of ["workspace.js", "administration.js", "webui-runtime.js", "style.css"]) {
      const asset = fs.readFileSync(path.join(assets, name), "utf8");
      assert.ok(asset.includes(projectLicense), name + " must include the complete project license");
    }
    assert.throws(() => packageWebui(scratch, "bad"));
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
});

test("both entries share React DOM without pulling workspace features into Administration", () => {
  const { js } = JSON.parse(fs.readFileSync(path.join(repo, "dist/webui-metafile.json"), "utf8"));
  const runtime = js.outputs["dist/frontend/webui-runtime.js"];
  const contributions = output => Object.keys(output.inputs).filter(input => output.inputs[input].bytesInOutput > 0);
  assert.ok(contributions(runtime).some(input => /react-dom\/cjs\/react-dom-client\.production\.js$/.test(input)));
  const runtimeJs = fs.readFileSync(path.join(repo, "dist/frontend/webui-runtime.js"), "utf8");
  const reactDomLicense = fs.readFileSync(path.join(repo, "tools/webui/node_modules/react-dom/LICENSE"), "utf8");
  assert.ok(runtimeJs.includes(reactDomLicense));
  for (const name of ["app.js", "administration.js"]) {
    const output = js.outputs[`dist/frontend/${name}`];
    assert.deepEqual(output.imports.map(({ path }) => path), ["dist/frontend/webui-runtime.js"]);
    assert.ok(contributions(output).every(input => !input.includes("/react-dom/")));
  }
  const workspaceOnly = contributions(js.outputs["dist/frontend/app.js"]);
  const directAdmin = [...contributions(runtime), ...contributions(js.outputs["dist/frontend/administration.js"])];
  for (const input of ["frontend/app.js", "frontend/charts.js"]) {
    assert.ok(workspaceOnly.includes(input), input);
    assert.ok(!directAdmin.includes(input), input);
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
  for (const name of ["alert", "badge", "btn", "card", "checkbox", "hero", "input", "menu", "modal", "navbar", "select", "stat", "table", "textarea", "toggle"]) {
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
