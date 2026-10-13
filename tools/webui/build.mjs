import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { buildSync } from "esbuild";

const root = path.resolve(import.meta.dirname, "../..");
const source = path.join(root, "frontend");
const output = path.join(root, "dist/frontend");
fs.mkdirSync(path.join(output, "administration"), { recursive: true });
const bundle = buildSync({
  absWorkingDir: root,
  metafile: true,
  entryPoints: [path.join(source, "app.js"), path.join(source, "administration.js")],
  outdir: output,
  nodePaths: [path.join(import.meta.dirname, "node_modules")],
  bundle: true,
  minify: true,
  platform: "browser",
  format: "iife",
  target: "es2022",
  define: { "process.env.NODE_ENV": '"production"' },
  legalComments: "inline",
});
// Preserve project and dependency license text inside the existing assets.
function licenseText(directory) {
  const name = fs.readdirSync(directory).find((name) => /^licen[cs]e(?:\.(?:md|txt))?$/i.test(name));
  if (!name) throw new Error("Missing dependency license: " + directory);
  return fs.readFileSync(path.join(directory, name), "utf8").replaceAll("*/", "* /");
}
const projectNotice = "/*! jModGuard\n" + licenseText(root) + "\n*/";
for (const [file, emitted] of Object.entries(bundle.metafile.outputs)) {
  const packages = new Set();
  for (const [name, contribution] of Object.entries(emitted.inputs)) {
    if (!name.includes("node_modules/") || !contribution.bytesInOutput) continue;
    let directory = path.dirname(path.join(root, name));
    while (!fs.existsSync(path.join(directory, "package.json"))) directory = path.dirname(directory);
    packages.add(directory);
  }
  const notices = [...packages].sort().map((directory) =>
    "/*! " + path.relative(root, directory) + "\n" + licenseText(directory) + "\n*/").join("\n");
  fs.appendFileSync(path.join(root, file), "\n" + projectNotice + "\n" + notices + "\n");
}
const daisyui = fileURLToPath(import.meta.resolve("daisyui/daisyui.css"));
const css = buildSync({
  absWorkingDir: root,
  entryPoints: [path.join(import.meta.dirname, "style.css")],
  outfile: path.join(output, "style.css"),
  bundle: true,
  minify: true,
  metafile: true,
  legalComments: "inline",
  banner: { css: projectNotice + "\n/*! daisyUI\n" + licenseText(path.dirname(daisyui)) + "\n*/" },
});
// Build-only audit evidence: never included in the five published assets.
fs.writeFileSync(path.join(root, "dist/webui-metafile.json"),
  JSON.stringify({ js: bundle.metafile, css: css.metafile }, null, 2) + "\n");
for (const name of ["index.html", "administration/index.html"]) {
  fs.copyFileSync(path.join(source, name), path.join(output, name));
}
