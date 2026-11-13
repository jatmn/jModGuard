import React from "react";

// The one Web UI icon set (#588): hand-authored 24×24 round stroke paths, no
// icon-library dependency. The Web UI shows these SVGs, never emoji or glyph icons.
// Each icon is drawn in a fixed color by meaning (an "intent"): the --icon-*
// palette in frontend/style.css, independent of the daisyUI theme tokens.
export const ICON_PATHS = {
  // Row and record actions.
  hammer: ["M9.5 7.5l4-4l7 7l-4 4z", "M13 11l-9 9"],
  "user-check": ["M5 7a4 4 0 1 0 8 0a4 4 0 1 0-8 0", "M3 21v-1a5 5 0 0 1 5-5h2a5 5 0 0 1 5 5v1", "M16 11l2 2l4-4"],
  trash: [
    "M4 7h16",
    "M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2",
    "M6 7l1 13a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-13",
    "M10 11v7",
    "M14 11v7",
  ],
  pencil: ["M17 3l4 4L8 20l-5 1 1-5L17 3z", "M14 6l4 4"],
  eye: ["M2 12s3.6-7 10-7s10 7 10 7s-3.6 7-10 7S2 12 2 12z", "M9 12a3 3 0 1 0 6 0a3 3 0 1 0-6 0"],
  copy: [
    "M11 9h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2h-8a2 2 0 0 1-2-2v-8a2 2 0 0 1 2-2z",
    "M5 15a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2",
  ],
  "x-circle": ["M3 12a9 9 0 1 0 18 0a9 9 0 1 0-18 0", "M15 9l-6 6", "M9 9l6 6"],
  "check-circle": ["M3 12a9 9 0 1 0 18 0a9 9 0 1 0-18 0", "M8 12l3 3l5-6"],
  "alert-circle": ["M3 12a9 9 0 1 0 18 0a9 9 0 1 0-18 0", "M12 8v5", "M12 16v.01"],
  "arrow-up-circle": ["M3 12a9 9 0 1 0 18 0a9 9 0 1 0-18 0", "M12 16V8", "M8 12l4-4l4 4"],
  download: ["M12 4v11", "M7 10l5 5l5-5", "M4 17v2a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-2"],
  refresh: ["M20 12a8 8 0 1 1-2.34-5.66L20 8", "M20 4v4h-4"],
  "log-out": ["M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4", "M16 17l5-5l-5-5", "M21 12H9"],
  "external-link": ["M14 4h6v6", "M20 4l-9 9", "M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4"],
  "arrow-left": ["M19 12H5", "M11 6l-6 6l6 6"],
  // Navigation.
  "layout-grid": ["M4 4h6v6H4z", "M14 4h6v6h-6z", "M4 14h6v6H4z", "M14 14h6v6h-6z"],
  "bar-chart": ["M4 4v16h16", "M9 16v-5", "M13 16V8", "M17 16v-3"],
  shield: ["M12 3l7 3v5c0 4.5-3 8-7 10c-4-2-7-5.5-7-10V6z", "M9 12l2 2l4-4"],
  "message-x": [
    "M5 4h14a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1h-8l-5 4v-4H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z",
    "M10 8l4 4",
    "M14 8l-4 4",
  ],
  mail: ["M4 5h16a1 1 0 0 1 1 1v12a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V6a1 1 0 0 1 1-1z", "M3 7l9 6l9-6"],
  "image-off": [
    "M5 4h14a1 1 0 0 1 1 1v14a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5a1 1 0 0 1 1-1z",
    "M4 16l5-5l4 4l2-2l5 5",
    "M15 8.5v.01",
    "M3 3l18 18",
  ],
  terminal: ["M5 7l5 5l-5 5", "M12 17h7"],
  "file-text": ["M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8z", "M14 3v5h5", "M9 13h6", "M9 17h6"],
  hash: ["M9 4L7 20", "M17 4l-2 16", "M4 9h16", "M3 15h16"],
  list: ["M9 6h11", "M9 12h11", "M9 18h11", "M4 6v.01", "M4 12v.01", "M4 18v.01"],
  users: [
    "M6 8a3 3 0 1 0 6 0a3 3 0 1 0-6 0",
    "M3 20v-1a5 5 0 0 1 5-5h2a5 5 0 0 1 5 5v1",
    "M16 5a3 3 0 0 1 0 6",
    "M18 14a4 4 0 0 1 3 4v2",
  ],
  bot: ["M6 8h12a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2v-8a2 2 0 0 1 2-2z", "M12 8V4", "M9 13v2", "M15 13v2"],
  briefcase: ["M4 8h16v11H4z", "M9 8V5h6v3", "M4 13h16"],
  sliders: ["M4 6h9", "M17 6h3", "M15 4v4", "M4 12h3", "M11 12h9", "M9 10v4", "M4 18h11", "M19 18h1", "M17 16v4"],
  server: ["M4 4h16v6H4z", "M4 14h16v6H4z", "M8 7v.01", "M8 17v.01"],
  crown: ["M3 17l2-10l5 4l2-6l2 6l5-4l2 10z", "M3 21h18"],
  database: [
    "M4 6c0-1.7 3.6-3 8-3s8 1.3 8 3s-3.6 3-8 3s-8-1.3-8-3z",
    "M4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6",
    "M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3",
  ],
};

// Fixed color intents. Each has a --icon-<intent> light and dark shade in style.css.
export const ICON_INTENTS = ["danger", "success", "info", "warning", "analytics", "members", "neutral"];

// One meaning per action, shared by every page: destructive red, restorative
// green, edits and views blue, warnings amber, copy/cancel gray. Actions join the
// set when a page uses them.
export const ACTION_ICONS = {
  ban: { icon: "hammer", intent: "danger" },
  unban: { icon: "user-check", intent: "success" },
  remove: { icon: "trash", intent: "danger" },
  leave: { icon: "log-out", intent: "danger" },
  edit: { icon: "pencil", intent: "info" },
  view: { icon: "eye", intent: "info" },
  download: { icon: "download", intent: "info" },
  retry: { icon: "refresh", intent: "info" },
  copy: { icon: "copy", intent: "neutral" },
  cancel: { icon: "x-circle", intent: "neutral" },
  approve: { icon: "check-circle", intent: "success" },
  attention: { icon: "alert-circle", intent: "danger" },
  update: { icon: "arrow-up-circle", intent: "warning" },
  back: { icon: "arrow-left", intent: "neutral" },
};

// Sidebar and page navigation. Meanings match the Discord templates' emoji where
// they overlap (#592: hammer for Bans, bar chart for Analytics); the Web UI draws SVGs.
export const NAV_ICONS = {
  dashboard: { icon: "layout-grid", intent: "info" },
  analytics: { icon: "bar-chart", intent: "analytics" },
  rules: { icon: "shield", intent: "success" },
  badwords: { icon: "message-x", intent: "warning" },
  "known-spam": { icon: "mail", intent: "danger" },
  "known-bad-images": { icon: "image-off", intent: "danger" },
  commands: { icon: "terminal", intent: "info" },
  templates: { icon: "file-text", intent: "info" },
  logchannels: { icon: "hash", intent: "neutral" },
  events: { icon: "list", intent: "neutral" },
  members: { icon: "users", intent: "members" },
  bans: { icon: "hammer", intent: "danger" },
  actions: { icon: "bot", intent: "info" },
  cases: { icon: "briefcase", intent: "warning" },
  settings: { icon: "sliders", intent: "neutral" },
  servers: { icon: "server", intent: "members" },
  updates: { icon: "arrow-up-circle", intent: "info" },
  // The sidebar's own buttons: owner tools, preview exit and the account footer.
  administration: { icon: "crown", intent: "info" },
  "view-as": { icon: "eye", intent: "info" },
  "exit-preview": { icon: "arrow-left", intent: "neutral" },
  "sign-out": { icon: "log-out", intent: "neutral" },
  usage: { icon: "database", intent: "neutral" },
};

// Decorative: the accessible name always comes from the surrounding control or text.
// Without an intent the icon follows its control's text color (used inside filled buttons).
export function Icon({ name, intent, className = "" }) {
  return React.createElement(
    "svg",
    {
      className: ["icon", intent && `icon-${intent}`, className].filter(Boolean).join(" "),
      "data-icon": name,
      viewBox: "0 0 24 24",
      fill: "none",
      stroke: "currentColor",
      strokeWidth: 2,
      strokeLinecap: "round",
      strokeLinejoin: "round",
      "aria-hidden": "true",
      focusable: "false",
    },
    ICON_PATHS[name].map((d, index) => React.createElement("path", { key: index, d })),
  );
}
