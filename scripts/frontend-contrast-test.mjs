import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

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

// #590 desktop density: the theme alias blocks carry the shared scale tokens.
const THEME_BLOCKS = [
  ["light", ':root, :root[data-theme="light"], [data-theme="light"]'],
  ["dark", ':root[data-theme="dark"], [data-theme="dark"]'],
];
function rule(selector, source = css) {
  const at = source.split("\n").findIndex((line) => line.startsWith(`${selector} {`));
  assert.ok(at >= 0, `missing rule ${selector}`);
  const rest = source.split("\n").slice(at).join("\n");
  return rest.slice(rest.indexOf("{") + 1, rest.indexOf("}"));
}
const escapeRegExp = (text) => text.replace(/[.*+?^${}()|[\]\\-]/g, "\\$&");
function declaration(body, name) {
  const match = body.match(new RegExp(`(?:^|[;{\\s])${escapeRegExp(name)}:\\s*([^;]+);`));
  return match?.[1].trim();
}
test("declaration() matches property names literally", () => {
  assert.equal(declaration("--a.b: 1px;", "--a.b"), "1px");
  assert.equal(declaration("--aXb: 1px;", "--a.b"), undefined);
  assert.equal(declaration("--a\\b: 2px;", "--a\\b"), "2px");
});
function px(value) {
  const match = String(value).match(/^([\d.]+)(rem|px)$/);
  assert.ok(match, `expected a rem/px length, got ${value}`);
  return Number(match[1]) * (match[2] === "rem" ? 16 : 1);
}
function mediaBlock(query) {
  const start = css.indexOf(`@media ${query} {`);
  assert.ok(start >= 0, `missing @media ${query}`);
  let depth = 0;
  for (let index = css.indexOf("{", start); index < css.length; index++) {
    if (css[index] === "{") depth++;
    if (css[index] === "}" && --depth === 0) return css.slice(css.indexOf("{", start) + 1, index);
  }
  throw new Error(`unterminated @media ${query}`);
}
// daisyUI 5.7: btn/input/select height = --size-field x10 (sm x8, xs x6); checkbox/badge = --size-selector x6.
const FIELD = { md: 10, sm: 8, xs: 6 };

for (const [theme, selector] of THEME_BLOCKS) {
  test(`${theme} theme tokens set the desktop admin-console scale`, () => {
    const body = rule(selector);
    const value = (name) => px(declaration(body, name));
    const font = value("--ui-font-size");
    assert.ok(font >= 13 && font <= 14, `${theme} body text ${font}px`);
    assert.ok(value("--ui-font-size-sm") >= 12, `${theme} secondary text must stay readable`);
    const field = value("--size-field");
    assert.ok(field * FIELD.md >= 28 && field * FIELD.md <= 32, `${theme} control height ${field * FIELD.md}px`);
    assert.ok(field * FIELD.sm >= 24, `${theme} small control ${field * FIELD.sm}px`);
    assert.ok(value("--size-selector") * 6 >= 24, `${theme} checkbox ${value("--size-selector") * 6}px`);
    assert.equal(value("--target-min"), 24, `${theme} target floor`);
    assert.ok(value("--card-p") <= 16, `${theme} card padding`);
    assert.ok(value("--ui-gap") <= 12, `${theme} section gap`);
    const stat = value("--stat-value-size");
    assert.ok(stat >= 16 && stat <= 22, `${theme} stat numeral ${stat}px`);
  });
}

test("the page uses the scale tokens instead of the old touch-sized values", () => {
  assert.match(rule("body"), /font-size:\s*var\(--ui-font-size\)/);
  assert.match(rule(".stat-value"), /font-size:\s*var\(--stat-value-size\)/);
  assert.match(css, /#content\s*\{[^}]*gap:\s*var\(--ui-gap\)/);
  const heading = declaration(rule("#content h2"), "font-size");
  assert.ok(px(heading) <= 20, `page heading ${heading}`);
  // Old scale: 2rem page headings, 36px/40px content padding, 1.875rem stat numerals.
  assert.doesNotMatch(css, /clamp\(1\.5rem, 2\.2vw, 2rem\)|padding: 36px 40px|clamp\(1\.25rem, 2vw, 1\.875rem\)/);
});

test("compact controls keep a visible focus ring with 3:1 contrast on every surface", () => {
  const focus = css.match(/^:is\(([^)]*)\):focus-visible\s*\{([^}]*)\}/m);
  assert.ok(focus, "missing the shared :focus-visible rule");
  for (const control of [".btn", ".input", ".select", ".textarea", ".checkbox", ".menu button", ".info-tip-trigger", "a", "summary"])
    assert.ok(focus[1].split(/,\s*/).includes(control), `focus ring must cover ${control}`);
  assert.match(focus[2], /outline:\s*2px solid var\(--accent\)/);
  assert.match(focus[2], /outline-offset:\s*2px/);
  // No first-party rule removes the outline from a focused control.
  for (const [, selectors, body] of css.matchAll(/([^{}]*:focus(?:-visible|-within)?[^{}]*)\{([^}]*)\}/g))
    assert.doesNotMatch(body, /outline(?:-style)?:\s*(?:none|0)\b/, selectors.trim());
  for (const [theme, selector] of [["light", ":root"], ["dark", ':root[data-theme="dark"]']]) {
    const colors = tokens(selector);
    for (const surface of ["--bg", "--panel", "--panel2", "--input", "--hero", "--sidebar", "--side-panel", "--side-hover", "--selected"]) {
      const ratio = contrast(colors["--accent"], colors[surface]);
      assert.ok(ratio >= 3, `${theme} focus ring on ${surface}: ${ratio.toFixed(2)}:1`);
    }
  }
});

test("the smallest interactive controls keep 24px targets", () => {
  // daisyUI btn-xs/btn-sm/btn-square and input/select heights come from --size-field; the
  // floor covers any variant smaller than the target minimum.
  assert.match(rule(".btn, .input, .select, .info-tip-trigger"), /min-height:\s*var\(--target-min\)/);
  assert.match(rule(".btn-square, .btn-circle, .info-tip-trigger"), /min-width:\s*var\(--target-min\)/);
  assert.match(rule("#tabs button"), /min-height:\s*var\(--target-min\)/);
  assert.match(rule(".administration-badge"), /min-height:\s*var\(--target-min\)/);
  // No first-party rule pins a control below 24px.
  for (const [, selectors, body] of css.matchAll(/([^{}]+)\{([^{}]*)\}/g)) {
    // Only rules whose subject (the last compound selector) is the control itself.
    const subjects = selectors.split(",").map((part) => part.trim().split(/\s+|>/).filter(Boolean).at(-1) ?? "");
    if (!subjects.some((subject) => /^(?:\.(?:btn|input|select|checkbox|info-tip-trigger)\b|button$)/.test(subject.replace(/^[^.]*(?=\.)/, "")))) continue;
    for (const [, property, value] of body.matchAll(/(?:^|;)\s*((?:min-)?(?:height|width)):\s*([\d.]+(?:px|rem))/g))
      assert.ok(px(value) >= 24, `${selectors.trim()} ${property}: ${value}`);
  }
  // Interactive badges, checkbox and size variants smaller than the floored ones stay unused.
  for (const page of ["app.js", "administration.js", "charts.js", "appearance.js"]) {
    const source = readFileSync(new URL(`../frontend/${page}`, import.meta.url), "utf8");
    assert.doesNotMatch(source, /\b(?:checkbox|badge|input|select)-xs\b|\bcheckbox-sm\b/, page);
    assert.doesNotMatch(source, /h\((?:"button"|ActionButton)[^)]*className:\s*"badge/, page);
  }
});

test("narrow and touch screens scale controls up, never below the 24px target", () => {
  const narrow = mediaBlock("(max-width: 720px), (pointer: coarse)");
  const desktop = px(declaration(rule(THEME_BLOCKS[0][1]), "--size-field"));
  const field = narrow.match(/^\s*(:root[^{]*)\{\s*--size-field:\s*([\d.]+rem);\s*\}/m);
  assert.ok(field, "narrow screens restate --size-field for both themes");
  for (const [, selector] of THEME_BLOCKS)
    for (const part of selector.split(/,\s*/)) assert.ok(field[1].includes(part), `narrow override covers ${part}`);
  assert.ok(px(field[2]) >= desktop, "narrow screens never shrink controls");
  assert.ok(px(field[2]) * FIELD.xs >= 24, `narrow xs control ${px(field[2]) * FIELD.xs}px`);
  // No media query lowers the target floor or the shared scale.
  for (const [, query] of css.matchAll(/@media ([^{]+)\{/g)) {
    const body = mediaBlock(query.trim());
    assert.doesNotMatch(body, /--target-min|--ui-font-size|--size-selector/, query);
  }
});

for (const [theme, selector] of [["light", ":root"], ["dark", ':root[data-theme="dark"]']]) {
  test(`${theme} compact text, header and tooltip colors keep 4.5:1`, () => {
    const colors = tokens(selector);
    const color = (body, property) => {
      const name = declaration(body, property)?.match(/^var\((--[\w-]+)\)$/)?.[1];
      assert.ok(name && colors[name], `${property} must use a theme token, got ${declaration(body, property)}`);
      return colors[name];
    };
    const tip = rule(".info-tip-text");
    const trigger = rule(".info-tip-trigger");
    const pairs = [
      ["tooltip text", color(tip, "color"), color(tip, "background")],
      ["tooltip trigger", color(trigger, "color"), colors["--panel"]],
      ["tooltip trigger hover", color(trigger, "color"), color(rule(".info-tip-trigger:hover"), "background")],
      ["page title", colors["--text"], colors["--bg"]],
      ["page header detail", color(rule(".muted"), "color"), colors["--bg"]],
      ["stat label", color(rule(".stat-title"), "color"), colors["--panel"]],
      ["setup step help", color(rule(".setup-link span span"), "color"), colors["--input"]],
      ["setup note", color(rule(".setup-note"), "color"), colors["--panel"]],
      ["field help", color(rule(".field-help"), "color"), colors["--panel"]],
      ["analytics note", color(rule(".dashboard-note p"), "color"), color(rule(".dashboard-note"), "background")],
      ["sidebar labels", colors["--side-muted"], colors["--sidebar"]],
      ["active navigation", colors["--selected-text"], colors["--selected"]],
    ];
    for (const [name, text, background] of pairs) {
      const ratio = contrast(text, background);
      assert.ok(ratio >= 4.5, `${theme} ${name}: ${ratio.toFixed(2)}:1`);
    }
  });
}

// #588: fixed-meaning icon colors. Each --icon-* intent has one light and one dark shade,
// separate from the daisyUI theme tokens, and every shade stays visible on each surface an
// icon is drawn on in its theme.
const iconIntents = JSON.parse(readFileSync(new URL("../frontend/icons.js", import.meta.url), "utf8")
  .match(/export const ICON_INTENTS = (\[[^\]]*\]);/)[1]);
function iconPalettes() {
  const palettes = {};
  for (const [, selector, body] of css.matchAll(/([^{}]+)\{([^{}]*--icon-[\w-]+\s*:[^{}]*)\}/g)) {
    const theme = /dark/.test(selector.replace(/\/\*[\s\S]*?\*\//g, "")) ? "dark" : "light";
    assert.equal(palettes[theme], undefined, `one ${theme} icon palette`);
    palettes[theme] = Object.fromEntries([...body.matchAll(/--icon-([\w-]+):\s*([^;]+);/g)].map(([, name, value]) => [name, value.trim()]));
  }
  for (const theme of ["light", "dark"]) assert.ok(palettes[theme], `missing ${theme} --icon-* palette`);
  return palettes;
}
// sRGB source-over: daisyUI menu hover paints base-content at 10% over the sidebar.
function over(color, base, alpha) {
  const [top, bottom] = [color, base].map((hex) => hex.slice(1).match(/../g).map((value) => parseInt(value, 16)));
  return `#${top.map((value, index) => Math.round(value * alpha + bottom[index] * (1 - alpha)).toString(16).padStart(2, "0")).join("")}`;
}
function hue(hex) {
  const [r, g, b] = hex.slice(1).match(/../g).map((value) => parseInt(value, 16) / 255);
  const max = Math.max(r, g, b), d = max - Math.min(r, g, b);
  if (!d) return 0;
  const h = max === r ? ((g - b) / d) % 6 : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
  return (h * 60 + 360) % 360;
}
function iconCssRule(selector) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const block = css.match(new RegExp(`(?:^|\\})\\s*${escaped}\\s*\\{([^}]*)\\}`, "m"));
  assert.ok(block, `missing ${selector} rule`);
  return block[1];
}

for (const [theme, selector] of [["light", ":root"], ["dark", ':root[data-theme="dark"]']]) {
  test(`${theme} icon colors keep 3:1 on every surface icons sit on`, () => {
    const colors = tokens(selector);
    const icons = iconPalettes()[theme];
    const surfaces = {
      panel: colors["--panel"], "panel2 (icon button hover)": colors["--panel2"], "hero (zebra rows)": colors["--hero"],
      bg: colors["--bg"], input: colors["--input"], sidebar: colors["--sidebar"], "side-hover": colors["--side-hover"],
      "selected (sidebar active)": colors["--selected"], "sidebar menu hover": over(colors["--text"], colors["--sidebar"], 0.1),
      ...Object.fromEntries(["warning", "info", "error", "success"].map((name) =>
        [`badge-soft ${name}`, mixOklab(colors[statusColor(name)], colors["--panel"], 0.08)])),
    };
    for (const intent of iconIntents) {
      for (const [surface, color] of Object.entries(surfaces)) {
        const ratio = contrast(icons[intent], color);
        assert.ok(ratio >= 3, `${theme} --icon-${intent} ${icons[intent]} on ${surface} ${color}: ${ratio.toFixed(2)}:1`);
      }
      // Readable as text on cards too, should an icon color ever label something.
      const text = contrast(icons[intent], colors["--panel"]);
      assert.ok(text >= 4.5, `${theme} --icon-${intent} as text on --panel: ${text.toFixed(2)}:1`);
    }
    // The icon button focus ring and hover outline stay visible on the same surfaces.
    for (const [token, floor] of [["--accent", 3], ["--text", 3]])
      for (const surface of ["--panel", "--panel2", "--hero", "--bg"]) {
        const ratio = contrast(colors[token], colors[surface]);
        assert.ok(ratio >= floor, `${theme} ${token} ring on ${surface}: ${ratio.toFixed(2)}:1`);
      }
  });
}

test("icon colors are their own palette, the same meanings in both themes", () => {
  const { light, dark } = iconPalettes();
  assert.ok(iconIntents.length >= 7);
  assert.deepEqual(Object.keys(light).sort(), [...iconIntents].sort(), "light theme defines every icon color");
  assert.deepEqual(Object.keys(dark).sort(), [...iconIntents].sort(), "dark theme defines every icon color");
  for (const [theme, selector] of [["light", ":root"], ["dark", ':root[data-theme="dark"]']]) {
    const palette = { light, dark }[theme];
    const themeColors = new Set(Object.values(tokens(selector)).map((value) => value.toLowerCase()));
    for (const [intent, value] of Object.entries(palette)) {
      assert.match(value, /^#[\da-f]{6}$/i, `${theme} --icon-${intent} is a fixed color, not a theme alias`);
      assert.ok(!themeColors.has(value.toLowerCase()), `${theme} --icon-${intent} is not a theme token color`);
    }
    assert.equal(new Set(Object.values(palette)).size, iconIntents.length, `${theme} meanings have distinct colors`);
  }
  for (const intent of iconIntents) {
    // One rule maps a meaning to its color for every theme; only the shade differs.
    assert.equal(iconCssRule(`svg.icon-${intent}`).trim(), `color: var(--icon-${intent});`);
    if (intent !== "neutral") {
      const drift = Math.abs(hue(light[intent]) - hue(dark[intent]));
      assert.ok(Math.min(drift, 360 - drift) <= 20, `--icon-${intent} keeps its hue across themes (${drift.toFixed(0)} degrees)`);
    }
  }
  // Icons never take the daisyUI theme colors.
  for (const [, selector, body] of css.matchAll(/([^{}]*(?:svg\.icon|btn-icon|nav-icon)[^{}]*)\{([^{}]*)\}/g)) {
    assert.doesNotMatch(body, /--color-(?:primary|secondary|accent)|var\(--(?:focus|selected)\)|(?:^|[;\s])color:\s*var\(--accent\)/,
      `${selector.trim()} uses a theme color for an icon`);
  }
});

test("icon buttons keep a 24px target and a visible focus ring", () => {
  const button = iconCssRule(".btn.btn-icon");
  const rem = (value) => Number(value.match(/([\d.]+)rem/)[1]);
  assert.ok(rem(button.match(/--size:\s*([^;]+)/)[1]) >= 1.5, "icon buttons render at least 24px square");
  for (const property of ["min-width", "min-height"]) {
    const value = button.match(new RegExp(`${property}:\\s*([^;]+)`))?.[1];
    assert.ok(value, `icon buttons set ${property}`);
    assert.ok(rem(value) >= 1.5, `${property} floor ${value} is at least 24px`);
  }
  assert.match(iconCssRule(".btn.btn-icon:focus-visible"), /outline:\s*2px solid var\(--accent\)/);
  assert.match(iconCssRule(".btn.btn-icon:hover:not(:disabled)"), /--btn-bg:\s*var\(--panel2\)/);
  assert.match(iconCssRule(".btn.btn-icon:hover:not(:disabled)"), /border-color:\s*var\(--text\)/);
  // Busy and disabled actions grey their icon with the control.
  assert.match(iconCssRule('.btn:is(:disabled, [aria-disabled="true"]) svg.icon'), /color:\s*inherit/);
  // Sidebar icons sit on their own --panel tile so the inverted active row keeps their color.
  const nav = iconCssRule(".sidebar svg.nav-icon");
  assert.match(nav, /background:\s*var\(--panel\)/);
  assert.ok(rem(nav.match(/width:\s*([^;]+)/)[1]) >= 1.25, "sidebar icons stay legible");
});

// #608: Dashboard Overview tiles and Getting started links draw icons from the shared map.
// Each icon color keeps 3:1 on the background its card or link actually paints, in both themes.
const iconMaps = vm.runInNewContext(`${readFileSync(new URL("../frontend/icons.js", import.meta.url), "utf8")
  .replace(/^import .*;$/gm, "").replaceAll("export ", "")}\n({ DASHBOARD_ICONS, NAV_ICONS })`, { React: {} });
const SETUP_LINK_TABS = ["rules", "commands", "logchannels"];
for (const [theme, selector] of [["light", ":root"], ["dark", ':root[data-theme="dark"]']]) {
  test(`${theme} dashboard card and setup link icons keep 3:1 on their backgrounds`, () => {
    const colors = tokens(selector);
    const icons = iconPalettes()[theme];
    const token = (body, property) => {
      const name = declaration(body, property)?.match(/^var\((--[\w-]+)\)$/)?.[1];
      assert.ok(name && colors[name], `${property} must use a theme token, got ${declaration(body, property)}`);
      return colors[name];
    };
    const tile = token(rule(".dashboard-section .stats"), "background");
    const link = token(rule(".btn.setup-link"), "--btn-color");
    // daisyUI 5 .btn:hover darkens --btn-color with 7% black in OKLab.
    const surfaces = { "Overview tile": tile, "Getting started card": colors["--panel"], "setup link": link, "setup link hover": mixOklab("#000000", link, 0.07) };
    const tiles = Object.entries(iconMaps.DASHBOARD_ICONS);
    assert.equal(tiles.length, 12, "one icon per Overview tile");
    const checks = [
      ...tiles.map(([key, entry]) => [`tile ${key}`, entry, ["Overview tile", "Getting started card"]]),
      ...SETUP_LINK_TABS.map((tab) => [`setup link ${tab}`, iconMaps.NAV_ICONS[tab], ["setup link", "setup link hover"]]),
    ];
    for (const [name, entry, on] of checks) {
      assert.ok(icons[entry.intent], `${name} uses a palette color, got ${entry.intent}`);
      for (const surface of on) {
        const ratio = contrast(icons[entry.intent], surfaces[surface]);
        assert.ok(ratio >= 3, `${theme} ${name} --icon-${entry.intent} ${icons[entry.intent]} on ${surface} ${surfaces[surface]}: ${ratio.toFixed(2)}:1`);
      }
    }
  });
}

test("dashboard icons use the shared icon size, not a page-specific scale", () => {
  assert.match(iconCssRule("svg.icon"), /width:\s*1rem;\s*height:\s*1rem/);
  for (const [, selector, body] of css.matchAll(/([^{}]*(?:dashboard|stat|setup)[^{}]*)\{([^{}]*)\}/g)) {
    if (!/\bsvg\b|\.icon\b/.test(selector)) continue;
    assert.doesNotMatch(body, /(?:^|[;\s])(?:width|height|font-size|--size)\s*:/, `${selector.trim()} resizes dashboard icons`);
  }
  // A busy setup link still greys its icon with the label.
  assert.match(iconCssRule('.btn:is(:disabled, [aria-disabled="true"]) svg.icon'), /color:\s*inherit/);
});
