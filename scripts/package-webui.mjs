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
  const frontend = path.join(root, "dist", "frontend");
  const out = path.join(root, "dist", "webui");
  const prefix = `/assets/releases/${sha}`;
  const assets = path.join(out, "assets", "releases", sha);
  const shared = ["style.css", "webui-runtime.js"];
  const pages = [
    // Old updaters require app.js in the archive before they touch Storage.
    // Omitting it prevents them from switching to HTML that needs an uninstalled runtime.
    { html: "index.html", source: "app.js", entry: "workspace.js" },
    { html: "administration/index.html", source: "administration.js", entry: "administration.js" },
  ];
  for (const name of [...shared, ...pages.flatMap(page => [page.html, page.source])]) {
    if (!fs.lstatSync(path.join(frontend, name)).isFile()) {
      throw new Error(`WebUI source is not a regular file: ${name}`);
    }
  }
  const htmlFiles = pages.map(page => {
    let html = fs.readFileSync(path.join(frontend, page.html), "utf8");
    for (const [from, to] of [
      [`src="/${page.source}"`, `src="${prefix}/${page.entry}"`],
      ...shared.map(name => [`href="/${name}"`, `href="${prefix}/${name}"`]),
    ]) {
      if (!html.includes(from)) throw new Error(`${page.html} lacks asset reference ${from}`);
      html = html.replace(from, to);
    }
    return html;
  });
  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(path.join(out, "administration"), { recursive: true });
  fs.mkdirSync(assets, { recursive: true });
  for (const name of shared) {
    fs.copyFileSync(path.join(frontend, name), path.join(assets, name));
  }
  for (const [index, page] of pages.entries()) {
    fs.writeFileSync(path.join(out, page.html), htmlFiles[index]);
    const js = fs.readFileSync(path.join(frontend, page.source), "utf8");
    fs.writeFileSync(path.join(assets, page.entry), `window.JMODGUARD_WEBUI_SHA = "${sha}";\n` + js);
  }
  return out;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.stdout.write(`${packageWebui(...process.argv.slice(2))}\n`);
}
