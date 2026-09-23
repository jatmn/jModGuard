import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const css = readFileSync(new URL("../frontend/style.css", import.meta.url), "utf8");

function tokens(selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const block = css.match(new RegExp(`${escaped}\\s*\\{([^}]+)\\}`));
  assert.ok(block, `missing ${selector} theme tokens`);
  return Object.fromEntries([...block[1].matchAll(/(--[\w-]+):\s*(#[\da-fA-F]{6})/g)]
    .map(([, name, color]) => [name, color]));
}

function luminance(hex) {
  const channels = hex.slice(1).match(/../g).map((value) => parseInt(value, 16) / 255);
  return channels.map((channel) => channel <= 0.04045
    ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4)
    .reduce((sum, channel, index) => sum + channel * [0.2126, 0.7152, 0.0722][index], 0);
}

function contrast(first, second) {
  const [low, high] = [luminance(first), luminance(second)].sort((a, b) => a - b);
  return (high + 0.05) / (low + 0.05);
}

for (const [theme, selector] of [["light", ":root"], ["dark", ':root[data-theme="dark"]']]) {
  test(`${theme} controls have a visible boundary on their surfaces`, () => {
    const colors = tokens(selector);
    for (const surface of ["--input", "--panel", "--panel2", "--side-panel"]) {
      const ratio = contrast(colors["--input-border"], colors[surface]);
      assert.ok(ratio >= 3, `${theme} control border on ${surface}: ${ratio.toFixed(2)}:1`);
    }
    assert.ok(contrast(colors["--accent"], colors["--input"]) >= 3,
      `${theme} checked checkbox must contrast with its field background`);
    assert.ok(contrast(colors["--accent-text"], colors["--accent"]) >= 3,
      `${theme} check mark must contrast with its checked background`);
  });
}

test("component borders use the contrast-tested theme colors", () => {
  // daisyUI 5 uses --border as a width in its border shorthand. Defining it as
  // a theme color invalidates that shorthand and makes unchecked boxes invisible.
  assert.doesNotMatch(css, /(?:^|[;{\s])--border\s*:/m);
  assert.match(css, /--shell-border:\s*#[\da-fA-F]{6}/);
  assert.match(css, /border:\s*1px solid var\(--shell-border\)/);
  assert.match(css, /\.input, \.select, \.textarea\s*\{\s*--input-color:\s*var\(--input-border\)/);
  const checkbox = css.match(/\.checkbox\s*\{([^}]*)\}/)?.[1];
  assert.ok(checkbox, "missing checkbox theme rule");
  assert.match(checkbox, /border-width:\s*2px/);
  assert.match(checkbox, /border-color:\s*var\(--input-border\)/);
  // daisyUI owns the checked fill inside a CSS layer; an unlayered fill here hides its check mark.
  assert.doesNotMatch(checkbox, /background-color\s*:/);
  assert.match(css, /\.btn:not\([^}]*\)\s*\{\s*border-color:\s*var\(--input-border\)/);
});
