#!/usr/bin/env node
// Produce an immutable WebUI OCI image payload without building Rust or
// uploading to Bunny. Both HTML entry files point to the same release prefix.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export function packageWebui(root, sha) {
if (!root || !/^[0-9a-f]{40}$/.test(sha)) {
  throw new Error("usage: node scripts/package-webui.mjs SOURCE_ROOT FULL_SOURCE_SHA");
}
const frontend = path.join(root, "frontend");
const out = path.join(root, "dist", "webui");
const prefix = `/assets/releases/${sha}`;
const required = ["index.html", "app.js", "style.css", "administration/index.html", "administration.js"];
for (const name of required) {
  const file = path.join(frontend, name);
  const stat = fs.lstatSync(file);
  if (!stat.isFile() || stat.isSymbolicLink()) throw new Error(`WebUI source is not a regular file: ${name}`);
}
fs.rmSync(out, { recursive: true, force: true });
fs.mkdirSync(path.join(out, "administration"), { recursive: true });
fs.mkdirSync(path.join(out, "assets", "releases", sha), { recursive: true });
const rootHtml = fs.readFileSync(path.join(frontend, "index.html"), "utf8")
  .replace('href="/style.css"', `href="${prefix}/style.css"`)
  .replace('src="/app.js"', `src="${prefix}/app.js"`);
if (!rootHtml.includes(`${prefix}/app.js`) || !rootHtml.includes(`${prefix}/style.css`)) {
  throw new Error("root HTML asset references did not rewrite");
}
const adminHtml = fs.readFileSync(path.join(frontend, "administration/index.html"), "utf8")
  .replace('href="/style.css"', `href="${prefix}/style.css"`)
  .replace('src="/administration.js"', `src="${prefix}/administration.js"`);
if (!adminHtml.includes(`${prefix}/administration.js`) || !adminHtml.includes(`${prefix}/style.css`)) {
  throw new Error("Administration HTML asset references did not rewrite");
}
fs.writeFileSync(path.join(out, "index.html"), rootHtml);
fs.writeFileSync(path.join(out, "administration/index.html"), adminHtml);
const assets = path.join(out, "assets", "releases", sha);
fs.writeFileSync(path.join(assets, "app.js"), `window.JMODGUARD_WEBUI_SHA = "${sha}";\n` + fs.readFileSync(path.join(frontend, "app.js"), "utf8"));
fs.copyFileSync(path.join(frontend, "style.css"), path.join(assets, "style.css"));
fs.writeFileSync(path.join(assets, "administration.js"), `window.JMODGUARD_WEBUI_SHA = "${sha}";\n` + fs.readFileSync(path.join(frontend, "administration.js"), "utf8"));
return out;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.stdout.write(`${packageWebui(...process.argv.slice(2))}\n`);
}
