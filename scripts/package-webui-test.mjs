#!/usr/bin/env node
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { packageWebui } from "./package-webui.mjs";

const repo = path.resolve(import.meta.dirname, "..");
const sha = "a".repeat(40);

test("WebUI package has immutable references and both entry pages", () => {
  const scratch = fs.mkdtempSync(path.join(process.env.TMPDIR || os.tmpdir(), "jmodguard-webui-package-"));
  try {
    fs.mkdirSync(path.join(scratch, "frontend", "administration"), { recursive: true });
    for (const name of ["index.html", "app.js", "style.css", "administration/index.html", "administration.js"]) {
      fs.copyFileSync(path.join(repo, "frontend", name), path.join(scratch, "frontend", name));
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
    assert.throws(() => packageWebui(scratch, "bad"));
  } finally {
    fs.rmSync(scratch, { recursive: true, force: true });
  }
});
