import React, { useState, useEffect, useLayoutEffect, useRef, createContext, useContext } from "react";
import { createRoot } from "react-dom/client";
import {
  Dialog,
  DialogPanel,
  Field,
  Label,
  HeadlessInput,
  HeadlessSelect,
  HeadlessTextarea,
} from "../tools/webui/components.js";

const h = React.createElement;
const $ = (selector) => document.querySelector(selector);
const BusyContext = createContext(false);
const root = createRoot(document.getElementById("app"));
let view = "startup-view",
  startupStatus = "Waiting for the API to finish starting…",
  previewUnavailable = false;
let committedRevision = -1;
let contentRevision = 0,
  editor = null,
  notification = null,
  toastTimer;
function render() {
  root.render(h(App));
}

// The shared busy policy belongs to the application; React owns the controls.
function ActionButton({ onClick, savedInputs, disabled, ...props }) {
  const working = useContext(BusyContext);
  return h("button", {
    ...props,
    disabled: working || disabled,
    onClick: onClick && ((event) => handleEvent(event.currentTarget, onClick, event, savedInputs)),
  });
}
function Input(props) {
  const working = useContext(BusyContext);
  return h(HeadlessInput, { ...props, disabled: working || props.disabled });
}
function Textarea(props) {
  const working = useContext(BusyContext);
  return h(HeadlessTextarea, { ...props, disabled: working || props.disabled });
}
function Select({ className = "", children, ...props }) {
  const working = useContext(BusyContext);
  return h(HeadlessSelect, { ...props, disabled: working || props.disabled, className }, children);
}
function Appearance({ preference, setPreference }) {
  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => {
      const theme = preference === "system" ? (media.matches ? "dark" : "light") : preference;
      document.documentElement.dataset.theme = theme;
      document.querySelector('meta[name="theme-color"]').content = theme === "dark" ? "#101e33" : "#e0eeff";
    };
    apply();
    media.addEventListener("change", apply);
    return () => media.removeEventListener("change", apply);
  }, [preference]);
  return h(
    "label",
    { className: "theme-picker" },
    "Appearance ",
    h(
      "select",
      {
        className: "theme-select select select-sm",
        "aria-label": "Appearance",
        value: preference,
        onChange: (event) => {
          setPreference(event.target.value);
          try {
            window.localStorage.setItem("jmodguard.theme", event.target.value);
          } catch (_) {}
        },
      },
      ...["system", "light", "dark"].map((value) =>
        h("option", { key: value, value }, value[0].toUpperCase() + value.slice(1)),
      ),
    ),
  );
}
function OwnerTools() {
  const profile = useRef(null);
  if (isPreview())
    return h(
      "div",
      { id: "owner-tools", className: "owner-tools" },
      h(
        ActionButton,
        {
          className: "btn btn-sm",
          onClick: async () => {
            try {
              await api("/api/preview", "DELETE");
            } catch (_) {}
            clearPreviewToken();
            window.location = "/";
          },
        },
        "Exit preview",
      ),
    );
  if (!isBotOwner()) return null;
  return h(
    "div",
    { id: "owner-tools", className: "owner-tools" },
    h("a", { className: "btn btn-sm", href: "/administration/index.html" }, "Administration"),
    h(
      ActionButton,
      {
        className: "btn btn-sm",
        onClick: async () => {
          if (!state.guildId) return toast("Select a server first", "error");
          const launched = await api("/api/preview", "POST", {
            guild_id: String(state.guildId),
            profile: profile.current.value,
          });
          window.open(
            `${location.pathname}#preview=${encodeURIComponent(launched.token)}`,
            "_blank",
            "noopener",
          );
        },
      },
      "View as…",
    ),
    h(
      Select,
      { ref: profile, className: "select", "aria-label": "Preview profile" },
      ...["administrator", "moderator", "member"].map((value) =>
        h("option", { key: value, value }, value[0].toUpperCase() + value.slice(1)),
      ),
    ),
  );
}
function App() {
  const [preference, setPreference] = useState(() => {
    let value;
    try {
      value = window.localStorage.getItem("jmodguard.theme");
    } catch (_) {}
    return ["system", "light", "dark"].includes(value) ? value : "system";
  });

  useEffect(() => {
    const beforeUnload = (event) => {
      if (dirty || busy) {
        event.preventDefault();
        event.returnValue = "";
      }
    };
    window.addEventListener("beforeunload", beforeUnload);
    return () => window.removeEventListener("beforeunload", beforeUnload);
  }, []);
  const navigation = [
    ["dashboard", "◈", "Dashboard"],
    ["rules", "◇", "Automod Rules"],
    ["badwords", "⊘", "Badwords"],
    ["commands", "⌘", "Commands"],
    ["templates", "≡", "Templates"],
    ["logchannels", "#", "Log Channels"],
    ["events", "≋", "Audit Log"],
    ["members", "♙", "Members"],
    ["bans", "⊖", "Bans"],
    ["actions", "↻", "Bot actions"],
    ["cases", "▣", "Cases"],
    ["settings", "⚙", "Settings"],
  ];
  return h(
    BusyContext.Provider,
    { value: busy },
    isPreview() &&
      h(
        "div",
        { id: "preview-banner", className: "preview-banner alert alert-warning", role: "status" },
        "Preview — changes disabled",
      ),
    view === "main-view"
      ? h(
          "div",
          { id: "main-view" },
          h(
            "aside",
            { className: "sidebar" },
            h(
              "div",
              { className: "brand" },
              h("span", { className: "brand-mark", "aria-hidden": true }, "jG"),
              h("span", null, "jModGuard", h("small", null, "MODERATION WORKSPACE")),
            ),
            h(
              "div",
              { className: "server-picker" },
              h("label", { htmlFor: "guild-select" }, "YOUR SERVER"),
              h(
                Select,
                {
                  id: "guild-select",
                  "aria-label": "YOUR SERVER",
                  className: "select select-sm w-full",
                  value: state.guildId || "",
                  onChange: async (event) => {
                    if (!canLeave()) return;
                    pendingDrafts = null;
                    pendingCase = null;
                    state.guildId = event.target.value;
                    state.config = null;
                    dirty = false;
                    persistWorkspace();
                    if (await loadConfig()) renderTab();
                  },
                },
                state.guilds.length
                  ? state.guilds.map((guild) => h("option", { key: guild.id, value: guild.id }, guild.name))
                  : h("option", { value: "" }, "No manageable servers"),
              ),
            ),
            h(
              "nav",
              { "aria-label": "Workspace navigation" },
              h(
                "ul",
                { id: "tabs", className: "menu w-full" },
                ...navigation.flatMap(([tab, icon, label], i) => [
                  (i === 0 || i === 6) &&
                    h(
                      "li",
                      { key: `group-${i}`, className: "nav-label menu-title" },
                      i === 0 ? "Workspace" : "Community",
                    ),
                  h(
                    "li",
                    { key: tab },
                    h(
                      ActionButton,
                      {
                        "data-tab": tab,
                        className: state.tab === tab ? "active menu-active btn-active" : "",
                        "aria-current": state.tab === tab ? "page" : undefined,
                        onClick: () => navigate(tab),
                      },
                      h("span", { "aria-hidden": true }, icon),
                      " " + label,
                    ),
                  ),
                ]),
              ),
            ),
            h(OwnerTools),
            h(
              "div",
              { className: "sidebar-footer" },
              h(Appearance, { preference, setPreference }),
              h(
                "div",
                { className: "sidebar-user" },
                h(
                  "span",
                  { id: "user-chip" },
                  `${state.me?.user.username} (${state.me?.user.global_name || ""})`.trim(),
                ),
                h(
                  ActionButton,
                  { id: "logout-btn", className: "btn btn-sm btn-ghost", onClick: logout },
                  "Sign out",
                ),
              ),
            ),
          ),
          h(
            "div",
            { className: "workspace" },
            h(
              "header",
              { className: "navbar bg-base-100 workspace-header" },
              h(
                "div",
                { className: "workspace-title" },
                h("span", { className: "eyebrow" }, "SERVER WORKSPACE"),
                h("strong", { id: "workspace-name" }, selectedGuild()?.name || "Your servers"),
              ),
              h(
                "span",
                { id: "workspace-status" },
                isPreview()
                  ? "Preview — changes disabled"
                  : busy
                    ? "Working…"
                    : dirty
                      ? "Unsaved changes"
                      : "Changes saved with each action",
              ),
            ),
            h(
              "main",
              { id: "content", tabIndex: -1, onInput: trackDraft },
              h(Content, { key: contentRevision }),
            ),
            h(
              "footer",
              { className: "workspace-footer" },
              "jModGuard ",
              h("span", null, "Built for better communities."),
            ),
          ),
        )
      : h(
          "div",
          { id: view, className: "welcome-view" },
          h(Appearance, { preference, setPreference }),
          h(
            "div",
            { className: "login-card card bg-base-100" },
            h(
              "div",
              { className: "card-body items-center text-center" },
              h("span", { className: "brand-mark", "aria-hidden": true }, "jG"),
              h(
                "p",
                { className: "eyebrow" },
                view === "login-view" ? "JMODGUARD / MODERATION WORKSPACE" : "YOUR COMMUNITY, IN GOOD HANDS",
              ),
              view === "login-view"
                ? h(
                    React.Fragment,
                    null,
                    h("h1", null, "A calmer place", h("br"), "to keep order."),
                    h(
                      "p",
                      null,
                      "Thoughtful protection for your Discord community.",
                      h("br"),
                      "Manage your rules, people, and settings in one place.",
                    ),
                    h(
                      "a",
                      { id: "login-link", className: "btn btn-primary", href: authUrl("/login") },
                      "Continue with Discord ",
                      h("span", { "aria-hidden": true }, "↗"),
                    ),
                    h("small", null, "For server managers and community builders."),
                  )
                : h(
                    React.Fragment,
                    null,
                    h("h1", null, "Getting things ready."),
                    h("p", { id: "startup-status", role: "status" }, startupStatus),
                    previewUnavailable &&
                      h(
                        "a",
                        {
                          id: "preview-return-link",
                          className: "btn btn-primary",
                          href: "/",
                          onClick: returnFromPreview,
                        },
                        "Return to owner dashboard",
                      ),
                  ),
            ),
          ),
        ),
    (!editor || view !== "main-view") && h(Notification),
    view === "main-view" &&
      h(
        Dialog,
        {
          open: !!editor,
          onClose: () => {
            if (canLeave()) closeOverlay();
          },
          className: "modal modal-open modal-overlay",
          "aria-label": "Configuration editor",
        },
        editor,
        h(Notification),
      ),
  );
}
function Notification() {
  return h(
    "div",
    {
      id: "toast",
      role: "status",
      "aria-live": "polite",
      "aria-atomic": true,
      className: `toast toast-end toast-bottom ${notification?.kind || ""}`,
      hidden: !notification,
    },
    notification && h("div", {
      className: `alert alert-soft ${notification.kind === "error" ? "alert-error" : notification.kind === "success" ? "alert-success" : ""}`,
    }, notification.message),
  );
}
function trackDraft(event) {
  if (
    event.target.matches("input, textarea, select") &&
    !["events", "members", "bans"].includes(state.tab)
  )
    markDirty();
}
function Content() {
  useLayoutEffect(() => {
    committedRevision = contentRevision;
    const content = $("#content");
    content.querySelectorAll("input, select, textarea").forEach((input, i) => {
      input.dataset.inputKey = String(i);
      input._initialValue = input.value;
    });
    restoreDrafts();
  }, []);
  const components = {
    dashboard: tabDashboard,
    rules: tabRules,
    badwords: tabBadwords,
    commands: tabCommands,
    templates: tabTemplates,
    logchannels: tabLogChannels,
    events: tabEvents,
    members: tabMembers,
    bans: tabBans,
    actions: tabActions,
    cases: tabCases,
    settings: tabSettings,
  };
  if (!state.guildId)
    return emptyPanel(
      "No servers to manage yet",
      "Sign in with an account that has Manage Server permission in a server using jModGuard.",
    );
  if (!state.config)
    return emptyPanel(
      state.loading ? "Loading your workspace…" : "Configuration unavailable",
      state.error || "Fetching saved settings for this server.",
      !state.loading &&
        h(
          ActionButton,
          {
            className: "btn btn-primary",
            onClick: async () => {
              if (await loadConfig()) renderTab();
            },
          },
          "Retry",
        ),
      { role: "status" },
    );
  return h(components[state.tab] || tabDashboard);
}
function renderTab() {
  dirty = false;
  contentRevision++;
  render();
}
function updateWorkspaceStatus() {
  render();
}
function setBusy(value) {
  busy = value;
  render();
}
function showView(id) {
  view = id;
  render();
}
function setStartupStatus(text) {
  startupStatus = text;
  render();
}
function showPreviewUnavailable() {
  view = "startup-view";
  previewUnavailable = true;
  startupStatus =
    "This preview is no longer signed in. Return to the owner dashboard and launch a new preview.";
  render();
}
function toast(message, kind = "info") {
  notification = { message, kind };
  clearTimeout(toastTimer);
  toastTimer = setTimeout(
    () => {
      notification = null;
      render();
    },
    kind === "error" ? 5000 : 2600,
  );
  render();
}
function closeOverlay() {
  editor = null;
  dirty = false;
  render();
}
function editRule(record) {
  editor = h(RuleEditor, { record });
  render();
}
function editCommand(record) {
  editor = h(CommandEditor, { record });
  render();
}
async function init() {
  try {
    state.me = await waitForApiMe();
    state.guilds = state.me.guilds;
    restoreWorkspace();
    showView("main-view");
    if (await loadConfig()) renderTab();
  } catch (error) {
    if (!handleAuthLoss(error)) {
      showView("startup-view");
      setStartupStatus("Failed to load: " + (error.message || String(error)));
      toast("Failed to load: " + error.message, "error");
    }
  }
}
async function logout() {
  if (!canLeave()) return;
  dirty = false;
  if (isPreview()) {
    try {
      await api("/api/preview", "DELETE");
    } catch (_) {}
    clearPreviewToken();
    window.location = "/";
    return;
  }
  clearPreviewToken();
  try {
    const response = await fetch(authUrl("/logout"), { method: "POST", credentials: "include" });
    const data = await response.json();
    window.location = data.return_to || "/";
  } catch (_) {
    window.location = "/";
  }
}

let state = {
  me: null,
  guilds: [],
  guildId: null,
  config: null,
  tab: "dashboard",
  loading: false,
  error: null,
};

let configRequest = 0;

let busy = false;

let dirty = false;

let pendingDrafts = null;

const pendingLogChannelActions = new Map();

const actionReconcileDelays = [1000, 1500, 2500, 5000, 10000, 20000, 40000, 80000, 120000, 150000];

const tabs = [
  "dashboard",
  "rules",
  "badwords",
  "commands",
  "templates",
  "logchannels",
  "events",
  "members",
  "bans",
  "actions",
  "cases",
  "settings",
];

let pendingCase = null;

function workspaceKey() {
  return `jmodguard.workspace:${apiBase()}:${state.me.user.id}`;
}

function restoreWorkspace() {
  let saved = {};
  try {
    saved = JSON.parse(window.localStorage.getItem(workspaceKey())) || {};
  } catch (_) {
    /* Storage may be unavailable. */
  }
  state.guildId = state.guilds.find((g) => g.id === saved.guildId)?.id || state.guilds[0]?.id || null;
  state.tab = tabs.includes(saved.tab) ? saved.tab : "dashboard";
}

function persistWorkspace() {
  try {
    window.localStorage.setItem(workspaceKey(), JSON.stringify({ guildId: state.guildId, tab: state.tab }));
  } catch (_) {
    /* Session still works without storage. */
  }
}

function canLeave() {
  return !busy && (!dirty || confirm("Discard unsaved changes?"));
}

function markDirty() {
  dirty = true;
  updateWorkspaceStatus();
}

function currentDraftInputs() {
  return [...($("#content")?.querySelectorAll("input, select, textarea") || [])].filter(
    (input) =>
      input._initialValue !== undefined &&
      input.value !== input._initialValue,
  );
}

function currentDrafts() {
  return currentDraftInputs().map((input) => ({ key: input.dataset.inputKey, value: input.value }));
}

function captureDrafts(node, savedInputs) {
  const form = node.closest?.(".card, .modal-box");
  if (!form) return [];
  return currentDraftInputs()
    .filter(
      (input) => !(savedInputs ? savedInputs.some((ref) => ref.current === input) : form.contains(input)),
    )
    .map((input) => ({ key: input.dataset.inputKey, value: input.value }));
}

function retainDrafts(guildId, tab, drafts, enrollmentId = state.config?.enrollment_id) {
  if (!drafts.length || guildId !== state.guildId || tab !== state.tab) return;
  pendingDrafts = { guildId, tab, drafts, enrollmentId };
  restoreDrafts();
}

function restoreDrafts() {
  if (committedRevision !== contentRevision) return;
  if (!pendingDrafts || pendingDrafts.guildId !== state.guildId || pendingDrafts.tab !== state.tab) return;
  if (state.config && pendingDrafts.enrollmentId !== state.config.enrollment_id) {
    pendingDrafts = null;
    dirty = false;
    updateWorkspaceStatus();
    return;
  }
  markDirty();
  // The unavailable/loading screen has no inputs. Keep the draft until Retry succeeds.
  if (!state.config) return;
  for (const draft of pendingDrafts.drafts) {
    const input = $("#content").querySelector(`[data-input-key="${draft.key}"]`);
    if (input) {
      input.value = draft.value;
      input.dispatchEvent?.(new Event("input", { bubbles: true }));
    }
  }
  pendingDrafts = null;
}

async function handleEvent(node, handler, event, savedInputs) {
  if (busy) return;
  const guildId = state.guildId,
    tab = state.tab,
    enrollmentId = state.config?.enrollment_id,
    revision = contentRevision;
  const drafts = captureDrafts(node, savedInputs);
  try {
    const result = handler(event);
    if (result && typeof result.then === "function") {
      setBusy(true);
      await result;
      if (revision !== contentRevision || !node.isConnected) retainDrafts(guildId, tab, drafts, enrollmentId);
    }
  } catch (e) {
    reportApiError(e);
  } finally {
    if (busy) setBusy(false);
  }
}

function navigate(tab) {
  if (!canLeave()) return false;
  view = "main-view";
  pendingDrafts = null;
  state.tab = tabs.includes(tab) ? tab : "dashboard";
  persistWorkspace();
  renderTab();
  return true;
}

function isBotOwner() {
  return !!(state.me && state.me.is_bot_owner);
}

let previewTokenFallback = null;

function capturePreviewToken() {
  const fragment = new URLSearchParams(location.hash.replace(/^#/, ""));
  const token = fragment.get("preview");
  if (!token) return;
  // Keep an in-memory source of truth before touching browser storage. A
  // blocked sessionStorage must never silently restore the owner's write access.
  previewTokenFallback = token;
  try {
    window.sessionStorage.setItem("jmodguard_preview", token);
  } catch (_) {
    /* The in-memory fallback remains active for this tab. */
  }
  // The tab-bound fragment remains the reload-safe source if storage later
  // disappears. Fragments are not sent in HTTP requests or Referer headers.
}

function currentPreviewToken() {
  try {
    return window.sessionStorage.getItem("jmodguard_preview") || previewTokenFallback;
  } catch (_) {
    return previewTokenFallback;
  }
}

function clearPreviewToken() {
  previewTokenFallback = null;
  try {
    window.sessionStorage.removeItem("jmodguard_preview");
  } catch (_) {}
  const fragment = new URLSearchParams(location.hash.replace(/^#/, ""));
  if (fragment.has("preview")) {
    fragment.delete("preview");
    const hash = fragment.toString();
    try {
      history.replaceState({}, "", location.pathname + location.search + (hash ? `#${hash}` : ""));
    } catch (_) {
      /* The following explicit navigation still drops the fragment. */
    }
  }
}

function leaveExpiredPreview() {
  clearPreviewToken();
  if (typeof window !== "undefined" && window.location) {
    if (typeof window.location.replace === "function") window.location.replace("/");
    else window.location.href = "/";
  }
}

function isPreview() {
  return !!(state.me && state.me.preview);
}

function apiBase() {
  return ((typeof window !== "undefined" && window.JMODGUARD_API_BASE) || "").replace(/\/$/, "");
}

function apiUrl(path) {
  return apiBase() + path;
}

function authUrl(path) {
  const base = apiBase();
  if (!base) return path;
  return `${base}${path}?return_to=${encodeURIComponent(window.location.origin)}`;
}

function apiError(message, status, retryAfterMs) {
  const err = new Error(message);
  err.status = status;
  err.retryAfterMs = retryAfterMs;
  return err;
}

function parseRetryAfterMs(header, capSeconds = 30) {
  if (!header) return 0;
  const seconds = Number(header);
  if (!Number.isFinite(seconds) || seconds < 0) return 0;
  return Math.min(seconds, capSeconds) * 1000;
}

function isDiscordRateLimited(err) {
  return !!err && (err.status === 429 || err.code === "discord_rate_limited");
}

function isUnauthenticated(err) {
  const msg = String(err && err.message);
  return (
    err.status === 401 ||
    msg.includes("not authenticated") ||
    msg === "session expired" ||
    msg.includes("401")
  );
}

function handleAuthLoss(error) {
  if (!isUnauthenticated(error)) return false;
  if (currentPreviewToken()) {
    showPreviewUnavailable();
    return true;
  }
  showView("login-view");
  return true;
}

function reportApiError(error) {
  if (!handleAuthLoss(error)) toast(error.message || String(error), "error");
}

function isApiWarmup(err) {
  return err.status === 503 && String(err && err.message) === "not ready";
}

function isOriginStarting(err) {
  if (isDiscordRateLimited(err)) return false;
  if (isApiWarmup(err)) return true;
  if (!err || !err.status) return true;
  if (err.status === 502 || err.status === 504) return true;
  return err.status === 503 && String(err && err.message) === "HTTP 503";
}

const API_WARMUP_BUDGET_MS = 8 * 60 * 1000;

const ORIGIN_REACHABILITY_BUDGET_MS = 90 * 1000;

function originGaveUpMessage(err) {
  if (isApiWarmup(err)) {
    return "API did not become ready in time. Start the bot first and wait for database ready (migrate=true).";
  }
  return "Cannot reach the API. Check that the API container is running and that the Pull Zone OriginUrl rules send /api to it.";
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function returnFromPreview() {
  clearPreviewToken();
}

function pageEnrollment(path, method = "GET") {
  // Bind edits and scoped reads to the membership this page loaded. Never
  // fetch a new token or retry against a replacement installation.
  const edit = path.match(
    /^\/api\/guilds\/(\d+)\/(settings|rules|badwords|commands|templates|copy|logchannels|logfilters|unban|actions|events|members|member-sync|bans|stats|cases|case-delete-roles)(?:\/|\?|$)/,
  );
  if (
    edit &&
    (method !== "GET" ||
      [
        "rules",
        "badwords",
        "commands",
        "templates",
        "logchannels",
        "actions",
        "events",
        "members",
        "member-sync",
        "bans",
        "stats",
        "cases",
        "case-delete-roles",
      ].includes(edit[2])) &&
    edit[1] !== "0" &&
    edit[1] === state.guildId &&
    state.config?.enrollment_id
  ) {
    return state.config.enrollment_id;
  }
  return null;
}

function credentialedOpts(path, method) {
  // credentials: include so cross-origin CDN SPA (JMODGUARD_API_BASE) sends the session cookie.
  const opts = { method, headers: {}, credentials: "include", cache: "no-store" };
  const enrollment = pageEnrollment(path, method);
  if (enrollment) opts.headers["x-jmodguard-enrollment"] = enrollment;
  const preview = currentPreviewToken();
  if (preview) opts.headers["x-jmodguard-preview"] = preview;
  return opts;
}

function throwApiFailure(data, status, retryAfterMs) {
  const msg = data && (data.error || data.message) ? data.error || data.message : `HTTP ${status}`;
  const error = apiError(msg, status, retryAfterMs);
  error.code = data && data.code;
  if (error.code === "preview_expired") leaveExpiredPreview();
  else handleAuthLoss(error);
  throw error;
}

async function api(path, method = "GET", body = null, signal) {
  const opts = credentialedOpts(path, method);
  const controller = signal ? null : new AbortController();
  const timeout = controller ? setTimeout(() => controller.abort(), 30000) : null;
  opts.signal = signal || controller.signal;
  try {
    if (body !== null) {
      opts.headers["content-type"] = "application/json";
      opts.body = JSON.stringify(body);
    }
    const res = await fetch(apiUrl(path), opts);
    const text = await res.text();
    let data = null;
    try {
      data = text ? JSON.parse(text) : null;
    } catch (_) {
      data = text;
    }
    if (!res.ok) throwApiFailure(data, res.status, parseRetryAfterMs(res.headers.get("retry-after"), 900));
    return data;
  } catch (e) {
    if (e && (e.name === "AbortError" || e.name === "TimeoutError")) {
      throw apiError(
        signal ? "Failed to fetch" : "Request timed out. Refresh to verify saved state before trying again.",
      );
    }
    throw e;
  } finally {
    if (timeout !== null) clearTimeout(timeout);
  }
}

function apiRead(path, signal) {
  return api(path, "POST", null, signal);
}

async function downloadPrivate(path, filename) {
  const opts = credentialedOpts(path, "GET");
  const res = await fetch(apiUrl(path), opts);
  if (!res.ok) {
    let data = null;
    try {
      data = JSON.parse(await res.text());
    } catch (_) {}
    throwApiFailure(data, res.status);
  }
  const blob = await res.blob();
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = filename || "evidence";
  link.rel = "noopener";
  document.body.appendChild(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}

async function submitBotAction(path, method, body) {
  const canonicalBody = Object.fromEntries(
    Object.keys(body)
      .sort()
      .map((key) => [key, body[key]]),
  );
  const key = `${workspaceKey()}:action:${method}:${path}:${JSON.stringify(canonicalBody)}`;
  const enrollmentId = state.config?.enrollment_id;
  const guildId = path.split("/")[3];
  if (!enrollmentId || guildId !== state.guildId)
    throw new Error("Reload this server before requesting an action.");
  let saved;
  try {
    const raw = window.sessionStorage.getItem(key);
    saved = raw
      ? JSON.parse(raw)
      : {
          request_id: `${Math.floor(Date.now() / 1000)}_${crypto.randomUUID()}`,
          enrollment_id: enrollmentId,
        };
    window.sessionStorage.setItem(key, JSON.stringify(saved));
  } catch (_) {
    throw new Error("Browser session storage must be available and valid to safely submit bot actions.");
  }
  if (!saved || typeof saved.request_id !== "string" || saved.enrollment_id !== enrollmentId) {
    if (
      confirm(
        "This saved request belongs to an earlier bot membership. Check Discord for its outcome. Clear the saved request without submitting another action?",
      )
    ) {
      window.sessionStorage.removeItem(key);
    }
    throw new Error(
      "Saved request was not submitted. Review the current server before making a new request.",
    );
  }
  const requestId = saved.request_id;
  let job;
  try {
    job = await api(path, method, { ...body, request_id: requestId });
  } catch (error) {
    if (
      ((error.status === 410 && error.code === "action_request_expired") ||
        (error.status === 400 && error.code === "action_request_invalid") ||
        (error.status === 409 && error.code === "action_enrollment_changed")) &&
      confirm(
        "The earlier action may already have run, and its result may no longer be listed. Have you checked Bot actions and Discord, and want to clear this saved request? This will not submit another action.",
      )
    ) {
      window.sessionStorage.removeItem(key);
      throw new Error(
        "Saved request cleared. Review the current state, then submit again only if another action is needed.",
      );
    }
    throw error;
  }
  if (
    !job ||
    typeof job.id !== "string" ||
    job.guild_id !== guildId ||
    job.actor_id !== state.me.user.id ||
    job.enrollment_id !== enrollmentId ||
    !["pending", "claimed", "running", "succeeded", "failed", "uncertain", "cancelled", "expired"].includes(
      job.state,
    )
  ) {
    throw new Error(
      "Action acknowledgment was invalid. Check Bot actions; retrying here will reuse the same request ID.",
    );
  }
  try {
    window.sessionStorage.removeItem(key);
  } catch (_) {
    /* Retaining the same ID is safe. */
  }
  const completed = job.state === "succeeded";
  const waiting = ["pending", "claimed", "running"].includes(job.state);
  toast(
    completed
      ? "Bot action completed. See Bot actions for details."
      : waiting
        ? "Action queued. Check Bot actions for the result."
        : `Action ${job.state}: ${job.outcome || "Check Bot actions for details."}`,
    completed ? "success" : waiting ? "info" : "error",
  );
  return job;
}

function logChannelRow(event) {
  return state.config?.log_channels?.find((row) => row.event === event);
}

function logChannelActionKey(job) {
  return `${job.guild_id}:${job.enrollment_id}:${job.action.event}`;
}

function clearPendingLogChannelEntry(key) {
  const entry = pendingLogChannelActions.get(key);
  if (entry?.timer !== null) clearTimeout(entry.timer);
  pendingLogChannelActions.delete(key);
}

function overlayPendingLogChannels(config, guildId) {
  if (!config?.enrollment_id) return [];
  const overlaid = [];
  for (const [key, entry] of pendingLogChannelActions) {
    const job = entry.job;
    if (job.guild_id !== guildId || job.enrollment_id !== config.enrollment_id) continue;
    const row = config.log_channels?.find((candidate) => candidate.event === job.action.event);
    if (!row) continue;
    row.effective = entry.requested;
    row.guild_override = entry.requested;
    row.pending_action = { id: job.id, state: job.state, previous: entry.previous };
    overlaid.push({ key, entry });
  }
  return overlaid;
}

function renderLogChannelsPreservingDrafts() {
  if (state.tab !== "logchannels") return;
  const drafts = committedRevision === contentRevision ? currentDrafts() : [];
  if (drafts.length) {
    pendingDrafts = {
      guildId: state.guildId,
      tab: state.tab,
      drafts,
      enrollmentId: state.config?.enrollment_id,
    };
  }
  renderTab();
}

function applyQueuedLogChannel(job) {
  if (
    !actionIsLogChannel(job) ||
    job.guild_id !== state.guildId ||
    job.enrollment_id !== state.config?.enrollment_id
  )
    return false;
  const row = logChannelRow(job.action.event);
  if (!row) return false;
  const active = ["pending", "claimed", "running"].includes(job.state);
  const terminal = ["succeeded", "failed", "uncertain", "cancelled", "expired"].includes(job.state);
  if (!active && !terminal) return false;
  const key = logChannelActionKey(job);
  const existing = pendingLogChannelActions.get(key);
  if (
    existing &&
    existing.job.id !== job.id &&
    terminal &&
    ["pending", "claimed", "running"].includes(existing.job.state)
  ) {
    // A retained request key can replay an older terminal receipt after a
    // newer request has become this row's active owner. Keep watching newer.
    return true;
  }
  if (existing && existing.job.id !== job.id && existing.timer !== null) clearTimeout(existing.timer);
  const previous = existing?.previous ||
    row.pending_action?.previous || {
      effective: row.effective ?? null,
      guild_override: row.guild_override ?? null,
    };
  const requested = job.action.channel_id ?? null;
  row.effective = requested;
  row.guild_override = requested;
  if (job.state === "succeeded") {
    clearPendingLogChannelEntry(key);
    row.pending_action = null;
  } else {
    const entry = {
      job,
      requested,
      previous,
      attempt: existing?.job.id === job.id ? existing.attempt : 0,
      timer: existing?.job.id === job.id ? existing.timer : null,
    };
    pendingLogChannelActions.set(key, entry);
    row.pending_action = { id: job.id, state: job.state, previous };
  }
  // A snapshot started before this acknowledged request must not replace its
  // queued representation after the button has already returned success.
  ++configRequest;
  state.loading = false;
  state.error = null;
  return true;
}

function scheduleLogChannelReconcile(job, attempt = 0) {
  const key = logChannelActionKey(job);
  const entry = pendingLogChannelActions.get(key);
  if (!entry || entry.job.id !== job.id || entry.timer !== null) return;
  if (attempt >= actionReconcileDelays.length) {
    toast(
      "Log-channel validation is still pending. Its requested value remains marked queued; see Bot actions for details.",
      "info",
    );
    return;
  }
  entry.attempt = attempt;
  entry.timer = setTimeout(() => {
    const current = pendingLogChannelActions.get(key);
    if (!current || current.job.id !== job.id) return;
    current.timer = null;
    reconcileQueuedLogChannel(job, attempt).catch((error) => {
      if (!isUnauthenticated(error)) {
        if (attempt + 1 < actionReconcileDelays.length) scheduleLogChannelReconcile(job, attempt + 1);
        else toast("Could not confirm the log-channel result. See Bot actions for details.", "error");
      }
    });
  }, actionReconcileDelays[attempt]);
}

async function reconcileQueuedLogChannel(original, attempt = 0) {
  const key = logChannelActionKey(original);
  let entry = pendingLogChannelActions.get(key);
  let row = logChannelRow(original.action.event);
  if (
    !entry ||
    entry.job.id !== original.id ||
    !row?.pending_action ||
    row.pending_action.id !== original.id ||
    original.guild_id !== state.guildId ||
    original.enrollment_id !== state.config?.enrollment_id
  )
    return;
  const originalTerminal = ["succeeded", "failed", "uncertain", "cancelled", "expired"].includes(
    original.state,
  );
  let job = original;
  if (!originalTerminal) {
    const jobs = await apiRead(`/api/guilds/${original.guild_id}/actions`);
    job = jobs.find((candidate) => candidate.id === original.id);
    entry = pendingLogChannelActions.get(key);
    row = logChannelRow(original.action.event);
    if (
      !entry ||
      entry.job.id !== original.id ||
      !row?.pending_action ||
      row.pending_action.id !== original.id ||
      original.guild_id !== state.guildId ||
      original.enrollment_id !== state.config?.enrollment_id
    )
      return;
  }
  if (!job || ["pending", "claimed", "running"].includes(job.state)) {
    if (job) {
      entry.job = job;
      row.pending_action.state = job.state;
    }
    scheduleLogChannelReconcile(original, attempt + 1);
    return;
  }
  // A newer request for this event owns the row now; never let an older
  // terminal receipt overwrite or roll it back.
  if (row.pending_action?.id !== original.id) return;
  entry.job = job;
  row.pending_action.state = job.state;
  if (job.state === "succeeded") {
    clearPendingLogChannelEntry(key);
    row.pending_action = null;
    renderLogChannelsPreservingDrafts();
    toast("Log channel saved", "success");
    return;
  }

  let authoritative;
  try {
    authoritative = await apiRead(
      `/api/guilds/${original.guild_id}/logchannels/${encodeURIComponent(original.action.event)}`,
    );
  } catch (error) {
    if (isUnauthenticated(error)) return;
    renderLogChannelsPreservingDrafts();
    scheduleLogChannelReconcile(job, attempt + 1);
    return;
  }
  entry = pendingLogChannelActions.get(key);
  row = logChannelRow(original.action.event);
  if (
    !entry ||
    entry.job.id !== original.id ||
    !row?.pending_action ||
    original.guild_id !== state.guildId ||
    original.enrollment_id !== state.config?.enrollment_id
  )
    return;
  if (row.pending_action?.id !== original.id) return;
  row.effective = authoritative.effective;
  row.guild_override = authoritative.guild_override;
  clearPendingLogChannelEntry(key);
  row.pending_action = null;
  renderLogChannelsPreservingDrafts();
  toast(`Log-channel action ${job.state}: ${job.outcome || "the saved mapping was not changed"}`, "error");
}

async function waitForApiMe() {
  let warmupStarted = null;
  let reachabilityStarted = null;
  let delay = 500;
  let sawStartup = false;
  while (true) {
    const attemptStarted = Date.now();
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), ORIGIN_REACHABILITY_BUDGET_MS);
    try {
      const me = await apiRead("/api/me", controller.signal);
      return me;
    } catch (e) {
      if (!isOriginStarting(e)) throw e;
      const now = Date.now();
      if (isApiWarmup(e)) {
        reachabilityStarted = null;
        if (warmupStarted === null) warmupStarted = attemptStarted;
        if (now - warmupStarted >= API_WARMUP_BUDGET_MS) {
          throw apiError(originGaveUpMessage(e), e.status || 0, 0);
        }
      } else {
        if (reachabilityStarted === null) reachabilityStarted = attemptStarted;
        if (now - reachabilityStarted >= ORIGIN_REACHABILITY_BUDGET_MS) {
          throw apiError(originGaveUpMessage(e), e.status || 0, 0);
        }
      }
      if (!sawStartup) {
        sawStartup = true;
        showView("startup-view");
        setStartupStatus("Waiting for the API to finish starting…");
      }
      await sleep(Math.max(e.retryAfterMs || 0, delay));
      delay = Math.min(Math.floor(delay * 1.5), 2000);
    } finally {
      clearTimeout(timer);
    }
  }
}

async function loadConfig() {
  const guildId = state.guildId;
  const request = ++configRequest;
  if (!guildId) {
    renderTab();
    return;
  }
  state.loading = true;
  state.error = null;
  if (!state.config) renderTab();
  try {
    const config = await apiRead(`/api/guilds/${guildId}/config`);
    if (request !== configRequest || guildId !== state.guildId) return;
    if (state.config && state.config.enrollment_id !== config.enrollment_id) {
      pendingDrafts = null;
      dirty = false;
      for (const [key, entry] of pendingLogChannelActions) {
        if (entry.job.guild_id === guildId && entry.job.enrollment_id !== config.enrollment_id) {
          clearPendingLogChannelEntry(key);
        }
      }
      toast("Bot membership changed. Configuration reloaded; review it before making changes.", "info");
    }
    state.config = config;
    for (const { entry } of overlayPendingLogChannels(config, guildId)) {
      if (entry.timer === null) scheduleLogChannelReconcile(entry.job, entry.attempt);
    }
    return true;
  } catch (e) {
    if (request !== configRequest || guildId !== state.guildId) return;
    if (isUnauthenticated(e)) return;
    state.config = null;
    editor = null;
    state.error = e.message || String(e);
    toast("Could not refresh configuration: " + state.error, "error");
  } finally {
    if (request === configRequest) {
      state.loading = false;
      if (state.error) renderTab();
    }
  }
}

function selectedGuild() {
  return state.guilds.find((g) => g.id === state.guildId);
}

function applyConfigPatch(res) {
  const patch = res?.config_patch;
  const config = state.config;
  if (!patch || !config) return false;
  // Any snapshot requested before this acknowledged commit is now stale.
  ++configRequest;
  state.loading = false;
  state.error = null;
  if (patch.kind === "rule") {
    const row = config.rules?.find((rule) => rule.rule_id === patch.id);
    if (!row || (patch.value == null && (patch.scope === "global" || row.global == null))) return false;
    if (patch.scope === "global") {
      row.global = patch.value;
      if (!row.guild_override) row.effective = patch.value;
    } else {
      row.guild_override = patch.value;
      row.effective = patch.value ?? row.global;
    }
    return true;
  }
  if (patch.kind === "badword") {
    if (!Array.isArray(config.badwords)) return false;
    if (patch.operation === "add" && patch.value) config.badwords.push(patch.value);
    else if (patch.operation === "delete")
      config.badwords = config.badwords.filter((word) => word.id !== patch.id);
    else return false;
    return true;
  }
  if (patch.kind === "command" && patch.value) {
    config.commands ||= [];
    let row = config.commands.find((command) => command.name === patch.name);
    if (!row) {
      row = {
        name: patch.name,
        scope: patch.scope,
        effective: patch.value,
        guild_override: null,
        global: null,
      };
      config.commands.push(row);
    }
    if (patch.scope === "global") {
      row.global = patch.value;
      if (!row.guild_override) {
        row.scope = "global";
        row.effective = patch.value;
      }
    } else {
      row.scope = "guild";
      row.guild_override = patch.value;
      row.effective = patch.value;
    }
    return true;
  }
  if (patch.kind === "template") {
    const row = config.templates?.find((template) => template.event === patch.event);
    if (!row || (patch.value == null && (patch.scope === "global" || row.global == null))) return false;
    if (patch.scope === "global") {
      row.global = patch.value;
      if (row.guild_override == null) row.effective = patch.value;
    } else {
      row.guild_override = patch.value;
      row.effective = patch.value ?? row.global;
    }
    return true;
  }
  if (patch.kind === "settings" && !patch.sync_nickname && patch.settings) {
    if (patch.bot_name_present && !patch.nickname_reconcile) return false;
    const fields = [
      ["prefix", patch.prefix_present],
      ["bot_name", patch.bot_name_present],
    ];
    if (patch.scope === "global") {
      config.global_settings = patch.settings;
      config.global_settings_override ||= {};
      for (const [field, present] of fields) {
        if (!present) continue;
        config.global_settings_override[field] = patch[field] ?? null;
        if (config.settings_override?.[field] == null) config.settings[field] = patch.settings[field];
      }
    } else {
      config.settings = patch.settings;
      config.settings_override ||= {};
      for (const [field, present] of fields) {
        if (!present) continue;
        const parent = config.global_settings?.[field];
        config.settings_override[field] =
          patch[field] == null || patch.settings[field] === parent ? null : patch.settings[field];
      }
      if (patch.bot_name_present) config.nickname_reconcile = patch.nickname_reconcile;
    }
    return true;
  }
  return false;
}

async function reloadAfter(work, onSuccess) {
  const res = await work();
  if (!applyConfigPatch(res) && !(await loadConfig())) return;
  if (onSuccess) onSuccess(res);
  renderTab();
}

function emptyPanel(title, body, action, attrs) {
  return h(
    "div",
    { className: "hero bg-base-200 rounded-box", ...(attrs || {}) },
    h(
      "div",
      { className: "hero-content text-center" },
      h("div", { className: "hero-copy" }, h("h2", {}, title), h("p", {}, body), action || null),
    ),
  );
}

function field(label, control) {
  control = React.cloneElement(control, { "aria-label": label });
  return h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, label), control);
}

function eventHeading(event) {
  return h(
    "div",
    { className: "event-heading" },
    h(
      "h3",
      { className: "card-title" },
      event.replaceAll("_", " ").replace(/^./, (c) => c.toUpperCase()),
    ),
    h("code", { className: "muted" }, event),
  );
}

function statsRow(...items) {
  return h("div", { className: "stats bg-base-100 w-full" }, ...items);
}

function tabDashboard() {
  const guildId = useRef(state.guildId).current;
  const g = selectedGuild();
  const cfg = state.config;
  const rulesOn = cfg.rules.filter((r) => r.effective.enabled).length;
  const rulesDry = cfg.rules.filter((r) => r.effective.dry_run).length;
  const commandsOn = cfg.commands.filter((c) => c.effective.enabled).length;
  const chans = cfg.log_channels.filter((l) => l.effective && String(l.effective) !== "0").length;
  const result = useApiRead(`/api/guilds/${guildId}/stats`);
  const moderation = [
    ["events", "Moderation records"],
    ["warns", "Warns"],
    ["bans", "Bans"],
    ["cases", "Cases"],
  ].map(([key, label]) => ({
    key,
    value: h(
      "div",
      { className: "stat-value" },
      result.loading ? "Loading…" : result.data?.[key] == null ? "Unavailable" : String(result.data[key]),
    ),
    label,
  }));
  return h(
    "div",
    { className: "contents" },
    h(
      "section",
      { className: "hero dashboard-hero" },
      h(
        "div",
        { className: "hero-content" },
        h(
          "div",
          { className: "hero-copy" },
          h("span", { className: "eyebrow" }, "COMMUNITY AT A GLANCE"),
          h("h2", {}, g ? `${g.name}, under your care.` : "Your moderation workspace"),
          h(
            "p",
            {},
            "Review your server’s protection, configure your tools, and keep track of moderation activity.",
          ),
          h(
            "div",
            { className: "row" },
            h(
              ActionButton,
              { className: "btn btn-primary", onClick: () => navigate("rules") },
              "Review protection →",
            ),
            h(
              ActionButton,
              { className: "btn btn-outline", onClick: () => navigate("events") },
              "View audit log",
            ),
          ),
        ),
      ),
    ),
    h(
      "section",
      { className: "card bg-base-100 card-border getting-started" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Getting started"),
        h("p", { className: "muted" }, "Set up protection, choose your tools, then decide where reports go."),
        h(
          "div",
          { className: "setup-links" },
          ...[
            ["01", "Tune automod", "Review rules and test them in dry-run.", "rules"],
            ["02", "Configure commands", "Choose commands and moderator permissions.", "commands"],
            ["03", "Route your logs", "Connect event reports to Discord channels.", "logchannels"],
          ].map(([number, title, description, tab]) =>
            h(
              ActionButton,
              { className: "btn setup-link", onClick: () => navigate(tab) },
              h("span", { className: "setup-number" }, number),
              h("span", {}, h("strong", {}, title), h("span", {}, description)),
              h("span", { "aria-hidden": "true" }, "→"),
            ),
          ),
        ),
        h(
          "p",
          { className: "setup-note" },
          "Use each editor’s Save action to apply changes. The bot usually picks up configuration changes within a few seconds. " +
            (isBotOwner()
              ? "As bot owner, you can also edit Global defaults for all servers."
              : "Global defaults can only be edited by the bot owner."),
        ),
      ),
    ),
    h(
      "section",
      { className: "dashboard-section" },
      h("h3", { className: "card-title" }, "Protection & tools"),
      statsRow(
        stat("Rules enabled", `${rulesOn} / ${cfg.rules.length}`),
        stat("Rules in dry-run", rulesDry),
        stat("Commands enabled", `${commandsOn} / ${cfg.commands.length}`),
        stat("Log channels", chans),
      ),
    ),
    h(
      "section",
      { className: "dashboard-section" },
      h("h3", { className: "card-title" }, "Moderation activity"),
      statsRow(
        ...moderation.map(({ value, label }) =>
          h("div", { className: "stat" }, h("div", { className: "stat-title" }, label), value),
        ),
      ),
      h(
        "aside",
        { className: "dashboard-note" },
        h("strong", {}, "About these counts"),
        h(
          "p",
          {},
          "Moderation records exclude routine history and older plan summaries whose action type was not recorded. Those summaries remain in the Audit Log.",
        ),
      ),
    ),
    h(
      "section",
      { className: "dashboard-section" },
      h("h3", { className: "card-title" }, "Server configuration"),
      statsRow(
        stat("Prefix", cfg.settings.prefix),
        stat("Bot name", cfg.settings.bot_name || "default"),
        stat("Badword patterns", cfg.badwords.length),
        stat("Routine chat collection", "Off"),
      ),
    ),
  );
}

function stat(label, value) {
  return h(
    "div",
    { className: "stat" },
    h("div", { className: "stat-title" }, label),
    h("div", { className: "stat-value" }, value),
  );
}

function tabRules() {
  const rules = state.config.rules;
  return h(
    "div",
    {},
    h("h2", {}, "Automod rules"),
    h(
      "p",
      { className: "muted", style: { marginBottom: "14px" } },
      "Each rule has its own parameters, whitelists, and an escalation table " +
        "(`at` = strike count at which the action fires; the highest-severity fired action is executed). " +
        "Exact image and repeated-text rules use separate matchers: dry-run logs matches only, " +
        "while normal mode deletes matched messages and bans. Use dry-run while tuning.",
    ),
    ...rules.map((r) => ruleCard(r)),
  );
}

function ruleCard(r) {
  const rule = r.effective;
  const spam = spamRule(r.rule_id);
  const supported = !spam || spamConfigured(r.rule_id, rule);
  const badge = !rule.enabled || !supported ? "off" : rule.dry_run ? "dry" : "on";
  const label = !rule.enabled
    ? "disabled"
    : !supported
      ? "inactive — convert this rule"
      : rule.dry_run
        ? "dry-run"
        : spam
          ? "delete and ban"
          : "enabled";
  return h(
    "div",
    { className: "card bg-base-100 card-border" },
    h(
      "div",
      { className: "card-body" },
      h(
        "div",
        { className: "row", style: { justifyContent: "space-between" } },
        h(
          "div",
          { className: "rule-heading" },
          h("h3", { className: "card-title" }, r.name),
          h(
            "div",
            { className: "tag-group", "aria-label": "Rule status and source" },
            h(
              "span",
              {
                className: `badge badge-soft badge-sm ${{ off: "badge-error", dry: "badge-warning", on: "badge-success" }[badge]}`,
              },
              label,
            ),
            r.guild_override
              ? h("span", { className: "badge badge-soft badge-sm badge-neutral override-tag" }, "server override")
              : null,
            r.global ? h("span", { className: "badge badge-soft badge-sm badge-primary scope-tag" }, "global") : null,
          ),
        ),
        h(
          "div",
          { className: "card-actions" },
          h(ActionButton, { className: "btn btn-sm", onClick: () => editRule(r) }, "Edit"),
        ),
      ),
      h("p", {}, r.description),
    ),
  );
}

function spamRule(id) {
  return id === "attachment_flood" || id === "repeat_text";
}

function imageTestConfigured(rule) {
  return (
    spamConfigured(
      "attachment_flood",
      Object.assign({}, rule, {
        dry_run: true,
        params: Object.assign({ mode: "exact_image_test" }, rule.params || {}),
        actions: [{ action: "log" }],
      }),
    ) ||
    ((rule.params || {}).mode === "exact_image_test" &&
      rule.dry_run === true &&
      (rule.actions || []).every((a) => a.action === "log") &&
      spamThresholds(rule.params, "attachment_flood"))
  );
}

function spamConfigured(id, rule) {
  const p = rule.params || {};
  if (id === "attachment_flood") {
    const test =
      (p.mode === "exact_image_test" || (p.mode === "exact_image" && rule.dry_run === true)) &&
      (rule.actions || []).every((a) => a.action === "log") &&
      rule.dry_run === true;
    const enforce =
      p.mode === "exact_image" &&
      rule.dry_run === false &&
      (rule.actions || []).some((a) => a.action === "delete") &&
      (rule.actions || []).some((a) => a.action === "ban") &&
      (rule.actions || []).every((a) => ["delete", "ban", "log"].includes(a.action));
    return (test || enforce) && spamThresholds(p, id);
  }
  if (id === "repeat_text") {
    const test =
      p.mode === "exact_text" &&
      rule.dry_run === true &&
      (rule.actions || []).every((a) => a.action === "log");
    const enforce =
      p.mode === "exact_text" &&
      rule.dry_run === false &&
      (rule.actions || []).some((a) => a.action === "delete") &&
      (rule.actions || []).some((a) => a.action === "ban") &&
      (rule.actions || []).every((a) => ["delete", "ban", "log"].includes(a.action));
    return (test || enforce) && spamThresholds(p, id);
  }
  return false;
}

function spamThresholds(p, id) {
  const keys =
    id === "repeat_text"
      ? ["mode", "window_secs", "min_channels", "min_length"]
      : ["mode", "window_secs", "min_channels", "min_attachments"];
  if (!Object.keys(p).every((k) => keys.includes(k))) return false;
  const ranges =
    id === "repeat_text"
      ? [
          [p.window_secs, 1, 600],
          [p.min_channels, 2, 10],
          [p.min_length, 1, 2000],
        ]
      : [
          [p.window_secs, 1, 600],
          [p.min_channels, 2, 10],
          [p.min_attachments, 1, 10],
        ];
  return ranges.every(([n, min, max]) => Number.isInteger(n) && n >= min && n <= max);
}

function RuleEditor({ record: r }) {
  const [rule] = useState(() => JSON.parse(JSON.stringify(r.effective)));
  const [, setActionsRevision] = useState(0);
  const renderActions = () => setActionsRevision((value) => value + 1);
  const spam = spamRule(r.rule_id);
  const textRule = r.rule_id === "repeat_text";
  const image = r.rule_id === "attachment_flood";
  const imageChannelsRef = useRef(null);
  const imageChannels = h(Input, {
    ref: imageChannelsRef,
    type: "number",
    min: 2,
    max: 10,
    defaultValue: rule.params?.min_channels ?? 3,
    className: "input",
  });
  const imageCountRef = useRef(null);
  const imageCount = h(Input, {
    ref: imageCountRef,
    type: "number",
    min: 1,
    max: 10,
    defaultValue: rule.params?.min_attachments ?? 2,
    className: "input",
  });
  const imageWindowRef = useRef(null);
  const imageWindow = h(Input, {
    ref: imageWindowRef,
    type: "number",
    min: 1,
    max: 600,
    defaultValue: rule.params?.window_secs ?? 60,
    className: "input",
  });
  const textLengthRef = useRef(null);
  const textLength = h(Input, {
    ref: textLengthRef,
    type: "number",
    min: 1,
    max: 2000,
    defaultValue: rule.params?.min_length ?? 20,
    className: "input",
  });
  // Inactive legacy image rows should convert to the safe dry-run mode by default.
  const dryRunRef = useRef(null);
  const dryRun = h(Input, {
    ref: dryRunRef,
    type: "checkbox",
    defaultChecked: spam ? !spamConfigured(r.rule_id, rule) || rule.dry_run : rule.dry_run,
    onChange: (e) => (rule.dry_run = e.target.checked),
    className: "checkbox",
  });
  const paramsTextRef = useRef(null);
  const paramsText = h(Textarea, {
    ref: paramsTextRef,
    className: "mono" + " textarea",
    rows: 7,
    spellCheck: false,
    defaultValue: JSON.stringify(rule.params ?? {}, null, 2),
  });

  const whitelistTextRef = useRef(null);
  const whitelistText = h(Input, {
    ref: whitelistTextRef,
    type: "text",
    defaultValue: (rule.whitelist_roles || []).join(","),
    className: "input",
  });
  const channelsTextRef = useRef(null);
  const channelsText = h(Input, {
    ref: channelsTextRef,
    type: "text",
    defaultValue: (rule.whitelist_channels || []).join(","),
    className: "input",
  });
  const usersTextRef = useRef(null);
  const usersText = h(Input, {
    ref: usersTextRef,
    type: "text",
    defaultValue: (rule.whitelist_users || []).join(","),
    className: "input",
  });
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef);

  const actionsRows = h(
    "div",
    null,
    ...rule.actions.map((action, i) =>
      h(RuleAction, {
        key: itemKey(action),
        action,
        index: i,
        remove: () => {
          rule.actions.splice(i, 1);
          markDirty();
          renderActions();
        },
      }),
    ),
  );

  const box = h(
    DialogPanel,
    { className: "modal-box rule-editor", onInput: trackDraft },
    h(
      "div",
      { className: "editor-heading" },
      h("span", { className: "eyebrow" }, "AUTOMOD RULE"),
      h("h3", { className: "card-title" }, `Edit rule: ${r.name}`),
      h(
        "p",
        { className: "muted" },
        r.description || "Configure detection, actions and exemptions for this rule.",
      ),
    ),
    h(
      "div",
      { className: "row checkbox-group" },
      h(
        "label",
        { className: "checkbox-label" },
        h(Input, {
          type: "checkbox",
          defaultChecked: rule.enabled,
          onChange: (e) => (rule.enabled = e.target.checked),
          className: "checkbox",
        }),
        "Enabled",
      ),
      h("label", { className: "checkbox-label" }, dryRun, "Dry-run (log only)"),
    ),
    spam
      ? h(
          "div",
          {},
          h(
            "p",
            {},
            textRule
              ? "Exact text after trimming ends only. No case folding. Empty text never matches. Default minimum length is 20 Unicode scalars."
              : "Exact common image bytes across channels. Extra images are allowed. Creates and relevant attachment edits participate. Work limits can reduce coverage.",
          ),
          h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Distinct channels"), imageChannels),
          image
            ? h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Common distinct images per message"), imageCount)
            : null,
          textRule
            ? h(
                Field,
                { className: "fieldset field" },
                h(Label, { className: "fieldset-legend" }, "Minimum length (Unicode scalars after trim)"),
                textLength,
              )
            : null,
          h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Window (seconds)"), imageWindow),
        )
      : h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Parameters (JSON)"), paramsText),
    spam
      ? null
      : h(
          Field,
          { className: "fieldset field" },
          h(Label, { className: "fieldset-legend" }, "Action escalation"),
          h(
            "p",
            { className: "field-help" },
            "Strike count is the threshold for an action. Timeout minutes apply to timeout; delete-history days apply to ban.",
          ),
          actionsRows,
          h(
            ActionButton,
            {
              className: "btn btn-sm",
              onClick: () => {
                rule.actions.push({
                  at: rule.actions.length + 1,
                  action: "warn",
                  duration_minutes: 0,
                  delete_days: 0,
                  reason: "",
                });
                markDirty();
                renderActions();
              },
            },
            "+ add action",
          ),
        ),
    h(
      "div",
      { className: "row form-row" },
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Whitelist roles (ids, comma)"), whitelistText),
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Whitelist channels (ids)"), channelsText),
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Whitelist users (ids)"), usersText),
    ),
    h(
      "div",
      { className: "modal-action" },
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Applies to"), scopeSel),
      h(
        ActionButton,
        {
          className: "btn btn-primary",
          onClick: async () => {
            try {
              if (spam) {
                const punish = !dryRunRef.current.checked;
                if (textRule) {
                  rule.params = {
                    mode: "exact_text",
                    min_channels: Number(imageChannelsRef.current.value),
                    window_secs: Number(imageWindowRef.current.value),
                    min_length: Number(textLengthRef.current.value),
                  };
                } else {
                  rule.params = {
                    mode: punish ? "exact_image" : "exact_image_test",
                    min_channels: Number(imageChannelsRef.current.value),
                    min_attachments: Number(imageCountRef.current.value),
                    window_secs: Number(imageWindowRef.current.value),
                  };
                }
                rule.dry_run = !punish;
                rule.actions = punish
                  ? [
                      { at: 1, action: "delete", reason: "spam match", duration_minutes: 0, delete_days: 0 },
                      { at: 1, action: "ban", reason: "spam match", duration_minutes: 0, delete_days: 0 },
                    ]
                  : [
                      {
                        at: 1,
                        action: "log",
                        reason: textRule ? "Repeated text test" : "Exact image spam test",
                        duration_minutes: 0,
                        delete_days: 0,
                      },
                    ];
                if (!spamConfigured(r.rule_id, rule))
                  throw new Error(
                    "Channels must be 2–10 and window 1–600 seconds, all whole numbers. Images 1–10. Text length 1–2000.",
                  );
              } else {
                rule.params = JSON.parse(paramsTextRef.current.value || "{}");
              }
            } catch (e) {
              toast("Invalid params JSON: " + e.message, "error");
              return;
            }
            rule.whitelist_roles = splitIds(whitelistTextRef.current.value);
            rule.whitelist_channels = splitIds(channelsTextRef.current.value);
            rule.whitelist_users = splitIds(usersTextRef.current.value);
            const scope = scopeSelRef.current.value;
            const saved = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", {
              scope,
              data: rule,
            });
            const savedLocally = applyConfigPatch(saved);
            if (scope === "global") {
              try {
                const cleared = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", {
                  scope: "guild",
                });
                applyConfigPatch(cleared);
              } catch (e) {
                // The global write already committed. A failed second request must
                // not leave the pre-save snapshot visible as current configuration.
                const refreshed = await loadConfig();
                if (refreshed) {
                  closeOverlay();
                  renderTab();
                }
                toast(
                  `Global rule saved, but clearing this server's override could not be confirmed: ${e.message || String(e)}` +
                    (refreshed
                      ? ""
                      : " Configuration refresh also failed. Use Retry to load the saved state."),
                  "error",
                );
                return;
              }
            }
            if (!savedLocally && !(await loadConfig())) return;
            toast("Rule saved", "success");
            closeOverlay();
            renderTab();
          },
        },
        "Save",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-error",
          onClick: async () => {
            await reloadAfter(
              () => api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" }),
              () => {
                toast("Rule reset", "success");
                closeOverlay();
              },
            );
          },
        },
        "Reset to global",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-outline",
          onClick: () => {
            if (canLeave()) closeOverlay();
          },
        },
        "Cancel",
      ),
    ),
  );
  return box;
}

function parseSnowflake(raw) {
  const text = String(raw ?? "").trim();
  if (!text) return "";
  const mention = text.match(/^<(?:#|@!?|@&)(\d+)>$/);
  if (mention) return mention[1];
  const url = text.match(/discord(?:app)?\.com\/channels\/\d+\/(\d+)/i);
  if (url) return url[1];
  return /^\d+$/.test(text) ? text : "";
}

function parseLogFilterId(raw, kind) {
  const value = String(raw ?? "").trim();
  const match = kind === "user" ? value.match(/^<@!?(\d+)>$/) : value.match(/^<#(\d+)>$/);
  const url =
    kind === "channel"
      ? value.match(/^https?:\/\/(?:canary\.|ptb\.)?discord(?:app)?\.com\/channels\/\d+\/(\d+)\/?$/i)
      : null;
  const id = match?.[1] || url?.[1] || (/^\d+$/.test(value) ? value : "");
  return id && /[1-9]/.test(id) ? id : "";
}

function splitIds(s) {
  const ids = [];
  for (const raw of String(s ?? "").split(",")) {
    const token = raw.trim();
    if (!token) continue;
    const id = parseSnowflake(token);
    if (!id) throw new Error("Enter Discord IDs as digits, mentions, or channel URLs");
    ids.push(id);
  }
  return ids;
}

function applyPlanStepInput(step, type, raw) {
  if (type === "reply" || type === "dm" || type === "log") step.template = raw;
  else if (type === "add_role" || type === "remove_role") step.role_id = parseSnowflake(raw);
  else if (type === "timeout") step.minutes = Number(raw) || 1;
  else if (type === "ban") step.delete_days = Math.min(7, Math.max(0, Number(raw) || 0));
  else if (type === "react") step.emoji = raw;
  else if (type === "wait") step.secs = Math.max(1, Number(raw) || 1);
}

function tabBadwords() {
  const words = state.config.badwords;
  const patternRef = useRef(null);
  const pattern = h(Input, {
    ref: patternRef,
    type: "text",
    placeholder: "regex pattern, e.g. (scam|pump and dump)",
    style: { flex: "2" },
    className: "input",
  });
  const noteRef = useRef(null);
  const note = h(Input, {
    ref: noteRef,
    type: "text",
    placeholder: "note (optional)",
    style: { flex: "1" },
    className: "input",
  });
  const scopeRef = useRef(null);
  const scope = scopeSelect(scopeRef, "This server", "Global");
  const sampleRef = useRef(null);
  const sample = h(Input, {
    ref: sampleRef,
    type: "text",
    placeholder: "sample text to test, e.g. 5c@m 🅿🆁🅾🅼🅾",
    style: { flex: "2" },
    className: "input",
  });
  const [result, setResult] = useState("");

  return h(
    "div",
    {},
    h("h2", {}, "Badwords (bypass-resistant)"),
    h(
      "p",
      { className: "muted", style: { marginBottom: "12px" } },
      "Patterns are matched against both the raw message and a normalized form " +
        "(lowercase, unicode-decomposed, zero-width chars stripped, leetspeak mapped: 4/@→a, 3→e, 0→o, 1→i, 5→s, 7→t, $→s…). " +
        (isBotOwner()
          ? "Global patterns apply to every server."
          : "Global patterns (shown below) are enforced for all servers; only the bot owner can edit them."),
    ),
    h(
      "div",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h(
          "div",
          { className: "row" },
          pattern,
          note,
          scope,
          h(
            ActionButton,
            {
              className: "btn btn-primary",
              onClick: async () => {
                if (!patternRef.current.value.trim()) return toast("Enter a pattern", "error");
                await reloadAfter(
                  async () => {
                    const res = await api(`/api/guilds/${state.guildId}/badwords`, "POST", {
                      scope: scopeRef.current.value,
                      pattern: patternRef.current.value.trim(),
                      note: noteRef.current.value || null,
                    });
                    patternRef.current.value = "";
                    noteRef.current.value = "";
                    return res;
                  },
                  () => toast("Badword added", "success"),
                );
              },
            },
            "Add",
          ),
        ),
      ),
    ),
    h(
      "div",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Live tester"),
        h(
          "div",
          { className: "row" },
          h(Input, {
            type: "text",
            id: "test-pattern",
            placeholder: "regex",
            style: { flex: "1" },
            className: "input",
          }),
          sample,
          h(
            ActionButton,
            {
              className: "btn",
              onClick: async () => {
                const p = $("#test-pattern").value;
                if (!p || !sampleRef.current.value) return toast("Enter pattern and sample", "error");
                const res = await api(`/api/guilds/${state.guildId}/testregex`, "POST", {
                  pattern: p,
                  sample: sampleRef.current.value,
                });
                setResult(
                  `raw match: ${res.raw_match}\nnormalized match: ${res.normalized_match}\nnormalized: ${res.normalized_sample}`,
                );
              },
            },
            "Test",
          ),
        ),
        h("pre", { className: "code" }, result),
      ),
    ),
    h(
      "div",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, `Patterns (${words.length})`),
        words.length === 0
          ? h("p", { className: "muted" }, "No patterns yet.")
          : h(
              "div",
              {
                className: "table-scroll rounded-box bg-base-100",
                tabIndex: 0,
                role: "region",
                "aria-label": "Scrollable data table",
              },
              h(
                "table",
                { className: "table table-zebra" },
                h(
                  "thead",
                  {},
                  h(
                    "tr",
                    {},
                    h("th", {}, "Scope"),
                    h("th", {}, "Pattern"),
                    h("th", {}, "Note"),
                    h("th", {}, "Status"),
                    h("th", {}),
                  ),
                ),
                h(
                  "tbody",
                  {},
                  ...words.map((w) => {
                    const isGlobal = String(w.guild_id) === "0";
                    const canDelete = !isGlobal || isBotOwner();
                    const invalid = w.valid === false;
                    return h(
                      "tr",
                      {},
                      h("td", {}, isGlobal ? "global" : "server"),
                      h("td", { className: "mono" }, w.pattern),
                      h("td", { className: "muted" }, w.note || ""),
                      h(
                        "td",
                        {},
                        invalid
                          ? h(
                              "span",
                              {
                                className: "badge badge-soft badge-sm badge-error",
                                title: w.error || "failed compile limits",
                              },
                              "not enforced",
                            )
                          : h("span", { className: "badge badge-soft badge-sm badge-success" }, "active"),
                      ),
                      h(
                        "td",
                        {},
                        canDelete
                          ? h(
                              ActionButton,
                              {
                                className: "btn btn-sm btn-error",
                                onClick: async () => {
                                  await reloadAfter(
                                    () => api(`/api/guilds/${state.guildId}/badwords/${w.id}`, "DELETE"),
                                    () => toast("Removed", "success"),
                                  );
                                },
                              },
                              "✕",
                            )
                          : h("span", { className: "muted" }, "owner only"),
                      ),
                    );
                  }),
                ),
              ),
            ),
      ),
    ),
  );
}

function tabCommands() {
  const cmds = state.config.commands;
  return h(
    "div",
    {},
    h(
      "div",
      { className: "row title-row" },
      h("h2", {}, "Commands"),
      h(
        ActionButton,
        { className: "btn btn-primary", onClick: () => editCommand(null) },
        "+ New custom command",
      ),
    ),
    h(
      "p",
      { className: "muted", style: { margin: "6px 0 14px" } },
      "Every command lives in config: rename, reword responses, change permissions or cooldowns. " +
        "Custom commands run multi-step action plans (reply, dm, log, roles, timeout, kick, ban…). " +
        "Enabled commands are synced to Discord as slash commands.",
    ),
    h(
      "div",
      {
        className: "table-scroll rounded-box bg-base-100",
        tabIndex: 0,
        role: "region",
        "aria-label": "Scrollable data table",
      },
      h(
        "table",
        { className: "commands-table" + " table" },
        h(
          "thead",
          {},
          h(
            "tr",
            {},
            h("th", {}, "Command"),
            h("th", {}, "Category"),
            h("th", {}, "Description"),
            h("th", {}, "Permission"),
            h("th", {}, "State"),
            h("th", {}),
          ),
        ),
        h(
          "tbody",
          {},
          ...cmds.map((c) => {
            const d = c.effective;
            return h(
              "tr",
              {},
              h(
                "td",
                {},
                h(
                  "div",
                  { className: "command-identity" },
                  h("strong", { className: "mono" }, d.name),
                  h(
                    "div",
                    { className: "tag-group" },
                    c.guild_override
                      ? h(
                          "span",
                          { className: "badge badge-soft badge-sm badge-neutral override-tag" },
                          "server override",
                        )
                      : null,
                    c.scope === "global"
                      ? h("span", { className: "badge badge-soft badge-sm badge-primary scope-tag" }, "global")
                      : null,
                  ),
                ),
              ),
              h("td", {}, d.category),
              h("td", { className: "muted command-description" }, d.description),
              h("td", {}, d.permission),
              h(
                "td",
                {},
                h(
                  "span",
                  { className: `badge badge-soft badge-sm ${d.enabled ? "badge-success" : "badge-error"}` },
                  d.enabled ? "enabled" : "disabled",
                ),
              ),
              h(
                "td",
                {},
                h(ActionButton, { className: "btn btn-sm", onClick: () => editCommand(c) }, "Edit"),
              ),
            );
          }),
        ),
      ),
    ),
  );
}

function parsePlanParams(value) {
  const params = JSON.parse(value || "{}");
  if (!params || typeof params !== "object" || Array.isArray(params))
    throw new Error("plan parameters must be an object");
  if (params.steps !== undefined && !Array.isArray(params.steps))
    throw new Error("plan steps must be an array");
  params.steps = params.steps || [];
  if (params.steps.some((step) => !step || typeof step !== "object" || Array.isArray(step)))
    throw new Error("each plan step must be an object");
  return params;
}

function CommandEditor({ record: c }) {
  const isNew = c === null;
  const [data] = useState(() =>
    isNew
      ? {
          name: "",
          aliases: [],
          category: "Custom",
          description: "Custom command.",
          usage: "",
          enabled: true,
          permission: "everyone",
          roles: [],
          channels: [],
          cooldown_secs: 0,
          behavior: { kind: "plan", params: { steps: [{ type: "reply", template: "Hello {user}!" }] } },
          response: "",
          log_event: "",
        }
      : JSON.parse(JSON.stringify(c.effective)),
  );

  const nameInRef = useRef(null);
  const nameIn = h(Input, {
    ref: nameInRef,
    type: "text",
    defaultValue: data.name,
    placeholder: "command name",
    className: "input",
  });
  const descInRef = useRef(null);
  const descIn = h(Input, {
    ref: descInRef,
    type: "text",
    defaultValue: data.description,
    placeholder: "description",
    className: "input",
  });
  const usageInRef = useRef(null);
  const usageIn = h(Input, {
    ref: usageInRef,
    type: "text",
    defaultValue: data.usage,
    placeholder: "usage",
    className: "input",
  });
  const aliasesInRef = useRef(null);
  const aliasesIn = h(Input, {
    ref: aliasesInRef,
    type: "text",
    defaultValue: (data.aliases || []).join(","),
    placeholder: "aliases, comma separated",
    className: "input",
  });
  const respInRef = useRef(null);
  const respIn = h(Textarea, {
    ref: respInRef,
    defaultValue: data.response || "",
    placeholder: "response template, e.g. Banned {target} | {reason}",
    className: "textarea",
  });
  const permInRef = useRef(null);
  const permIn = h(
    Select,
    { ref: permInRef, defaultValue: data.permission, className: "select" },
    ["everyone", "mod", "admin", "owner"].map((p) => h("option", { value: p }, p)),
  );
  const cdInRef = useRef(null);
  const cdIn = h(Input, {
    ref: cdInRef,
    type: "number",
    min: 0,
    defaultValue: data.cooldown_secs || 0,
    className: "input",
  });
  const roleInRef = useRef(null);
  const roleIn = h(Input, {
    ref: roleInRef,
    type: "text",
    defaultValue: (data.roles || []).join(","),
    placeholder: "role ids",
    className: "input",
  });
  const chanInRef = useRef(null);
  const chanIn = h(Input, {
    ref: chanInRef,
    type: "text",
    defaultValue: (data.channels || []).join(","),
    placeholder: "channel ids",
    className: "input",
  });
  const logInRef = useRef(null);
  const logIn = h(Input, {
    ref: logInRef,
    type: "text",
    defaultValue: data.log_event || "",
    placeholder: "mod_action",
    className: "input",
  });
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef);
  const isPlan = data.behavior && data.behavior.kind === "plan";
  const [behaviorJson, setBehaviorJson] = useState(() =>
    JSON.stringify(isPlan ? data.behavior.params || {} : data.behavior || {}, null, 2),
  );
  const [stepsRevision, setStepsRevision] = useState(0);
  const renderSteps = () => setStepsRevision((value) => value + 1);
  const syncPlanJson = () => {
    if (isPlan) setBehaviorJson(JSON.stringify(data.behavior.params || {}, null, 2));
  };
  const behaviorAreaRef = useRef(null);
  const behaviorArea = h(Textarea, {
    ref: behaviorAreaRef,
    className: "mono textarea",
    style: { minHeight: "70px" },
    value: behaviorJson,
    onChange: (event) => setBehaviorJson(event.target.value),
    onBlur: () => {
      if (!isPlan) return;
      try {
        data.behavior.params = parsePlanParams(behaviorJson);
        renderSteps();
      } catch (error) {
        toast("Invalid behavior JSON: " + error.message, "error");
      }
    },
  });
  const typePlaceholder = (t) =>
    ({
      reply: "message template, e.g. Hi {user}!",
      dm: "DM template",
      log: "log template",
      add_role: "role id",
      remove_role: "role id",
      timeout: "minutes",
      ban: "delete_days",
      delete_command_message: "…",
      react: "emoji",
      wait: "seconds",
    })[t] || "value";
  const stepsBox = h(
    "div",
    null,
    isPlan &&
      data.behavior.params.steps.map((step, i) =>
        h(CommandStep, {
          key: itemKey(step),
          step,
          placeholder: typePlaceholder(step.type),
          onChange: () => {
            syncPlanJson();
            renderSteps();
          },
          remove: () => {
            data.behavior.params.steps.splice(i, 1);
            markDirty();
            syncPlanJson();
            renderSteps();
          },
        }),
      ),
  );
  void stepsRevision;
  return h(
    DialogPanel,
    { className: "modal-box", onInput: trackDraft },
    h("h3", { className: "card-title" }, isNew ? "New custom command" : `Edit command: ${c.name}`),
    h(
      "div",
      { className: "row form-row" },
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Name"), nameIn),
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Aliases"), aliasesIn),
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Permission"), permIn),
    ),
    h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Description"), descIn),
    h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Usage"), usageIn),
    isPlan
      ? h(
          "fieldset",
          { className: "fieldset field plan-steps" },
          h("legend", { className: "fieldset-legend" }, "Action plan steps"),
          stepsBox,
          h(
            ActionButton,
            {
              className: "btn btn-sm",
              onClick: () => {
                data.behavior.params.steps.push({ type: "reply", template: "" });
                markDirty();
                syncPlanJson();
                renderSteps();
              },
            },
            "+ add step",
          ),
        )
      : null,
    h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Behavior (advanced JSON)"), behaviorArea),
    h(
      Field,
      { className: "fieldset field" },
      h(Label, { className: "fieldset-legend" }, "Response template (in-chat reply; {target} {reason} {response} …)"),
      respIn,
    ),
    h(
      "div",
      { className: "row form-row" },
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Cooldown (secs)"), cdIn),
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Role restriction (ids)"), roleIn),
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Channel restriction (ids)"), chanIn),
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Log event"), logIn),
    ),
    h(
      "div",
      { className: "modal-action" },
      h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Applies to"), scopeSel),
      h(
        "label",
        { className: "checkbox-label" },
        h(Input, {
          type: "checkbox",
          defaultChecked: data.enabled,
          onChange: (e) => (data.enabled = e.target.checked),
          className: "checkbox",
        }),
        "Enabled",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-primary",
          onClick: async () => {
            try {
              if (isPlan) {
                data.behavior.params = parsePlanParams(behaviorAreaRef.current.value);
              } else {
                data.behavior = JSON.parse(behaviorAreaRef.current.value || "{}");
              }
            } catch (e) {
              toast("Invalid behavior JSON: " + e.message, "error");
              return;
            }
            data.name = nameInRef.current.value.trim().toLowerCase();
            if (!data.name) return toast("Name required", "error");
            data.aliases = aliasesInRef.current.value
              .split(",")
              .map((s) => s.trim())
              .filter(Boolean);
            data.description = descInRef.current.value;
            data.usage = usageInRef.current.value;
            data.response = respInRef.current.value;
            data.permission = permInRef.current.value;
            data.cooldown_secs = Number(cdInRef.current.value) || 0;
            data.roles = splitIds(roleInRef.current.value);
            data.channels = splitIds(chanInRef.current.value);
            data.log_event = logInRef.current.value;
            if (isPlan) {
              const steps = (data.behavior.params && data.behavior.params.steps) || [];
              for (const s of steps) {
                if (s.type === "add_role" || s.type === "remove_role") {
                  applyPlanStepInput(s, s.type, s.role_id);
                  if (!s.role_id) return toast("Role steps need a role id", "error");
                }
              }
            }
            await reloadAfter(
              () =>
                isNew
                  ? api(`/api/guilds/${state.guildId}/commands`, "POST", {
                      scope: scopeSelRef.current.value,
                      data,
                    })
                  : api(`/api/guilds/${state.guildId}/commands/${encodeURIComponent(c.name)}`, "PUT", {
                      scope: scopeSelRef.current.value,
                      data,
                    }),
              () => {
                toast("Command saved", "success");
                closeOverlay();
              },
            );
          },
        },
        "Save",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-error",
          hidden: isNew,
          onClick: async () => {
            if (!confirm(`Delete command ${c.name}?`)) return;
            // Prefer clearing a guild override (reset to global). Only delete the
            // global row when there is no override and the caller is the bot owner.
            const delScope = c.guild_override
              ? "guild"
              : c.scope === "global" && isBotOwner()
                ? "global"
                : "guild";
            await reloadAfter(
              () =>
                api(`/api/guilds/${state.guildId}/commands/${encodeURIComponent(c.name)}`, "DELETE", {
                  scope: delScope,
                }),
              () => {
                toast(c.guild_override ? "Override cleared" : "Deleted", "success");
                closeOverlay();
              },
            );
          },
        },
        "Delete",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-outline",
          onClick: () => {
            if (canLeave()) closeOverlay();
          },
        },
        "Cancel",
      ),
    ),
  );
}

function tabTemplates() {
  return h(
    "div",
    {},
    h("h2", {}, "Event templates"),
    h(
      "p",
      { className: "muted" },
      "Customize the messages sent to your log channels. Edit a template, review the sample, then save it for the selected scope.",
    ),
    h(
      "details",
      { className: "reference-panel" },
      h("summary", {}, "Template variables & message data"),
      h(
        "p",
        { className: "muted" },
        "Templates render into the configured log channels. Basic message reports supply {message_id} and {message_url}; updates also supply {after} and {attachments}. A single-message delete may supply {content} and author fields from a bounded recent in-process snippet when this process observed the create or content update; otherwise those values are unavailable. This is not persistent history or a Discord history lookup. Other event variables include {user} {mention} {user_id} {guild} {channel} {actor} {reason} {rule} {action} {content} {count} {account_age} {duration} {time}. Custom templates keep their wording. The readable-log upgrade updates exact previous defaults once; dates include whole seconds and UTC. Add fields to custom templates where needed.",
      ),
    ),
    ...state.config.templates.map((t) => h(TemplateCard, { key: t.event, record: t })),
  );
}

function TemplateCard({ record: t }) {
  const textRef = useRef(null);
  const text = h(Textarea, {
    ref: textRef,
    className: "mono template-input" + " textarea",
    rows: 8,
    spellCheck: false,
    defaultValue: t.effective,
    onInput: (event) => setPreview(event.target.value),
  });
  const [preview, setPreview] = useState(() =>
    t.effective
      .replaceAll("{user}", "Alice")
      .replaceAll("{mention}", "@alice")
      .replaceAll("{channel}", "#general")
      .replaceAll("{actor}", "Bob (mod)")
      .replaceAll("{reason}", "spam")
      .replaceAll("{rule}", "mention_spam")
      .replaceAll("{action}", "ban")
      .replaceAll("{content}", "FREE NITRO http://scam.link")
      .replaceAll("{count}", "3")
      .replaceAll("{time}", new Date().toISOString()),
  );
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef, "This server", "Global");
  return h(
    "div",
    { className: "card bg-base-100 card-border template-card" },
    h(
      "div",
      { className: "card-body" },
      eventHeading(t.event),
      h(
        "div",
        { className: "template-workspace" },
        field("Message template", text),
        h(
          "div",
          { className: "template-preview" },
          h("strong", {}, "Preview"),
          h("pre", { className: "code" }, preview),
        ),
      ),
      h(
        "div",
        { className: "editor-footer" },
        field("Save to", scopeSel),
        h(
          "div",
          { className: "row" },
          h(
            ActionButton,
            {
              className: "btn btn-primary btn-sm",
              onClick: async () => {
                await reloadAfter(
                  () =>
                    api(`/api/guilds/${state.guildId}/templates/${t.event}`, "PUT", {
                      scope: scopeSelRef.current.value,
                      template: textRef.current.value,
                    }),
                  () => toast("Template saved", "success"),
                );
              },
            },
            "Save",
          ),
          h(
            ActionButton,
            {
              className: "btn btn-sm",
              onClick: async () => {
                await reloadAfter(
                  () =>
                    api(`/api/guilds/${state.guildId}/templates/${t.event}`, "DELETE", { scope: "guild" }),
                  () => toast("Reset", "success"),
                );
              },
            },
            "Reset to global",
          ),
        ),
      ),
    ),
  );
}

function tabLogChannels() {
  return h(
    "div",
    {},
    h(
      "div",
      { className: "row title-row" },
      h("h2", {}, "Log channels"),
      h(
        ActionButton,
        {
          className: "btn",
          onClick: async () => {
            if (!canLeave()) return;
            if (await loadConfig()) renderTab();
          },
        },
        "Refresh configuration",
      ),
    ),
    h(
      "p",
      { className: "muted", style: { marginBottom: "12px" } },
      "Each event type routes to a channel in this server. Enter a channel ID, <#id> mention, or channel URL. Role and user ID fields on other tabs accept the same snowflake forms. Changes show as queued while the bot validates them, then reconcile automatically; Bot actions has the full receipt.",
    ),
    ...state.config.log_channels.map((lc) => h(LogChannelCard, { key: lc.event, lc })),
  );
}

function LogChannelCard({ lc }) {
  const guildId = state.guildId;
  const inputRef = useRef(null);
  const input = h(Input, {
    ref: inputRef,
    type: "text",
    defaultValue: lc.effective && String(lc.effective) !== "0" ? String(lc.effective) : "",
    placeholder: "channel id",
    className: "input",
  });
  const ignoredUsersRef = useRef(null);
  const ignoredUsers = h(Textarea, {
    ref: ignoredUsersRef,
    placeholder: "One Discord user or bot ID per line",
    rows: 2,
    className: "textarea",
    defaultValue: (lc.ignore?.users || []).join("\n"),
  });
  const ignoredChannelsRef = useRef(null);
  const ignoredChannels = h(Textarea, {
    ref: ignoredChannelsRef,
    placeholder: "One source channel ID per line",
    rows: 2,
    className: "textarea",
    defaultValue: (lc.ignore?.channels || []).join("\n"),
  });
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef, "This server", "Global", false);
  return h(
    "div",
    { className: "card bg-base-100 card-border log-channel-card" },
    h(
      "div",
      { className: "card-body" },
      h(
        "div",
        { className: "row title-row" },
        eventHeading(lc.event),
        lc.pending_action
          ? h(
              "span",
              { className: "badge badge-soft badge-sm badge-warning" },
              ["pending", "claimed", "running"].includes(lc.pending_action.state)
                ? "queued"
                : lc.pending_action.state,
            )
          : null,
      ),
      h(
        "div",
        { className: "routing-fields" },
        field("Destination channel", input),
        field("Applies to", scopeSel),
        h(
          "div",
          { className: "row routing-actions" },
          h(
            ActionButton,
            {
              className: "btn btn-sm btn-primary",
              savedInputs: [inputRef],
              onClick: async () => {
                const v = parseSnowflake(inputRef.current.value);
                if (!v) return toast("Enter a Discord channel ID, <#id> mention, or channel URL", "error");
                const job = await submitBotAction(`/api/guilds/${guildId}/logchannels/${lc.event}`, "PUT", {
                  scope: scopeSelRef.current.value,
                  channel_id: v,
                });
                if (applyQueuedLogChannel(job)) {
                  renderTab();
                  if (["pending", "claimed", "running"].includes(job.state)) scheduleLogChannelReconcile(job);
                  else if (job.state !== "succeeded") await reconcileQueuedLogChannel(job);
                }
              },
            },
            "Set",
          ),
          h(
            ActionButton,
            {
              className: "btn btn-sm",
              savedInputs: [inputRef],
              onClick: async () => {
                const job = await submitBotAction(
                  `/api/guilds/${guildId}/logchannels/${lc.event}`,
                  "DELETE",
                  {
                    scope: scopeSelRef.current.value,
                  },
                );
                if (applyQueuedLogChannel(job)) {
                  renderTab();
                  if (["pending", "claimed", "running"].includes(job.state)) scheduleLogChannelReconcile(job);
                  else if (job.state !== "succeeded") await reconcileQueuedLogChannel(job);
                }
              },
            },
            "Clear",
          ),
        ),
      ),
      h(
        "div",
        { className: "log-ignore-section" },
        h("h4", {}, "Event ignores"),
        h(
          "p",
          { className: "field-help" },
          "Skip reports involving these users or source channels. Leave empty to report all.",
        ),
        h(
          "div",
          { className: "ignore-fields" },
          field("Ignore user or bot IDs", ignoredUsers),
          field("Ignore source channel IDs", ignoredChannels),
        ),
        h(
          ActionButton,
          {
            className: "btn btn-sm",
            savedInputs: [ignoredUsersRef, ignoredChannelsRef],
            onClick: async () => {
              const users = ignoredUsersRef.current.value
                .split(/\r?\n|,/)
                .map((id) => id.trim())
                .filter(Boolean)
                .map((id) => parseLogFilterId(id, "user"));
              if (users.some((id) => !id)) return toast("Enter valid Discord user or bot IDs", "error");
              const channels = ignoredChannelsRef.current.value
                .split(/\r?\n|,/)
                .map((id) => id.trim())
                .filter(Boolean)
                .map((id) => parseLogFilterId(id, "channel"));
              if (channels.some((id) => !id))
                return toast("Enter a valid source channel ID, <#id> mention, or channel URL", "error");
              const saved = await api(`/api/guilds/${guildId}/logfilters/${lc.event}`, "PUT", {
                users,
                channels,
              });
              lc.ignore = saved.ignore;
              toast("Ignore list saved", "success");
              renderTab();
            },
          },
          "Save ignores",
        ),
      ),
    ),
  );
}

function actionIsLogChannel(job) {
  return job && job.action && job.action.kind === "set_log_channel";
}

function actionCanCancel(job) {
  if (!job) return false;
  if (job.state === "pending" || job.state === "claimed") return true;
  return actionIsLogChannel(job) && (job.state === "running" || job.state === "uncertain");
}

function actionCancelLabel(job) {
  return job && (job.state === "running" || job.state === "uncertain") ? "Force cancel" : "Cancel";
}

function actionCancelNeedsConfirm(job) {
  return actionIsLogChannel(job) && (job.state === "running" || job.state === "uncertain");
}

function memberTime(value) {
  if (value === null || value === undefined || value === "") return "Unknown";
  const date = new Date(typeof value === "string" && !/^\d+$/.test(value) ? value : Number(value));
  return Number.isNaN(date.getTime()) ? "Unknown" : date.toLocaleString(undefined, { timeZoneName: "short" });
}

function tabMembers() {
  const guildId = state.guildId;
  const enrollmentId = state.config?.enrollment_id;
  const handlers = useRef({});
  const [syncParts, setSyncParts] = useState([h("p", { key: "loading" }, "Loading import status…")]);
  const [memberRows, setMemberRows] = useState([]);
  const [detailParts, setDetailParts] = useState([]);
  const [pagination, setPagination] = useState({ first: true, next: true });
  const searchRef = useRef(null);
  const search = h(Input, {
    ref: searchRef,
    onInput: () => handlers.current.queueFilteredList?.(),
    onBlur: () => handlers.current.loadChangedFilters?.(),
    type: "search",
    placeholder: "User ID, mention, display name or username",
    "aria-label": "Search members",
    className: "input",
  });
  const statusFilterRef = useRef(null);
  const statusFilter = h(
    Select,
    {
      ref: statusFilterRef,
      onChange: () => handlers.current.loadChangedFilters?.(),
      "aria-label": "Member status",
      className: "select",
    },
    ...[
      ["current", "Current members"],
      ["banned", "Currently banned"],
      ["previously_banned", "Previously banned"],
      ["all", "All retained"],
    ].map(([value, label]) => h("option", { value: value }, label)),
  );
  const joinedFromRef = useRef(null);
  const joinedFrom = h(Input, {
    ref: joinedFromRef,
    onInput: () => handlers.current.queueFilteredList?.(),
    onBlur: () => handlers.current.loadChangedFilters?.(),
    type: "date",
    "aria-label": "Joined from date, UTC",
    className: "input",
  });
  const joinedToRef = useRef(null);
  const joinedTo = h(Input, {
    ref: joinedToRef,
    onInput: () => handlers.current.queueFilteredList?.(),
    onBlur: () => handlers.current.loadChangedFilters?.(),
    type: "date",
    "aria-label": "Joined through date, UTC",
    className: "input",
  });
  const sortRef = useRef(null);
  const sort = h(
    Select,
    {
      ref: sortRef,
      onChange: () => handlers.current.loadChangedFilters?.(),
      "aria-label": "Join date order",
      className: "select",
    },
    h("option", { value: "joined_newest" }, "Newest join first"),
    h("option", { value: "joined_oldest" }, "Oldest join first"),
  );
  useEffect(() => {
    const alive = () =>
      state.guildId === guildId && state.config?.enrollment_id === enrollmentId && state.tab === "members";
    let listRequest = 0,
      statusRequest = 0,
      detailRequest = 0;
    let listAbort = null,
      statusAbort = null,
      detailAbort = null;
    let timer = null,
      countdownTimer = null,
      debounce = null,
      disposed = false,
      after = "",
      nextAfter = null;
    let latestStatus = null,
      latestStatusRevision = -1,
      statusRetryNeeded = false,
      lastStatusError = null;
    let latestStatusReceivedAt = 0;
    let pendingSyncAction = null,
      syncActionOutcome = null;
    let emptyMembersCell = null;
    const emptyMembersText = () =>
      latestStatus?.status === "complete"
        ? "No members match these filters."
        : "No saved members match yet. Import may still be incomplete.";
    const aliveHere = () => !disposed && alive();
    const clearTimer = () => {
      if (timer !== null) clearTimeout(timer);
      timer = null;
    };
    const clearCountdown = () => {
      if (countdownTimer !== null) clearTimeout(countdownTimer);
      countdownTimer = null;
    };
    const auditPaginationStalled = (sync) =>
      sync?.audit_recovery_status === "needs_attention" &&
      sync?.audit_recovery_reason === "pagination_stalled";
    const memberSyncRetryEnabled = (sync) =>
      ["needs_attention", "waiting"].includes(sync?.status) ||
      (sync?.status === "complete" && auditPaginationStalled(sync));
    const schedulePoll = () => {
      clearTimer();
      if (!aliveHere() || document.visibilityState !== "visible") return;
      const active =
        statusRetryNeeded ||
        pendingSyncAction ||
        (latestStatus &&
          (["queued", "running", "waiting"].includes(latestStatus.status) ||
            ["queued", "running", "waiting"].includes(latestStatus.audit_recovery_status)));
      if (!active) return;
      let delay = 10000;
      if (isDiscordRateLimited(lastStatusError)) {
        delay = Math.max(10000, Math.min(lastStatusError.retryAfterMs || 10000, 900000));
      }
      timer = setTimeout(loadStatus, delay);
    };
    const asDate = (value) => memberTime(value);
    const renderStatus = (sync) => {
      clearCountdown();
      const parts = [];
      const title = sync.run_kind === "reconcile" ? "Reconcile from Discord" : "Initial member import";
      parts.push(h("h3", { className: "card-title" }, title));
      const stage = [sync.phase || "pending", sync.status || "needs attention"].join(" · ");
      parts.push(
        h("p", { role: "status" }, `${stage}${sync.status_reason ? ` — ${sync.status_reason}` : ""}`),
      );
      if (!sync.run_id && sync.status === "queued") {
        parts.push(
          h(
            "p",
            { className: "muted" },
            "The bot will create the first import job when it is online. Import controls become available after that job exists.",
          ),
        );
      } else if (!sync.run_id && sync.status === "needs_attention") {
        parts.push(
          h(
            "p",
            { className: "alert alert-warning" },
            "Import metadata is unavailable. Controls are disabled until the saved job is restored or repaired.",
          ),
        );
      }
      const phaseCounts = {
        members: sync.member_processed,
        bans: sync.ban_processed,
        verify: sync.verify_processed,
        audit: sync.audit_processed,
      };
      const processed = Number(sync.processed ?? sync.committed_count ?? sync.committed ?? 0);
      const phaseProcessed = phaseCounts[sync.phase] == null ? null : Number(phaseCounts[sync.phase]);
      const total = sync.total_estimate == null ? null : Number(sync.total_estimate);
      const percent =
        sync.status === "complete"
          ? 100
          : sync.percent == null
            ? null
            : Math.min(99, Math.max(0, Number(sync.percent)));
      const totalKind = sync.total_is_estimated === false ? "counted" : "estimated";
      const bar = h("progress", {
        max: "100",
        value: sync.status === "complete" ? "100" : percent,
        "aria-label": "Import progress",
        "aria-valuetext":
          sync.status === "complete"
            ? "Complete"
            : percent === null
              ? `${phaseProcessed == null ? "Unknown number of" : phaseProcessed} records committed in current phase; total unknown`
              : `${Math.round(percent)} percent of ${totalKind} member stage`,
      });
      parts.push(bar);
      const memberPhase = sync.phase === "members";
      let totalDescription = "current phase total unknown";
      if (total !== null && sync.total_is_estimated === false)
        totalDescription = `${total.toLocaleString()} members counted`;
      else if (total !== null && memberPhase)
        totalDescription = `approximately ${total.toLocaleString()} members estimated`;
      const progressText =
        sync.status === "complete"
          ? " · 100% complete"
          : percent === null
            ? ""
            : ` · ${Math.round(percent)}% of ${totalKind} member phase`;
      parts.push(
        h(
          "p",
          {},
          `${Number.isFinite(processed) ? processed.toLocaleString() : "0"} records committed in this run · ${totalDescription}${progressText}`,
        ),
      );
      const phases = ["members", "bans", "verify", "audit", "local_history"];
      const phaseLabels = {
        members: "Members",
        bans: "Current bans",
        verify: "Candidate checks",
        audit: "Available audit history",
        local_history: "Local case history",
      };
      const currentPhase =
        sync.phase === "complete" && sync.status !== "complete" && sync.local_history_complete === false
          ? phases.indexOf("local_history")
          : phases.indexOf(String(sync.phase || ""));
      parts.push(
        h(
          "p",
          { className: "muted" },
          phases
            .map((phase, index) => {
              const state =
                sync.status === "complete" || (currentPhase >= 0 && index < currentPhase)
                  ? "done"
                  : index === currentPhase
                    ? sync.status
                    : "pending";
              const count =
                phaseCounts[phase] == null
                  ? " · committed count unknown"
                  : ` · ${Number(phaseCounts[phase]).toLocaleString()} committed`;
              return `${phaseLabels[phase]}: ${state}${count}`;
            })
            .join(" · "),
        ),
      );
      const serverReadAt = Number(sync.as_of ?? sync.as_of_ms);
      const statusNow =
        Number.isFinite(serverReadAt) && serverReadAt > 0
          ? serverReadAt + Math.max(0, Date.now() - latestStatusReceivedAt)
          : Date.now();
      const elapsed =
        sync.elapsed_seconds ??
        (sync.started_at ? Math.max(0, (statusNow - Number(sync.started_at)) / 1000) : null);
      const workerUnconfirmed = ["worker_unconfirmed", "worker_offline"].includes(sync.estimate_reason);
      const forecastUnknown =
        workerUnconfirmed ||
        ["paused", "needs_attention"].includes(sync.status) ||
        (sync.status === "waiting" && sync.estimate_reason !== "rate_limited") ||
        sync.estimate_reason === "discord_unavailable";
      const phaseFinish = Number(sync.phase_finish_at);
      const forecastAvailable =
        !forecastUnknown &&
        sync.phase_remaining_seconds != null &&
        sync.phase_finish_at != null &&
        Number.isFinite(phaseFinish) &&
        phaseFinish > statusNow;
      const remaining = workerUnconfirmed
        ? "Unknown until worker activity is confirmed"
        : !forecastAvailable
          ? "Unknown"
          : `about ${Math.ceil((phaseFinish - statusNow) / 60000)} minutes for this phase`;
      const overallFinish = Number(sync.overall_finish_at);
      const finish = forecastAvailable
        ? asDate(
            sync.overall_finish_at != null && Number.isFinite(overallFinish) && overallFinish > statusNow
              ? sync.overall_finish_at
              : sync.phase_finish_at,
          )
        : "Unknown";
      parts.push(
        h(
          "p",
          { className: "muted" },
          `Elapsed: ${elapsed == null ? "Unknown" : `${Math.round(Number(elapsed) / 60)} minutes`} · Remaining: ${remaining} · Expected finish: ${finish}`,
        ),
      );
      if (
        sync.phase === "members" &&
        sync.phase_finish_at != null &&
        sync.estimate_reason?.startsWith("preliminary member-stage estimate")
      ) {
        parts.push(
          h(
            "p",
            { className: "muted" },
            "Preliminary member-stage estimate; later stages have unknown totals.",
          ),
        );
      }
      if (sync.estimate_reason === "worker_unconfirmed")
        parts.push(
          h(
            "p",
            { className: "muted" },
            "Worker activity is not recently confirmed. The bot may be between paced pages; timing will resume when activity is confirmed.",
          ),
        );
      if (sync.estimate_reason === "worker_offline")
        parts.push(
          h("p", { className: "muted" }, "The bot worker is offline. Timing will resume when it is online."),
        );
      if (sync.next_attempt_at) {
        const nextAttempt = Number(sync.next_attempt_at);
        const rateLimited =
          sync.status === "waiting" &&
          (sync.status_reason === "rate_limited" || sync.estimate_reason === "rate_limited");
        let retryText = `Next attempt: ${asDate(sync.next_attempt_at)}`;
        if (rateLimited && Number.isFinite(nextAttempt)) {
          const seconds = Math.max(0, Math.ceil((nextAttempt - statusNow) / 1000));
          retryText = `${seconds ? `Rate limit: retry in ${Math.floor(seconds / 60)}m ${seconds % 60}s` : "Rate limit: retry due now"} · ${retryText}`;
          if (
            seconds &&
            aliveHere() &&
            document.visibilityState === "visible" &&
            !statusRetryNeeded &&
            latestStatus === sync
          ) {
            countdownTimer = setTimeout(() => {
              countdownTimer = null;
              if (aliveHere() && latestStatus === sync) renderStatus(sync);
            }, 1000);
          }
        }
        parts.push(h("p", { className: "muted" }, retryText));
      }
      if (sync.initial_completed_at_ms || sync.initial_completed_at)
        parts.push(
          h(
            "p",
            {},
            `Initial import completed: ${asDate(sync.initial_completed_at_ms ?? sync.initial_completed_at)}`,
          ),
        );
      if (sync.last_success_at) {
        const counts = [
          ["processed", "processed"],
          ["inserted", "inserted"],
          ["updated", "updated"],
          ["unchanged", "unchanged"],
        ]
          .filter(([field]) => sync[`last_success_${field}`] != null)
          .map(([field, label]) => `${Number(sync[`last_success_${field}`]).toLocaleString()} ${label}`);
        parts.push(
          h(
            "p",
            { className: "muted" },
            `Last successful run: ${asDate(sync.last_success_at)}${counts.length ? ` · ${counts.join(" · ")}` : ""}`,
          ),
        );
      }
      if (sync.reconciliation_needed)
        parts.push(
          h(
            "p",
            { className: "alert alert-warning" },
            "Coverage may be stale after an outage, restore, bot restart, or Gateway session replacement. A full current-member and current-ban check needs an administrator to select Reconcile from Discord.",
          ),
        );
      if (sync.audit_recovery_status && sync.audit_recovery_status !== "complete") {
        const phase =
          sync.audit_recovery_phase === "ban"
            ? "Ban actions"
            : sync.audit_recovery_phase === "unban"
              ? "Unban actions"
              : "Ban and unban actions";
        parts.push(
          h(
            "p",
            { className: "muted" },
            `Bounded audit recovery: ${phase} · ${sync.audit_recovery_status} · ${Number(sync.audit_recovery_processed || 0).toLocaleString()} entries checked${sync.audit_recovery_reason ? ` · ${sync.audit_recovery_reason}` : ""}`,
          ),
        );
        if (sync.audit_recovery_next_attempt_at)
          parts.push(
            h(
              "p",
              { className: "muted" },
              `Audit recovery next attempt: ${asDate(sync.audit_recovery_next_attempt_at)}`,
            ),
          );
        if (sync.audit_recovery_last_progress_at)
          parts.push(
            h(
              "p",
              { className: "muted" },
              `Audit recovery last progressed: ${asDate(sync.audit_recovery_last_progress_at)}`,
            ),
          );
      }
      if (Number(sync.unseen_candidate_count || 0) > 0)
        parts.push(
          h(
            "p",
            { className: "muted" },
            `${Number(sync.unseen_candidate_count).toLocaleString()} absence candidates still need bounded Discord checks.`,
          ),
        );
      if (sync.status !== "complete")
        parts.push(
          h(
            "p",
            { className: "muted" },
            "Results are partial while import is incomplete. Older join or ban dates may be unavailable.",
          ),
        );
      parts.push(h("p", { className: "muted" }, `Last committed progress: ${asDate(sync.last_progress_at)}`));
      if (sync.as_of_ms || sync.as_of)
        parts.push(h("p", { className: "muted" }, `Status read at ${asDate(sync.as_of_ms ?? sync.as_of)}`));
      if (pendingSyncAction)
        parts.push(
          h(
            "p",
            { role: "status" },
            `${pendingSyncAction.operation[0].toUpperCase()}${pendingSyncAction.operation.slice(1)} requested · bot action ${pendingSyncAction.state}. Waiting for the import status to reflect it.`,
          ),
        );
      if (syncActionOutcome) parts.push(h("p", { className: "err", role: "status" }, syncActionOutcome));
      const controls = [];
      if (!isPreview()) {
        for (const [operation, label] of [
          ["pause", "Pause"],
          ["resume", "Resume"],
          ["retry", "Retry"],
          ["reconcile", "Reconcile from Discord"],
        ]) {
          const disabled =
            !sync.run_id ||
            !!pendingSyncAction ||
            (operation === "pause" && !["queued", "running", "waiting"].includes(sync.status)) ||
            (operation === "resume" && sync.status !== "paused") ||
            (operation === "retry" && !memberSyncRetryEnabled(sync)) ||
            (operation === "reconcile" && (sync.status !== "complete" || !sync.initial_completed_at));
          controls.push(
            h(
              ActionButton,
              {
                className: "btn btn-sm",
                disabled: disabled,
                onClick: async () => {
                  if (!latestStatus?.run_id) return;
                  if (
                    operation === "reconcile" &&
                    !confirm(
                      "Request a full current-member and current-ban reconciliation from Discord? This can take a while.",
                    )
                  )
                    return;
                  let job;
                  try {
                    job = await submitBotAction(`/api/guilds/${guildId}/member-sync/actions`, "POST", {
                      operation,
                    });
                  } catch (error) {
                    if (
                      error.status !== 409 ||
                      !["member_sync_missing_job", "member_sync_invalid_operation"].includes(error.code)
                    )
                      throw error;
                    if (!aliveHere()) return;
                    syncActionOutcome = error.message;
                    renderStatus(latestStatus);
                    loadStatus();
                    return;
                  }
                  if (!aliveHere()) return;
                  syncActionOutcome = null;
                  if (["pending", "claimed", "running", "succeeded"].includes(job.state)) {
                    pendingSyncAction = { id: job.id, operation, state: job.state };
                  } else {
                    syncActionOutcome = `${label} action ${job.state}. Check Bot actions for details.`;
                  }
                  renderStatus(latestStatus);
                  schedulePoll();
                  loadStatus();
                },
              },
              label,
            ),
          );
        }
      }
      parts.push(h("div", { className: "row" }, ...controls));
      setSyncParts(parts);
    };
    const loadStatus = async () => {
      // A succeeded receipt is durable before this primary status read starts.
      // Its effect (or a later state) must therefore be visible in the result.
      const succeededId = pendingSyncAction?.state === "succeeded" ? pendingSyncAction.id : null;
      const request = ++statusRequest;
      statusAbort?.abort();
      statusAbort = new AbortController();
      try {
        const sync = await apiRead(`/api/guilds/${guildId}/member-sync`, statusAbort.signal);
        if (!aliveHere() || request !== statusRequest) return;
        const revision = Number(sync.progress_revision ?? 0);
        if (latestStatus && revision < latestStatusRevision && pendingSyncAction) {
          syncActionOutcome =
            "Import status changed after a restore. Check Bot actions before repeating the requested control.";
          pendingSyncAction = null;
        }
        statusRetryNeeded = false;
        lastStatusError = null;
        if (succeededId && pendingSyncAction?.id === succeededId) pendingSyncAction = null;
        latestStatus = sync;
        latestStatusRevision = revision;
        latestStatusReceivedAt = Date.now();
        if (emptyMembersCell)
          setMemberRows([h("tr", null, h("td", { colSpan: 5, className: "muted" }, emptyMembersText()))]);
        renderStatus(sync);
        schedulePoll();
        if (pendingSyncAction) {
          const pendingId = pendingSyncAction.id;
          try {
            const jobs = await apiRead(`/api/guilds/${guildId}/actions`, statusAbort.signal);
            if (!aliveHere() || request !== statusRequest || pendingSyncAction?.id !== pendingId) return;
            const job = jobs.find((item) => item.id === pendingId);
            if (job && ["failed", "uncertain", "cancelled", "expired"].includes(job.state)) {
              syncActionOutcome = `${pendingSyncAction.operation} action ${job.state}. Check Bot actions for details.`;
              pendingSyncAction = null;
            } else if (job) {
              pendingSyncAction.state = job.state;
              if (job.state === "succeeded") {
                // This status was read before the receipt. Read again in causal order.
                loadStatus();
                return;
              }
            }
            renderStatus(sync);
            schedulePoll();
          } catch (error) {
            // Keep the acknowledged request visible and retry its receipt on the next poll.
          }
        }
      } catch (error) {
        if (!aliveHere() || request !== statusRequest) return;
        statusRetryNeeded = true;
        lastStatusError = error;
        if (latestStatus) renderStatus(latestStatus);
        else setSyncParts([]);
        setSyncParts((parts) => [
          ...parts,
          h(
            "p",
            { className: "err", role: "status" },
            latestStatus
              ? "Could not refresh import status. Showing the last saved status; retrying shortly while visible."
              : "Could not load import status. Retrying shortly while visible.",
          ),
        ]);
        schedulePoll();
      }
    };
    const loadMembers = async () => {
      const request = ++listRequest;
      listAbort?.abort();
      listAbort = new AbortController();
      emptyMembersCell = null;
      setMemberRows([h("tr", {}, h("td", { colSpan: 5 }, "Loading members…"))]);
      try {
        const q = new URLSearchParams({
          limit: "50",
          status: statusFilterRef.current.value,
          sort: sortRef.current.value,
        });
        const rawSearch = String(searchRef.current.value || "").trim();
        const mention = rawSearch.match(/^<@!?(\d+)>$/);
        if (rawSearch) q.set("q", mention ? mention[1] : rawSearch);
        if (joinedFromRef.current.value) q.set("joined_from", joinedFromRef.current.value);
        if (joinedToRef.current.value) q.set("joined_to", joinedToRef.current.value);
        if (after) q.set("after", after);
        const page = await apiRead(`/api/guilds/${guildId}/members?${q}`, listAbort.signal);
        if (!aliveHere() || request !== listRequest) return;
        nextAfter = page.next_cursor || null;
        setPagination({ first: !after, next: !nextAfter });
        const rows = (page.members || []).map((member) =>
          h(
            "tr",
            {},
            h(
              "td",
              {},
              h("strong", {}, member.display_name || member.username || member.user_id),
              member.username ? h("small", { className: "muted" }, ` @${member.username}`) : null,
            ),
            h(
              "td",
              {},
              h("code", {}, member.user_id),
              " ",
              h(
                ActionButton,
                {
                  className: "btn btn-sm",
                  "aria-label": `Copy user ID ${member.user_id}`,
                  onClick: () => navigator.clipboard.writeText(member.user_id),
                },
                "Copy",
              ),
            ),
            h("td", {}, asDate(member.joined_at_ms)),
            h(
              "td",
              {},
              member.is_banned
                ? "Currently banned"
                : member.is_current
                  ? "Current"
                  : member.ever_banned
                    ? "Previously banned"
                    : "Departed",
              member.is_banned && member.current_ban_reason
                ? h("small", { className: "muted" }, ` · Current reason: ${member.current_ban_reason}`)
                : null,
            ),
            h(
              "td",
              {},
              h(
                ActionButton,
                { className: "btn btn-sm", onClick: () => openMember(member.user_id, member.display_name) },
                "View history",
              ),
            ),
          ),
        );
        emptyMembersCell = rows.length
          ? null
          : h("td", { colSpan: 5, className: "muted" }, emptyMembersText());
        setMemberRows(rows.length ? rows : [h("tr", {}, emptyMembersCell)]);
      } catch (error) {
        if (!aliveHere() || request !== listRequest || error.message === "Failed to fetch") return;
        setMemberRows([
          h("tr", {}, h("td", { colSpan: 5, className: "err" }, "Could not load members. Refresh to retry.")),
        ]);
      }
    };
    const openMember = async (userId, displayName) => {
      const request = ++detailRequest;
      detailAbort?.abort();
      detailAbort = new AbortController();
      let cursor = null;
      setDetailParts([h("p", {}, "Loading member history…")]);
      const parts = [];
      const appendPage = async () => {
        const q = new URLSearchParams({ limit: "50" });
        if (cursor) q.set("after", cursor);
        const page = await apiRead(
          `/api/guilds/${guildId}/members/${userId}/history?${q}`,
          detailAbort.signal,
        );
        if (!aliveHere() || request !== detailRequest) return;
        if (!cursor)
          parts.push(
            h(
              "h3",
              { className: "card-title" },
              displayName ? `History for ${displayName} (${userId})` : `History for ${userId}`,
            ),
          );
        for (const event of page.entries || []) {
          const sourceLabel =
            {
              discord_audit: "Discord audit action",
              discord_audit_unresolved: "Confirmed Discord audit action; local case association unresolved",
              application_audit_linked: "Bot action linked to Discord audit",
              gateway_observation_unlinked: "Gateway observation; action time unconfirmed",
              state_reconciliation: "Verified from current Discord state; transition time unknown",
              application: "Bot action",
            }[event.source_kind] || "";
          const eventLabel =
            event.source_kind === "gateway_observation_unlinked"
              ? `${event.kind || "Ban state"} observation`
              : event.source_kind === "discord_audit_unresolved"
                ? `${event.kind === "ban" ? "Ban" : event.kind === "unban" ? "Unban" : "Discord"} audit entry`
                : event.kind || "Observation";
          const visibleLabel =
            event.kind === "member_state_observation"
              ? "Membership absence"
              : event.kind === "membership_state_observation"
                ? "Membership state check"
                : event.kind === "ban_present_state_observation"
                  ? "Ban present (time unknown)"
                  : event.kind === "ban_absent_state_observation" || event.kind === "ban_state_observation"
                    ? "Ban absent (time unknown)"
                    : eventLabel;
          parts.push(
            h(
              "p",
              {},
              h("strong", {}, `${visibleLabel} · ${asDate(event.occurred_at_ms)}`),
              event.case_id
                ? h(
                    ActionButton,
                    {
                      className: "btn btn-sm",
                      onClick: () => {
                        if (!aliveHere()) return;
                        pendingCase = { guildId, caseId: event.case_id };
                        if (!navigate("cases")) pendingCase = null;
                      },
                    },
                    `Case ${event.case_id}`,
                  )
                : null,
              h(
                "span",
                { className: "muted" },
                `${sourceLabel ? ` ${sourceLabel}` : ""}${event.source_kind === "discord_audit_unresolved" && event.observation_id ? ` · Audit entry ${event.observation_id}` : ""}${event.observed_at_ms && event.source_kind === "state_reconciliation" ? ` · Checked ${asDate(event.observed_at_ms)}` : ""}${event.outcome ? ` · ${event.outcome}` : ""}${event.moderator_id ? ` · moderator ${event.moderator_id}` : ""}${event.reason ? ` · ${event.reason}` : ""}`,
              ),
            ),
          );
        }
        cursor = page.next_cursor || null;
        if (cursor) {
          setDetailParts([
            ...parts,
            h(
              ActionButton,
              {
                className: "btn btn-sm",
                onClick: async () => {
                  setDetailParts([...parts]);
                  await appendPage();
                },
              },
              "More history",
            ),
          ]);
        } else {
          if (!(page.entries || []).length)
            parts.push(h("p", { className: "muted" }, "No known events. Older history may be unavailable."));
          setDetailParts([...parts]);
        }
      };
      try {
        await appendPage();
      } catch (error) {
        if (aliveHere() && request === detailRequest && error.message !== "Failed to fetch")
          setDetailParts([h("p", { className: "err" }, "Could not load history.")]);
      }
    };
    const invalidateResults = () => {
      ++listRequest;
      listAbort?.abort();
      emptyMembersCell = null;
      ++detailRequest;
      detailAbort?.abort();
      after = "";
      nextAfter = null;
      setPagination({ first: true, next: true });
      setMemberRows([h("tr", {}, h("td", { colSpan: 5 }, "Loading members…"))]);
      setDetailParts([]);
    };
    const queueFilteredList = () => {
      if (!aliveHere()) return;
      invalidateResults();
      clearTimeout(debounce);
      debounce = setTimeout(() => {
        debounce = null;
        if (aliveHere()) loadMembers();
      }, 300);
    };
    const loadChangedFilters = () => {
      if (!aliveHere()) return;
      clearTimeout(debounce);
      debounce = null;
      invalidateResults();
      loadMembers();
    };
    handlers.current = {
      queueFilteredList,
      loadChangedFilters,
      first: () => {
        after = "";
        loadMembers();
      },
      next: () => {
        if (nextAfter) {
          after = nextAfter;
          loadMembers();
        }
      },
      refresh: () => {
        loadStatus();
        loadMembers();
      },
    };
    const visibility = () => {
      if (document.visibilityState === "visible" && aliveHere()) loadStatus();
      else {
        clearTimer();
        clearCountdown();
      }
    };
    document.addEventListener("visibilitychange", visibility);
    const dispose = () => {
      disposed = true;
      clearTimer();
      clearCountdown();
      clearTimeout(debounce);
      listAbort?.abort();
      statusAbort?.abort();
      detailAbort?.abort();
      document.removeEventListener("visibilitychange", visibility);
    };
    loadStatus();
    loadMembers();
    return dispose;
  }, [guildId, enrollmentId]);
  return h(
    "div",
    null,
    h("h2", null, "Members"),
    h(
      "p",
      { className: "muted" },
      "Search saved member identities and known join dates. Import and historical coverage are shown below.",
    ),
    h(
      "section",
      { className: "card bg-base-100 card-border", "aria-label": "Member import status" },
      ...syncParts,
    ),
    h(
      "section",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("div", { className: "row" }, search, statusFilter, joinedFrom, joinedTo, sort),
        h(
          "div",
          { className: "row" },
          h(ActionButton, { className: "btn", onClick: () => handlers.current.refresh?.() }, "Refresh"),
          h(
            ActionButton,
            { className: "btn", disabled: pagination.first, onClick: () => handlers.current.first?.() },
            "First page",
          ),
          h(
            ActionButton,
            { className: "btn", disabled: pagination.next, onClick: () => handlers.current.next?.() },
            "Next page",
          ),
        ),
        h(DataTable, { headings: ["Member", "User ID", "Joined", "Status", "History"] }, memberRows),
      ),
    ),
    h(
      "section",
      { className: "card bg-base-100 card-border", "aria-label": "Member history" },
      ...detailParts,
    ),
  );
}

function tabCases() {
  const guildId = state.guildId;
  const handlers = useRef({});
  const [rows, setRows] = useState([]);
  const [detailParts, setDetailParts] = useState([]);
  const [pagination, setPagination] = useState({ first: true, next: true });
  const [canDelete, setCanDelete] = useState(false);
  const [removedView, setRemovedView] = useState(false);
  const [handoff] = useState(() => {
    const next = pendingCase;
    pendingCase = null;
    return next;
  });
  useEffect(() => {
    let disposed = false;
    let request = 0;
    let detailRequest = 0;
    let after = "";
    let removed = false;
    let nextAfter = "";
    const pageSize = 50;
    const openCase = async (id) => {
      const currentDetail = ++detailRequest;
      try {
        const payload = await apiRead(`/api/guilds/${guildId}/cases/${id}`);
        const history = { history: [] };
        let sequence = "0";
        // The existing history contract admits at most 64 entries. Consume both
        // bounded pages so successful notes beyond the first 32 remain visible.
        for (let page = 0; page < 2; page++) {
          const result = await api(`/api/guilds/${guildId}/cases/${id}/history?limit=32&after=${sequence}`);
          const entries = result.history || [];
          history.history.push(...entries);
          if (entries.length < 32) break;
          sequence = entries[entries.length - 1].sequence;
        }
        const evidence = await apiRead(`/api/guilds/${guildId}/cases/${id}/evidence`);
        if (disposed || currentDetail !== detailRequest || state.guildId !== guildId) return;
        const c = payload.case || {};
        let noteIntent = null;
        let notePending = false;
        const noteRef = React.createRef();
        const note = h(Textarea, {
          key: `${id}:${currentDetail}`,
          ref: noteRef,
          rows: "3",
          placeholder: "Add a note. This does not change Discord.",
          className: "textarea",
        });
        setDetailParts([
          h("h3", { className: "card-title" }, "Case " + id),
          h("p", {}, `${c.action || "unknown"} · ${c.outcome || "unknown"} · subject ${c.subject_id || ""}`),
          h("p", { className: "muted" }, c.reason || "No reason recorded."),
          h(
            "p",
            { className: "muted" },
            "Removing a case deletes retained private records. It does not reverse a Discord ban, kick, timeout, or warning.",
          ),
          payload.can_delete
            ? h(
                ActionButton,
                {
                  className: "btn btn-sm standalone-action",
                  onClick: async () => {
                    if (!confirm("Remove this case record? This does not undo the Discord action.")) return;
                    const result = await api(`/api/guilds/${guildId}/cases/${id}`, "DELETE");
                    toast(
                      result?.cleanup_pending
                        ? "Case record removed. Cleanup is pending. Use Removed records / retry cleanup."
                        : "Case record removed",
                      "success",
                    );
                    renderTab();
                  },
                },
                "Remove case record",
              )
            : null,
          h("h3", { className: "card-title" }, "History"),
          ...(history.history || []).map((entry) =>
            h("p", { className: "muted" }, JSON.stringify(entry.entry.body)),
          ),
          h(
            Field,
            { className: "fieldset field" },
            h(Label, { className: "fieldset-legend" }, "Note"),
            note,
            h(
              ActionButton,
              {
                className: "btn btn-sm",
                type: "button",
                onClick: async () => {
                  if (notePending) return;
                  const text = noteRef.current.value.trim();
                  if (!noteIntent || noteIntent.text !== text) {
                    noteIntent = { entry_id: crypto.randomUUID().replaceAll("-", ""), text };
                  }
                  notePending = true;
                  try {
                    await api(`/api/guilds/${guildId}/cases/${id}/history`, "POST", noteIntent);
                    noteIntent = null;
                    toast("Note added", "success");
                    await openCase(id);
                    dirty = false;
                    updateWorkspaceStatus();
                  } finally {
                    notePending = false;
                  }
                },
              },
              "Add note",
            ),
          ),
          h("h3", { className: "card-title" }, "Private evidence"),
          ...(Array.isArray(c.evidence?.text) ? c.evidence.text : []).map((item) =>
            h(
              "div",
              {},
              h(
                "p",
                { className: "muted" },
                `Message ${item.message_id} · channel ${item.channel_id} · author ${item.author_id}`,
              ),
              h("pre", {}, item.text),
            ),
          ),
          ...(evidence.evidence || []).map((item) =>
            h(
              "p",
              {},
              h(
                ActionButton,
                {
                  className: "btn btn-sm",
                  type: "button",
                  onClick: async () => {
                    await downloadPrivate(`/api/guilds/${guildId}/cases/${id}/evidence/${item.id}`, item.id);
                  },
                },
                `Download ${item.id}`,
              ),
            ),
          ),
        ]);
      } catch (e) {
        if (disposed || currentDetail !== detailRequest || state.guildId !== guildId) return;
        setDetailParts([h("p", { className: "err" }, e.message || String(e))]);
      }
    };
    const run = async () => {
      const current = ++request;
      ++detailRequest;
      setDetailParts([]);
      setPagination((value) => ({ ...value, next: true }));
      setRows([h("tr", {}, h("td", { colSpan: 5 }, "Loading…"))]);
      try {
        const query = new URLSearchParams({ limit: String(pageSize), removed: String(removed) });
        if (after) query.set("after", after);
        const payload = await apiRead(`/api/guilds/${guildId}/cases?${query}`);
        if (disposed || current !== request || state.guildId !== guildId) return;
        setCanDelete(!!payload.can_delete);
        const ids = payload.cases || [];
        nextAfter = ids.length === pageSize ? ids[ids.length - 1] : "";
        setPagination({ next: !nextAfter, first: !after });
        if (!ids.length) {
          setRows([
            h(
              "tr",
              {},
              h(
                "td",
                { colSpan: 5, className: "muted" },
                removed ? "No more removed records." : "No selected cases on this page.",
              ),
            ),
          ]);
          return;
        }
        const rows = [];
        for (const id of ids) {
          if (removed) {
            rows.push(
              h(
                "tr",
                {},
                h("td", {}, id),
                h(
                  "td",
                  { colSpan: 4 },
                  h(
                    ActionButton,
                    {
                      className: "btn btn-sm",
                      onClick: async () => {
                        const result = await api(`/api/guilds/${guildId}/cases/${id}`, "DELETE");
                        toast(
                          result.cleanup_pending
                            ? "Cleanup is pending. Retry when storage is available."
                            : "Live record cleanup complete. This does not delete backup copies.",
                          "success",
                        );
                      },
                    },
                    "Retry cleanup",
                  ),
                ),
              ),
            );
            continue;
          }
          const payload = await apiRead(`/api/guilds/${guildId}/cases/${id}`);
          if (disposed || current !== request || state.guildId !== guildId) return;
          const c = payload.case || {};
          rows.push(
            h(
              "tr",
              {},
              h(
                "td",
                {},
                h(ActionButton, { className: "btn btn-sm", onClick: () => openCase(id) }, id.slice(0, 8)),
              ),
              h("td", {}, c.action || ""),
              h("td", {}, c.outcome || ""),
              h("td", {}, String(c.subject_id || "")),
              h(
                "td",
                { className: "muted" },
                c.occurred_at_ms ? new Date(Number(c.occurred_at_ms)).toLocaleString() : "",
              ),
            ),
          );
        }
        setRows(rows);
      } catch (e) {
        if (disposed || current !== request || state.guildId !== guildId) return;
        setRows([
          h(
            "tr",
            {},
            h("td", { colSpan: 5, className: "err" }, "Could not load cases: " + (e.message || String(e))),
          ),
        ]);
      }
    };
    handlers.current = {
      run,
      first: () => {
        after = "";
        run();
      },
      next: () => {
        if (nextAfter) {
          after = nextAfter;
          run();
        }
      },
      recovery: () => {
        removed = !removed;
        setRemovedView(removed);
        after = "";
        run();
      },
    };
    run();
    if (handoff?.guildId === guildId) openCase(handoff.caseId);
    return () => {
      disposed = true;
      ++request;
      ++detailRequest;
    };
  }, [guildId, handoff]);
  return h(
    "div",
    null,
    h("h2", null, "Cases"),
    h(
      "p",
      { className: "muted" },
      "Selected moderation facts for this bot membership. Ordinary messages are not archived. Removing a record does not reverse Discord.",
    ),
    h(
      "p",
      { className: "muted" },
      "Use removed records to retry cleanup. Their private content stays deleted, and retrying does not undo Discord actions.",
    ),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      h(ActionButton, { className: "btn", onClick: () => handlers.current.run?.() }, "Refresh"),
      h(
        ActionButton,
        { className: "btn", disabled: pagination.first, onClick: () => handlers.current.first?.() },
        "First page",
      ),
      h(
        ActionButton,
        { className: "btn", disabled: pagination.next, onClick: () => handlers.current.next?.() },
        "Next page",
      ),
      h(
        ActionButton,
        {
          className: "btn",
          style: { display: canDelete ? "" : "none" },
          onClick: () => handlers.current.recovery?.(),
        },
        removedView ? "Back to cases" : "Removed records / retry cleanup",
      ),
    ),
    h(DataTable, { headings: ["Case", "Action", "Outcome", "Subject", "When"] }, rows),
    h(
      "div",
      { className: "card bg-base-100 card-border", style: { marginTop: "16px" } },
      ...detailParts,
    ),
  );
}

function tabSettings() {
  const cfg = state.config;
  const ov = cfg.settings_override || {};
  const prefixRef = useRef(null);
  const prefix = h(Input, {
    ref: prefixRef,
    type: "text",
    defaultValue: cfg.settings.prefix,
    className: "input",
  });
  const botNameRef = useRef(null);
  const botName = h(Input, {
    ref: botNameRef,
    type: "text",
    defaultValue: cfg.settings.bot_name,
    className: "input",
  });

  const sourceEnrollment = useRef(null);
  const srcSelRef = useRef(null);
  const srcSel = h(
    Select,
    {
      ref: srcSelRef,
      className: "select",
      onChange: (event) =>
        handleEvent(
          event.currentTarget,
          async () => {
            sourceEnrollment.current = null;
            const guildId = srcSelRef.current.value;
            if (!guildId) return;
            const source = await apiRead(`/api/guilds/${guildId}/config`);
            if (srcSelRef.current?.value === guildId && source.enrollment_id) {
              sourceEnrollment.current = { guildId, id: source.enrollment_id };
            }
          },
          event,
        ),
    },
    h("option", { value: "" }, "Select a source server"),
    ...state.guilds.filter((g) => g.id !== state.guildId).map((g) => h("option", { value: g.id }, g.name)),
  );
  const sectionSelRef = useRef(null);
  const sectionSel = h(
    Select,
    { ref: sectionSelRef, className: "select" },
    ["all", "rules", "badwords", "commands", "templates", "settings"].map((s) =>
      h("option", { value: s }, s === "all" ? "everything" : s),
    ),
  );
  const globalPrefixRef = useRef(null);
  const globalPrefix = h(Input, {
    ref: globalPrefixRef,
    type: "text",
    defaultValue: cfg.global_settings.prefix,
    className: "input",
  });

  /** Clear one override field via API null (inherit parent). */
  async function inheritField(field) {
    await reloadAfter(
      () => api(`/api/guilds/${state.guildId}/settings`, "PUT", { [field]: null }),
      () => toast(`Using global ${field.replaceAll("_", " ")}`, "success"),
    );
  }

  function settingsField(labelText, input, overrideKey, hint, allowInheritance = true) {
    const hasOv = ov[overrideKey] != null;
    return h(
      Field,
      { className: "fieldset field" },
      h(Label, { className: "fieldset-legend" }, labelText),
      input,
      hint ? h("p", { className: "muted", style: { margin: "4px 0 0", fontSize: "12px" } }, hint) : null,
      hasOv && allowInheritance
        ? h(
            ActionButton,
            {
              className: "btn btn-sm",
              type: "button",
              savedInputs: [input.props.ref],
              style: { marginTop: "6px" },
              onClick: () => inheritField(overrideKey),
            },
            "Use global",
          )
        : null,
    );
  }

  const cards = [
    h(
      "div",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "This server"),
        cfg.nickname_reconcile?.state === "terminal"
          ? h(
              "p",
              { className: "error" },
              `Discord nickname needs operator action: ${cfg.nickname_reconcile.last_error || cfg.nickname_reconcile.last_error_code || "permanent Discord rejection"}`,
            )
          : cfg.nickname_reconcile?.state === "dormant"
            ? h(
                "p",
                { className: "muted" },
                "Discord nickname work is paused. After confirming the bot is in this server, use Re-sync Discord nickname to submit a fresh request. Work from an earlier membership is never replayed.",
              )
            : null,
        h(
          "div",
          { className: "row form-row" },
          settingsField(
            "Prefix",
            prefix,
            "prefix",
            ov.prefix != null ? "Server override — clear field + Save, or Use global" : null,
          ),
          settingsField(
            "Bot nickname",
            botName,
            "bot_name",
            "Empty clears the Discord nickname. Nicknames are configured per server.",
            false,
          ),
        ),
        h(
          "div",
          { className: "row" },
          h(
            ActionButton,
            {
              className: "btn btn-primary",
              onClick: async () => {
                // Tri-state body: omit unchanged, null clears override, value sets.
                // Only send fields the operator actually changed — always sending
                // bot_name re-applied a stale input when global nick changed elsewhere.
                // bot_name empty is special (AGENTS): persist "" to clear Discord nick — not inherit.
                const body = {};
                const rawPrefix = String(prefixRef.current.value ?? "").trim();
                if (rawPrefix === "") {
                  if (ov.prefix != null) body.prefix = null;
                  else {
                    toast("Prefix cannot be empty (Use global to inherit)", "error");
                    return;
                  }
                } else if (rawPrefix !== cfg.settings.prefix) {
                  body.prefix = rawPrefix;
                }
                if (botNameRef.current.value !== cfg.settings.bot_name) {
                  body.bot_name = botNameRef.current.value;
                }
                if (body.prefix === undefined && body.bot_name === undefined) {
                  toast("No settings changes", "success");
                  return;
                }
                await reloadAfter(
                  () => api(`/api/guilds/${state.guildId}/settings`, "PUT", body),
                  (res) =>
                    toast(
                      res.nickname_queued ? "Settings saved — Discord nickname queued" : "Settings saved",
                      "success",
                    ),
                );
              },
            },
            "Save",
          ),
          h(
            ActionButton,
            {
              className: "btn",
              savedInputs: [],
              onClick: async () => {
                await reloadAfter(
                  () => api(`/api/guilds/${state.guildId}/settings`, "PUT", { sync_nickname: true }),
                  (res) =>
                    toast(
                      res.nickname_queued
                        ? "Discord nickname reconciliation queued"
                        : "No nickname work queued",
                      "success",
                    ),
                );
              },
            },
            "Re-sync Discord nickname",
          ),
        ),
      ),
    ),
  ];

  const roleInputRef = useRef(null);
  const roleInput = h(Input, {
    ref: roleInputRef,
    type: "text",
    placeholder: "role id, <@&id> mention, comma-separated",
    className: "input",
  });
  const roleRevision = useRef("");
  const roleWriteGeneration = useRef(0);
  useEffect(() => {
    let active = true;
    const initialRoleGeneration = roleWriteGeneration.current;
    apiRead(`/api/guilds/${state.guildId}/case-delete-roles`)
      .then((roles) => {
        if (!active || initialRoleGeneration !== roleWriteGeneration.current) return;
        const loadedValue = (roles.roles || []).join(", ");
        if (
          roleInputRef.current._initialValue === undefined ||
          roleInputRef.current.value === roleInputRef.current._initialValue
        ) {
          roleInputRef.current.value = loadedValue;
        }
        roleInputRef.current._initialValue = loadedValue;
        roleRevision.current = roles.revision || "";
      })
      .catch((e) => {
        if (active && !isUnauthenticated(e)) reportApiError(e);
      });
    return () => {
      active = false;
    };
  }, []);
  cards.push(
    h(
      "div",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Case removal"),
        h(
          "p",
          { className: "muted" },
          "Server owners and Administrators can remove case records. Stored role IDs are remembered for a later member-role check; Discord OAuth currently cannot apply them. Removal never undoes a Discord action.",
        ),
        h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Delegated case-removal role IDs"), roleInput),
        h(
          "div",
          { className: "card-actions" },
          h(
            ActionButton,
            {
              className: "btn",
              onClick: async () => {
                const saved = await api(`/api/guilds/${state.guildId}/case-delete-roles`, "PUT", {
                  roles: splitIds(roleInputRef.current.value),
                  revision: roleRevision.current,
                });
                roleWriteGeneration.current++;
                roleRevision.current = saved.revision;
                roleInputRef.current._initialValue = roleInputRef.current.value;
                dirty = currentDraftInputs().length > 0;
                updateWorkspaceStatus();
                toast("Case-removal roles saved", "success");
              },
            },
            "Save case-removal roles",
          ),
        ),
      ),
    ),
  );

  if (isBotOwner()) {
    cards.push(
      h(
        "div",
        { className: "card bg-base-100 card-border" },
        h(
          "div",
          { className: "card-body" },
          h("h3", { className: "card-title" }, "Global defaults (applies to all servers)"),
          h(
            "p",
            { className: "muted", style: { marginBottom: "8px" } },
            "Global settings apply to every server until that server overrides a field. Rules/templates/commands work the same way — choose 'Global' in any editor, or set the global prefix here.",
          ),
          h(
            "div",
            { className: "row form-actions" },
            h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, "Global prefix"), globalPrefix),
            h(
              ActionButton,
              {
                className: "btn",
                onClick: async () => {
                  const raw = String(globalPrefixRef.current.value ?? "").trim();
                  if (raw === "") {
                    toast("Global prefix cannot be empty", "error");
                    return;
                  }
                  await reloadAfter(
                    () => api(`/api/guilds/0/settings`, "PUT", { prefix: raw }),
                    () => toast("Global prefix saved", "success"),
                  );
                },
              },
              "Save global prefix",
            ),
          ),
        ),
      ),
    );
  }

  cards.push(
    h(
      "div",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Copy configuration from another server"),
        h(
          "p",
          { className: "muted", style: { marginBottom: "8px" } },
          "Replace rules, badwords, commands, templates, settings, or everything with another server's configuration. Log channels are server-specific and are not copied. Source must be a server you manage.",
        ),
        h(
          "div",
          { className: "row" },
          srcSel,
          sectionSel,
          h(
            ActionButton,
            {
              className: "btn btn-primary",
              onClick: async () => {
                if (!sourceEnrollment.current || sourceEnrollment.current.guildId !== srcSelRef.current.value)
                  return toast("Select a source server to load its current configuration", "error");
                if (
                  !confirm(
                    `Copy ${sectionSelRef.current.value} from ${srcSelRef.current.selectedOptions[0].textContent}?`,
                  )
                )
                  return;
                await reloadAfter(
                  () =>
                    api(`/api/guilds/${state.guildId}/copy`, "POST", {
                      from_guild_id: String(srcSelRef.current.value),
                      from_enrollment_id: sourceEnrollment.current.id,
                      sections: [sectionSelRef.current.value],
                    }),
                  (res) => {
                    let msg = `Copied: ${(res.copied || []).join(", ") || "(nothing)"}`;
                    if (res.skipped_badwords > 0) {
                      msg += ` (${res.skipped_badwords} invalid badword(s) skipped — not enforced)`;
                    }
                    if (res.skipped_logchannels) {
                      msg += " (log channels skipped — channel IDs are server-specific; set them here)";
                    }
                    if (res.nickname_queued) msg += " (Discord nickname queued)";
                    toast(msg, res.skipped_badwords > 0 || res.skipped_logchannels ? "info" : "success");
                  },
                );
              },
            },
            "Copy",
          ),
        ),
      ),
    ),
  );

  return h("div", {}, h("h2", {}, "Settings"), ...cards);
}

function scopeSelect(
  ref,
  guildLabel = "This server only",
  globalLabel = "Global (all servers)",
  allowGlobal = true,
) {
  return h(
    Select,
    { ref, className: "select" },
    h("option", { value: "guild" }, guildLabel),
    allowGlobal && isBotOwner() && h("option", { value: "global" }, globalLabel),
  );
}
function RuleAction({ action, index, remove }) {
  return h(
    "div",
    { className: "escalation-step" },
    h(
      "div",
      { className: "row title-row" },
      h("strong", null, `Action ${index + 1}`),
      h(
        ActionButton,
        { className: "btn btn-sm btn-outline", "aria-label": `Remove action ${index + 1}`, onClick: remove },
        "Remove",
      ),
    ),
    h(
      "div",
      { className: "escalation-fields" },
      field(
        "Strike count",
        h(Input, {
          className: "input",
          type: "number",
          min: 1,
          defaultValue: action.at,
          onChange: (event) => {
            action.at = Math.max(1, parseInt(event.target.value) || 1);
          },
        }),
      ),
      field(
        "Action",
        h(
          Select,
          {
            className: "select",
            defaultValue: action.action,
            onChange: (event) => {
              action.action = event.target.value;
            },
          },
          ...["log", "delete", "warn", "timeout", "kick", "ban"].map((value) =>
            h("option", { key: value, value }, value),
          ),
        ),
      ),
      field(
        "Timeout (minutes)",
        h(Input, {
          className: "input",
          type: "number",
          min: 0,
          defaultValue: action.duration_minutes ?? 0,
          title: "minutes (timeout)",
          onChange: (event) => {
            action.duration_minutes = parseInt(event.target.value) || 0;
          },
        }),
      ),
      field(
        "Delete history (days)",
        h(Input, {
          className: "input",
          type: "number",
          min: 0,
          max: 7,
          defaultValue: action.delete_days ?? 0,
          title: "days of messages deleted (ban)",
          onChange: (event) => {
            action.delete_days = Math.min(7, Math.max(0, parseInt(event.target.value) || 0));
          },
        }),
      ),
    ),
    field(
      "Reason template",
      h(Input, {
        className: "input",
        type: "text",
        defaultValue: action.reason || "",
        placeholder: "reason template",
        onChange: (event) => {
          action.reason = event.target.value;
        },
      }),
    ),
  );
}

const itemKeys = new WeakMap();
let nextItemKey = 0;
function itemKey(item) {
  if (!itemKeys.has(item)) itemKeys.set(item, ++nextItemKey);
  return itemKeys.get(item);
}
function CommandStep({ step, placeholder, onChange, remove }) {
  const input = useRef(null);
  return h(
    "div",
    { className: "card card-border step-card" },
    h(
      "div",
      { className: "row control-row" },
      h(
        Select,
        {
          className: "select",
          value: step.type,
          "aria-label": "Step action",
          onChange: (event) => {
            const type = event.target.value,
              updated = { type };
            applyPlanStepInput(updated, type, input.current.value);
            if (type === "log") updated.event = "plan_executed";
            Object.keys(step).forEach((key) => delete step[key]);
            Object.assign(step, updated);
            input.current.value =
              type === "delete_command_message"
                ? ""
                : String(
                    step.template ??
                      step.role_id ??
                      step.minutes ??
                      step.delete_days ??
                      step.emoji ??
                      step.secs ??
                      "",
                  );
            onChange();
          },
        },
        ...[
          "reply",
          "dm",
          "log",
          "add_role",
          "remove_role",
          "timeout",
          "kick",
          "ban",
          "delete_command_message",
          "react",
          "wait",
        ].map((value) => h("option", { key: value, value }, value)),
      ),
      h(Input, {
        ref: input,
        "aria-label": "Step value",
        className: "input",
        type: "text",
        defaultValue:
          step.template ?? step.role_id ?? step.minutes ?? step.delete_days ?? step.emoji ?? step.secs ?? "",
        placeholder,
        style: { flex: 1 },
        onChange: (event) => {
          applyPlanStepInput(step, step.type, event.target.value);
          onChange();
        },
      }),
      h(ActionButton, { className: "btn btn-sm btn-error", onClick: remove }, "✕"),
    ),
  );
}

function useApiRead(path) {
  const [revision, setRevision] = useState(0);
  const [result, setResult] = useState({ loading: true, data: null, error: null });
  useEffect(() => {
    const controller = new AbortController();
    let current = true;
    const timeout = setTimeout(() => controller.abort(), 30000);
    setResult({ loading: true, data: null, error: null });
    apiRead(path, controller.signal)
      .then((data) => {
        if (current) setResult({ loading: false, data, error: null });
      })
      .catch((error) => {
        if (current)
          setResult({
            loading: false,
            data: null,
            error: controller.signal.aborted
              ? apiError("Request timed out. Refresh to verify saved state before trying again.")
              : error,
          });
      })
      .finally(() => clearTimeout(timeout));
    return () => {
      current = false;
      clearTimeout(timeout);
      controller.abort();
    };
  }, [path, revision]);
  return { ...result, refresh: () => setRevision((value) => value + 1) };
}
function DataTable({ headings, children }) {
  return h(
    "div",
    {
      className: "table-scroll rounded-box bg-base-100",
      tabIndex: 0,
      role: "region",
      "aria-label": "Scrollable data table",
    },
    h(
      "table",
      { className: "table table-zebra" },
      h("thead", null, h("tr", null, ...headings.map((text) => h("th", { key: text }, text)))),
      h("tbody", null, ...React.Children.toArray(children)),
    ),
  );
}
function tabEvents() {
  const [type, setType] = useState("");
  const [user, setUser] = useState("");
  const input = useRef(null);
  const result = useApiRead(
    `/api/guilds/${state.guildId}/events?limit=100` +
      (type ? `&event=${encodeURIComponent(type)}` : "") +
      (user ? `&user=${encodeURIComponent(user)}` : ""),
  );
  const refresh = () => {
    setUser(parseSnowflake(input.current.value));
    result.refresh();
  };
  return h(
    "div",
    null,
    h("h2", null, "Audit log"),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      h(
        Select,
        {
          className: "select",
          "aria-label": "Event type",
          value: type,
          onChange: (event) => {
            setType(event.target.value);
            setUser(parseSnowflake(input.current.value));
          },
        },
        h("option", { value: "" }, "all types"),
        ...state.config.event_types.map((value) => h("option", { key: value, value }, value)),
      ),
      h(Input, {
        ref: input,
        type: "text",
        className: "input",
        placeholder: "user id",
        onKeyDown: (event) => {
          if (event.key === "Enter") refresh();
        },
      }),
      h(ActionButton, { className: "btn", onClick: refresh }, "Refresh"),
    ),
    h(
      DataTable,
      { headings: ["Time", "Event", "User ID", "Actor ID", "Details"] },
      result.loading || result.error || !result.data?.length
        ? h(
            "tr",
            null,
            h(
              "td",
              { colSpan: 5, className: result.error ? "err" : result.loading ? "" : "muted" },
              result.loading
                ? "Loading…"
                : result.error
                  ? "Could not load records: " + result.error.message
                  : "No matching records.",
            ),
          )
        : result.data.map((record, i) => {
            let data;
            try {
              data = JSON.stringify(JSON.parse(record.data || "{}"), null, 1);
            } catch (_) {
              data = record.data || "";
            }
            return h(
              "tr",
              { key: record.id ?? i },
              h("td", { className: "muted" }, new Date(record.created_at).toLocaleString()),
              h("td", null, record.event_type),
              h("td", { className: "mono" }, record.user_id || "-"),
              h("td", { className: "mono" }, record.actor_id || "-"),
              h("td", { className: "mono", style: { fontSize: "11px" } }, data),
            );
          }),
    ),
  );
}
function tabBans() {
  const guildId = state.guildId;
  const result = useApiRead(`/api/guilds/${guildId}/bans?limit=200`);
  return h(
    "div",
    null,
    h("h2", null, "Bans"),
    h(
      "p",
      { className: "muted", style: { marginBottom: "12px" } },
      "Recorded bans include bot actions and observed Discord bans. Names and moderators appear when known. Request an unban here or unban in Discord. Check Bot actions for queued results.",
    ),
    h(
      DataTable,
      { headings: ["Banned user", "Status", "Moderator", "Reason", "Time", "Actions"] },
      result.loading || result.error || !result.data?.length
        ? h(
            "tr",
            null,
            h(
              "td",
              { colSpan: 6, className: result.error ? "err" : result.loading ? "" : "muted" },
              result.loading
                ? "Loading…"
                : result.error
                  ? "Could not load records: " + result.error.message
                  : "No matching records.",
            ),
          )
        : result.data.map((record, i) =>
            h(
              "tr",
              { key: record.id ?? i },
              h(
                "td",
                null,
                record.user_display_name
                  ? `${record.user_display_name} (${record.user_name ? `${record.user_name}, ` : ""}${record.user_id})`
                  : record.user_name
                    ? `${record.user_name} (${record.user_id})`
                    : record.user_id,
              ),
              h("td", null, record.active ? "banned" : "unbanned"),
              h(
                "td",
                null,
                record.moderator_id
                  ? record.moderator_name
                    ? `${record.moderator_name} (${record.moderator_id})`
                    : record.moderator_id
                  : "Unavailable",
              ),
              h("td", null, record.reason || "Reason unavailable"),
              h("td", { className: "muted" }, new Date(record.created_at).toLocaleString()),
              h(
                "td",
                null,
                record.active &&
                  h(
                    ActionButton,
                    {
                      className: "btn btn-sm",
                      onClick: async () => {
                        if (confirm(`Unban ${record.user_id}?`))
                          await submitBotAction(`/api/guilds/${guildId}/unban`, "POST", {
                            user_id: String(record.user_id),
                            reason: "unbanned via WebUI",
                          });
                      },
                    },
                    "Unban",
                  ),
              ),
            ),
          ),
    ),
  );
}
function tabActions() {
  const guildId = state.guildId;
  const result = useApiRead(`/api/guilds/${guildId}/actions`);
  return h(
    "div",
    null,
    h("h2", null, "Bot actions"),
    h(
      "p",
      { className: "muted" },
      "Latest 50 requests. Queued work requires the bot to be online. Unknown outcomes are not retried automatically; verify the result in Discord. Cancel unstarted work, including claimed requests the bot has not dispatched. Force-cancel a stuck log-channel request that is running or unknown so a new save can run; this does not undo Discord.",
    ),
    h(ActionButton, { className: "btn standalone-action", onClick: result.refresh }, "Refresh"),
    h(
      DataTable,
      { headings: ["Requested", "Action", "Requested by", "Status", "Result", "Controls"] },
      result.loading || result.error || !result.data?.length
        ? h(
            "tr",
            null,
            h(
              "td",
              { colSpan: 6 },
              result.loading
                ? "Loading…"
                : result.error
                  ? "Could not load actions: " + result.error.message
                  : "No bot actions yet.",
            ),
          )
        : result.data.map((job) =>
            h(
              "tr",
              { key: job.id },
              h("td", null, new Date(job.created_at * 1000).toLocaleString()),
              h(
                "td",
                null,
                job.action.kind === "unban"
                  ? `Unban ${job.action.user_id}`
                  : job.action.kind === "member_sync"
                    ? `Member sync: ${job.action.operation}`
                    : `${job.action.event}: ${job.action.channel_id || "clear log channel"}`,
              ),
              h("td", null, job.actor_id),
              h("td", null, job.state === "uncertain" ? "Outcome unknown" : job.state),
              h("td", null, job.outcome || "—"),
              h(
                "td",
                null,
                actionCanCancel(job) &&
                  h(
                    ActionButton,
                    {
                      className: "btn btn-sm",
                      onClick: async () => {
                        if (
                          actionCancelNeedsConfirm(job) &&
                          !confirm(
                            "Clear this stuck log-channel request? This does not change Discord. A new save can then run.",
                          )
                        )
                          return;
                        await api(`/api/guilds/${guildId}/actions/${encodeURIComponent(job.id)}`, "DELETE");
                        result.refresh();
                      },
                    },
                    actionCancelLabel(job),
                  ),
              ),
            ),
          ),
    ),
  );
}

capturePreviewToken();
render();
init();
