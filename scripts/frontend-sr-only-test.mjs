import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

// Visually hidden control labels must stay readable by screen readers (#600).
// Check the built stylesheet the pages actually load (daisyUI + frontend/style.css);
// ci-preflight builds it with `npm run build --prefix tools/webui` before this test.
const css = readFileSync(new URL("../dist/frontend/style.css", import.meta.url), "utf8");
const app = readFileSync(new URL("../frontend/app.js", import.meta.url), "utf8");

function declarations(selector) {
  const found = {};
  for (const [, selectors, body] of css.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    if (!selectors.split(",").some((part) => part.trim() === selector)) continue;
    for (const declaration of body.split(";")) {
      const colon = declaration.indexOf(":");
      if (colon < 0) continue;
      found[declaration.slice(0, colon).trim()] = declaration.slice(colon + 1).replace(/\s+/g, "").replace(/!important$/, "");
    }
  }
  return found;
}

test("the hidden Ban behavior label resolves to a visually-hidden rule in the shipped CSS", () => {
  assert.match(app, /h\(Label, \{ className: "sr-only" \}, "Ban behavior"\)/);
  const rule = declarations(".sr-only");
  assert.ok(Object.keys(rule).length, "shipped CSS has no .sr-only rule");
  assert.deepEqual(
    {
      position: rule.position,
      width: rule.width,
      height: rule.height,
      overflow: rule.overflow,
      clip: rule.clip,
      "white-space": rule["white-space"],
    },
    { position: "absolute", width: "1px", height: "1px", overflow: "hidden", clip: "rect(0,0,0,0)", "white-space": "nowrap" },
  );
  // display:none and visibility:hidden would also hide the label from screen readers.
  assert.notEqual(rule.display, "none");
  assert.notEqual(rule.visibility, "hidden");
});
