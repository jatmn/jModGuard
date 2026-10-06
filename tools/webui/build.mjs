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
  // The package barrel retains unrelated component initializers. Resolve the
  // pinned package's Combobox entry; a layout change must fail the build.
  alias: {
    "@headlessui/react": fileURLToPath(new URL("./components/combobox/combobox.js", import.meta.resolve("@headlessui/react"))),
  },
  entryPoints: [path.join(source, "app.js")],
  outfile: path.join(output, "app.js"),
  bundle: true,
  minify: true,
  platform: "browser",
  format: "iife",
  target: "es2022",
  define: { "process.env.NODE_ENV": '"production"' },
  legalComments: "inline",
});
// Preserve dependency license text inside the existing assets, not new files.
function licenseText(directory) {
  const name = fs.readdirSync(directory).find((name) => /^licen[cs]e(?:\.(?:md|txt))?$/i.test(name));
  if (!name) throw new Error("Missing dependency license: " + directory);
  return fs.readFileSync(path.join(directory, name), "utf8").replaceAll("*/", "* /");
}
const packages = new Set();
for (const name of Object.keys(bundle.metafile.inputs)) {
  if (!name.includes("node_modules/")) continue;
  let directory = path.dirname(path.join(root, name));
  while (!fs.existsSync(path.join(directory, "package.json"))) directory = path.dirname(directory);
  packages.add(directory);
}
const notices = [...packages].sort().map((directory) =>
  "/*! " + path.relative(root, directory) + "\n" + licenseText(directory) + "\n*/").join("\n");
fs.appendFileSync(path.join(output, "app.js"), "\n" + notices + "\n");
const daisyui = fileURLToPath(import.meta.resolve("daisyui/daisyui.css"));
const css = buildSync({
  absWorkingDir: root,
  entryPoints: [path.join(import.meta.dirname, "style.css")],
  outfile: path.join(output, "style.css"),
  bundle: true,
  minify: true,
  metafile: true,
  legalComments: "inline",
  banner: { css: "/*! daisyUI\n" + licenseText(path.dirname(daisyui)) + "\n*/" },
});
// Build-only audit evidence: never included in the five published assets.
fs.writeFileSync(path.join(root, "dist/webui-metafile.json"),
  JSON.stringify({ js: bundle.metafile, css: css.metafile }, null, 2) + "\n");
for (const name of ["index.html", "administration/index.html", "administration.js"]) {
  fs.copyFileSync(path.join(source, name), path.join(output, name));
}
