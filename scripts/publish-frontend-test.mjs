#!/usr/bin/env node
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

const mockCurl = `#!/usr/bin/env node
const fs = require("fs");
const path = require("path");
const store = process.env.MOCK_STORE;
const log = process.env.MOCK_LOG;
const rules = (process.env.MOCK_FAIL_PUT || "").split(",").filter(Boolean).map((rule) => {
  const [file, skip, times] = rule.split(":");
  return { file, skip: Number(skip || 0), times: Number(times || 0) };
});
const args = process.argv.slice(2);
let method = "GET";
let outfile = "";
let writeFmt = "";
let upload = "";
let url = "";
for (let i = 0; i < args.length; i++) {
  const a = args[i];
  if (a === "-X") method = args[++i];
  else if (a === "-o") outfile = args[++i];
  else if (a === "-w") writeFmt = args[++i];
  else if (a === "--upload-file") upload = args[++i];
  else if (a === "-H" || a === "-d") i++;
  else if (a.startsWith("https://")) url = a;
}
const file = decodeURIComponent(new URL(url).pathname.split("/").filter(Boolean).pop() || "");
fs.appendFileSync(log, method + " " + file + "\\n");
if (url.includes("/purgeCache")) process.exit(0);
if (method === "GET") {
  const p = path.join(store, file);
  if (fs.existsSync(p)) {
    if (outfile) fs.copyFileSync(p, outfile);
    if (writeFmt.includes("http_code")) process.stdout.write("200");
    process.exit(0);
  }
  if (process.env.MOCK_GET_CODE) {
    if (writeFmt.includes("http_code")) process.stdout.write(process.env.MOCK_GET_CODE);
    process.exit(0);
  }
  if (writeFmt.includes("http_code")) process.stdout.write("404");
  process.exit(0);
}
if (method === "PUT") {
  const rule = rules.find((item) => item.file === file);
  if (rule) {
    const countPath = path.join(store, ".failcount-" + file);
    const n = (fs.existsSync(countPath) ? Number(fs.readFileSync(countPath, "utf8")) : 0) + 1;
    fs.writeFileSync(countPath, String(n));
    if (n > rule.skip && n <= rule.skip + rule.times) process.exit(22);
  }
  fs.copyFileSync(upload, path.join(store, file));
  process.exit(0);
}
if (method === "DELETE") {
  try { fs.unlinkSync(path.join(store, file)); } catch (_) {}
  process.exit(0);
}
process.exit(1);
`;

function setup() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "jmodguard-publish-"));
  const bin = path.join(dir, "bin");
  fs.mkdirSync(bin);
  fs.writeFileSync(path.join(bin, "curl"), mockCurl, { mode: 0o755 });
  const frontend = path.join(dir, "frontend");
  fs.mkdirSync(frontend);
  fs.writeFileSync(path.join(frontend, "index.html"), "NEW-HTML");
  fs.writeFileSync(path.join(frontend, "app.js"), "NEW-JS");
  fs.writeFileSync(path.join(frontend, "style.css"), "NEW-CSS");
  const store = path.join(dir, "store");
  fs.mkdirSync(store);
  return { dir, frontend, store, log: path.join(dir, "log") };
}

function publish(ctx, extra = {}) {
  return spawnSync("bash", [path.join(repo, "scripts/publish-frontend-bunny.sh")], {
    encoding: "utf8",
    env: {
      ...process.env,
      PATH: `${path.join(ctx.dir, "bin")}:${process.env.PATH}`,
      BUNNY_STORAGE_ZONE: "zone",
      BUNNY_STORAGE_PASSWORD: "pw",
      BUNNY_STORAGE_ENDPOINT: "storage.test",
      JMODGUARD_FRONTEND_ROOT: ctx.frontend,
      JMODGUARD_PUBLISH_RETRY_SLEEP: "0",
      MOCK_STORE: ctx.store,
      MOCK_LOG: ctx.log,
      ...extra,
    },
  });
}

test("refuses to publish when backup GET is not 200 or 404", () => {
  const ctx = setup();
  const result = publish(ctx, { MOCK_GET_CODE: "500" });
  assert.notEqual(result.status, 0);
  assert.match(result.stderr, /cannot determine existing/);
  assert.equal(fs.existsSync(path.join(ctx.store, "index.html")), false);
});

test("failed later upload restores backed-up objects", () => {
  const ctx = setup();
  fs.writeFileSync(path.join(ctx.store, "app.js"), "OLD-JS");
  fs.writeFileSync(path.join(ctx.store, "style.css"), "OLD-CSS");
  fs.writeFileSync(path.join(ctx.store, "index.html"), "OLD-HTML");
  const result = publish(ctx, { MOCK_FAIL_PUT: "style.css:0:20" });
  assert.notEqual(result.status, 0);
  assert.equal(fs.readFileSync(path.join(ctx.store, "app.js"), "utf8"), "OLD-JS");
  assert.equal(fs.readFileSync(path.join(ctx.store, "style.css"), "utf8"), "OLD-CSS");
  assert.equal(fs.readFileSync(path.join(ctx.store, "index.html"), "utf8"), "OLD-HTML");
});

test("failed later upload deletes objects confirmed missing", () => {
  const ctx = setup();
  const result = publish(ctx, { MOCK_FAIL_PUT: "style.css:0:20" });
  assert.notEqual(result.status, 0);
  assert.equal(fs.existsSync(path.join(ctx.store, "app.js")), false);
  assert.equal(fs.existsSync(path.join(ctx.store, "style.css")), false);
});

test("rollback retries until restore succeeds", () => {
  const ctx = setup();
  fs.writeFileSync(path.join(ctx.store, "app.js"), "OLD-JS");
  fs.writeFileSync(path.join(ctx.store, "style.css"), "OLD-CSS");
  fs.writeFileSync(path.join(ctx.store, "index.html"), "OLD-HTML");
  const result = publish(ctx, { MOCK_FAIL_PUT: "style.css:0:20,app.js:1:3" });
  assert.notEqual(result.status, 0);
  assert.equal(fs.readFileSync(path.join(ctx.store, "app.js"), "utf8"), "OLD-JS");
  assert.equal(fs.readFileSync(path.join(ctx.store, "style.css"), "utf8"), "OLD-CSS");
});

test("successful publish writes all three files", () => {
  const ctx = setup();
  const result = publish(ctx);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(fs.readFileSync(path.join(ctx.store, "app.js"), "utf8"), "NEW-JS");
  assert.equal(fs.readFileSync(path.join(ctx.store, "style.css"), "utf8"), "NEW-CSS");
  assert.equal(fs.readFileSync(path.join(ctx.store, "index.html"), "utf8"), "NEW-HTML");
});
