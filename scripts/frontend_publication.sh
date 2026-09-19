#!/usr/bin/env bash
# Bunny publication receipt: asset hashes, never a source-commit checkpoint.
set -euo pipefail

exec node --input-type=module - "$@" <<'JS'
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";

const FILES = ["index.html", "app.js", "style.css"];

function die(message) {
  process.stderr.write(`frontend_publication: ${message}\n`);
  process.exit(1);
}

function assetHashes(root) {
  const result = {};
  for (const name of FILES) {
    const file = path.join(root, name);
    let st;
    try {
      st = fs.lstatSync(file);
    } catch {
      die(`${name} must be a regular asset`);
    }
    if (st.isSymbolicLink() || !st.isFile()) {
      die(`${name} must be a regular asset`);
    }
    result[name] = crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
  }
  return result;
}

function validZone(zone) {
  return typeof zone === "string" && /^[1-9][0-9]*$/.test(zone);
}

function configuredZone() {
  const zone = process.env.BUNNY_PULL_ZONE_ID || "";
  if (!validZone(zone)) {
    die("BUNNY_PULL_ZONE_ID must be a positive numeric Pull Zone ID");
  }
  return zone;
}

function parse(raw, zone) {
  try {
    const data = JSON.parse(raw);
    if (
      !validZone(zone) ||
      typeof data !== "object" ||
      data === null ||
      Array.isArray(data) ||
      data.version !== 1 ||
      typeof data.complete !== "boolean" ||
      data.pull_zone_id !== zone
    ) {
      return null;
    }
    const files = data.files;
    if (typeof files !== "object" || files === null || Array.isArray(files)) {
      return null;
    }
    let requiresApi = data.requires_api;
    if (requiresApi === undefined) {
      requiresApi = !data.complete;
    }
    if (typeof requiresApi !== "boolean") {
      return null;
    }
    data.requires_api = requiresApi;
    if (Object.keys(files).length === 0 && !data.complete) {
      return data;
    }
    if (
      FILES.some((name) => !(name in files)) ||
      Object.keys(files).length !== FILES.length ||
      !Object.values(files).every((value) => typeof value === "string" && /^[0-9a-f]{64}$/.test(value))
    ) {
      return null;
    }
    return data;
  } catch {
    return null;
  }
}

function makeReceipt(mode, root, previous, zone) {
  let old = null;
  if (fs.existsSync(previous)) {
    old = parse(fs.readFileSync(previous), zone);
  }
  const desired = assetHashes(root);
  const complete = mode === "complete";
  let requiresApi = false;
  if (!complete) {
    requiresApi = old === null || (!old.complete && old.requires_api);
    for (const name of ["index.html", "app.js"]) {
      const backup = path.join(path.dirname(previous), name);
      requiresApi =
        requiresApi ||
        !old ||
        old.files[name] !== desired[name] ||
        !fs.existsSync(backup) ||
        !fs.statSync(backup).isFile() ||
        crypto.createHash("sha256").update(fs.readFileSync(backup)).digest("hex") !== desired[name];
    }
  }
  const files = complete ? desired : old ? old.files : {};
  return {
    complete,
    files,
    pull_zone_id: zone,
    requires_api: Boolean(requiresApi),
    version: 1,
  };
}

const args = process.argv.slice(2);
const zone = configuredZone();
if (args.length === 1 && args[0] === "zone") {
  process.stdout.write(`${zone}\n`);
  process.exit(0);
}
if (args.length !== 3) {
  die("expected zone, or pending|complete ROOT PREVIOUS");
}
const [mode, root, previous] = args;
if (mode !== "pending" && mode !== "complete") {
  die("expected pending or complete");
}
process.stdout.write(`${JSON.stringify(makeReceipt(mode, root, previous, zone))}\n`);
JS
