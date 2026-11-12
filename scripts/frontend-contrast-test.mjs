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

// sRGB hex <-> OKLab, matching the color-mix(in oklab, ...) daisyUI uses for soft alerts.
const linear = (c) => c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
const gamma = (c) => c <= 0.0031308 ? 12.92 * c : 1.055 * c ** (1 / 2.4) - 0.055;
function oklab(hex) {
  const [r, g, b] = hex.slice(1).match(/../g).map((value) => linear(parseInt(value, 16) / 255));
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  return [0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
    1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
    0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s];
}
function mixOklab(color, base, share) {
  const [L, A, B] = oklab(color).map((value, index) => value * share + oklab(base)[index] * (1 - share));
  const l = (L + 0.3963377774 * A + 0.2158037573 * B) ** 3;
  const m = (L - 0.1055613458 * A - 0.0638541728 * B) ** 3;
  const s = (L - 0.0894841775 * A - 1.291485548 * B) ** 3;
  return `#${[4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s]
    .map((c) => Math.round(Math.min(1, Math.max(0, gamma(c))) * 255).toString(16).padStart(2, "0")).join("")}`;
}

// The daisyUI color variables are declared as var(--token) aliases in the theme blocks.
function statusColor(name) {
  const aliases = [...css.matchAll(new RegExp(`--color-${name}:\\s*var\\((--[\\w-]+)\\)`, "g"))].map((match) => match[1]);
  assert.ok(aliases.length >= 2 && aliases.every((alias) => alias === aliases[0]), `--color-${name} must alias one token in both themes`);
  return aliases[0];
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

for (const [theme, selector] of [["light", ":root"], ["dark", ':root[data-theme="dark"]']]) {
  test(`${theme} soft status alerts keep readable text`, () => {
    const colors = tokens(selector);
    // daisyUI 5 alert-soft: the status color as text on 8% of it mixed into base-100 (--panel).
    for (const name of ["warning", "info", "error", "success"]) {
      const text = colors[statusColor(name)];
      const ratio = contrast(text, mixOklab(text, colors["--panel"], 0.08));
      assert.ok(ratio >= 4.5, `${theme} alert-soft alert-${name}: ${ratio.toFixed(2)}:1`);
    }
  });
}

test("page notices use soft status alerts", () => {
  // A solid alert paints the status color as the background; ours are tuned as text colors.
  for (const page of ["app.js", "administration.js"]) {
    const source = readFileSync(new URL(`../frontend/${page}`, import.meta.url), "utf8");
    const notices = [...source.matchAll(/className:\s*("[^"\n]*"|`[^`]*`)/g)]
      .map(([, literal]) => literal)
      .filter((literal) => /(?:^|[\s"`])alert\s/.test(literal));
    assert.ok(notices.length > 0, `${page} has no alert class lists`);
    for (const className of notices)
      assert.match(className, /\balert-soft\b/, `${page}: ${className}`);
    // A class list kept in a variable or helper is still a literal somewhere.
    const solid = [...source.matchAll(/"[^"\n]*"|'[^'\n]*'|`[^`]*`/g)]
      .map(([literal]) => literal)
      .filter((literal) => /(?:^|[\s"'`])alert\s(?:.*\s)?alert-(?:warning|info|error|success)\b/.test(literal));
    for (const literal of solid)
      assert.match(literal, /\balert-soft\b/, `${page}: ${literal}`);
  }
});

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
