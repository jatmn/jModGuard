import { createState, useStore } from "./state.js";
import { Appearance, useAppearance } from "./appearance.js";
import React, { useState, useEffect, useRef, createContext, useContext } from "react";
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
// Activity and edits are independent: an in-flight save may retain other drafts.
/** @typedef {"idle" | "busy"} EditorActivity */
/** @typedef {"clean" | "dirty"} EditorEdits */
const BusyContext = createContext(false);
const ui = createState({
  view: "startup-view",
  startupStatus: "Waiting for the API to finish starting…",
  previewAvailability: "unchecked",
  contentRevision: 0,
  editor: null,
  notification: null,
  activity: "idle",
  edits: "clean",
  copySource: null,
  drafts: {},
  // Saved values that replace a draft control's default; cleared with drafts.
  draftBaselines: {},
  pendingDrafts: null
});
const root = createRoot(document.getElementById("app"));

let toastTimer;

// The shared busy policy belongs to the application; React owns the controls.
const DraftGroup = createContext(null);
function DraftCard({ group, children, ...props }) {
  return h(DraftGroup.Provider, { value: group }, h("div", props, children));
}
function ActionButton({ onClick, savedFields, disabled, ...props }) {
  const working = useContext(BusyContext);
  const group = useContext(DraftGroup);
  return h("button", {
    ...props,
    disabled: working || disabled,
    onClick: onClick && ((event) => handleEvent(onClick, event, savedFields, group)),
  });
}

// Hand-authored 24×24 stroke icons for row actions (no icon-library dependency;
// sized by the .btn-icon rules in frontend/style.css). The accessible name and
// hover tooltip come from the IconButton label.
const ICON_PATHS = {
  edit: ["M17 3l4 4L8 20l-5 1 1-5L17 3z", "M14 6l4 4"],
  remove: [
    "M4 7h16",
    "M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2",
    "M6 7l1 13a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-13",
    "M10 11v7",
    "M14 11v7",
  ],
  unban: ["M7 11V7a5 5 0 0 1 9.9-1", "M5 11h14a2 2 0 0 1 2 2v5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-5a2 2 0 0 1 2-2z", "M12 15v2"],
  cancel: ["M6 6l12 12", "M18 6L6 18"],
};
function Icon({ name }) {
  return h(
    "svg",
    {
      viewBox: "0 0 24 24",
      fill: "none",
      stroke: "currentColor",
      strokeWidth: 2,
      strokeLinecap: "round",
      strokeLinejoin: "round",
      "aria-hidden": "true",
      focusable: "false",
    },
    ICON_PATHS[name].map((d, index) => h("path", { key: index, d })),
  );
}
function IconButton({ icon, label, className = "btn btn-sm btn-square", ...props }) {
  return h(
    ActionButton,
    { className: `${className} btn-icon`, "aria-label": label, title: label, ...props },
    h(Icon, { name: icon }),
  );
}
function useDraftControl({ draftKey, defaultValue, onChange, ...props }) {
  const group = useContext(DraftGroup);
  const working = useContext(BusyContext);
  const { drafts, draftBaselines } = useStore(ui);
  if (!draftKey) return { ...props, defaultValue, onChange, disabled: working || props.disabled };
  const baseline = String(draftBaselines[draftKey] ?? defaultValue ?? "");
  return {
    ...props,
    value: drafts[draftKey]?.value ?? baseline,
    disabled: working || props.disabled,
    onChange: event => {
      const value = event.target.value;
      const next = { ...ui.getSnapshot().drafts };
      if (value === baseline) delete next[draftKey];
      else next[draftKey] = { value, group };
      ui.set({ drafts: next, edits: Object.keys(next).length > 0 ? "dirty" : "clean" });
      onChange?.(event);
    },
  };
}
function Input(props) { return h(HeadlessInput, useDraftControl(props)); }
function Textarea(props) { return h(HeadlessTextarea, useDraftControl(props)); }
function Select(props) { return h(HeadlessSelect, useDraftControl(props)); }
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
  const appearance = useAppearance();
  const display = useStore(ui);

  useEffect(() => {
    const beforeUnload = (event) => {
      if ((ui.getSnapshot().edits === "dirty") || (ui.getSnapshot().activity === "busy")) {
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
    ["known-spam", "⊗", "Known spam"],
    ["known-bad-images", "▧", "Known-bad images"],
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
    { value: (display.activity === "busy") },
    isPreview() &&
      h(
        "div",
        { id: "preview-banner", className: "preview-banner alert alert-soft alert-warning", role: "status" },
        "Preview — changes disabled",
      ),
    display.view === "main-view"
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
                    ui.set({ pendingDrafts: null, drafts: {}, draftBaselines: {}, copySource: null });
                    pendingCase = null;
                    state.guildId = event.target.value;
                    if (!tabAllowed(state.tab)) state.tab = "dashboard";
                    state.config = null;
                    ui.set({ edits: "clean" });
                    persistWorkspace();
                    if (await loadConfig()) renderTab();
                  },
                },
                state.guilds.length
                  ? state.guilds.map((guild) => h("option", { key: guild.id, value: guild.id }, guild.name))
                  : h("option", { value: "" }, "No manageable servers"),
              ),
              state.me?.role_discovery_incomplete && h("p", { className: "muted", role: "status" },
                "Some server access checks could not finish. ",
                h(ActionButton, { className: "btn btn-xs", onClick: async () => {
                  if (canLeave()) await init();
                } }, "Retry server discovery"),
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
                  tabAllowed(tab) &&
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
              h(Appearance, appearance),
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
                  : guildAccess() === "dashboard"
                    ? "Read-only access"
                  : (display.activity === "busy")
                    ? "Working…"
                    : (display.edits === "dirty")
                      ? "Unsaved changes"
                      : "Changes saved with each action",
              ),
            ),
            h(
              "main",
              { id: "content", tabIndex: -1, onChange: trackDraft },
              h(Content, { key: display.contentRevision }),
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
          { id: display.view, className: "welcome-view" },
          h(Appearance, appearance),
          h(
            "div",
            { className: "login-card card bg-base-100" },
            h(
              "div",
              { className: "card-body" },
              h("span", { className: "brand-mark", "aria-hidden": true }, "jG"),
              h(
                "p",
                { className: "eyebrow" },
                display.view === "login-view" ? "JMODGUARD / MODERATION WORKSPACE" : "YOUR COMMUNITY, IN GOOD HANDS",
              ),
              display.view === "login-view"
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
                    h("p", { id: "startup-status", role: "status" }, display.startupStatus),
                    display.previewAvailability === "unavailable" &&
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
    (!display.editor || display.view !== "main-view") && h(Notification),
    display.view === "main-view" &&
      h(
        Dialog,
        {
          open: !!display.editor,
          onClose: () => {
            if (canLeave()) closeOverlay();
          },
          className: "modal modal-open modal-overlay",
          "aria-label": "Configuration editor",
        },
        display.editor,
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
      className: `toast toast-end toast-bottom ${ui.getSnapshot().notification?.kind || ""}`,
      hidden: !ui.getSnapshot().notification,
    },
    ui.getSnapshot().notification && h("div", {
      className: `alert alert-soft ${ui.getSnapshot().notification.kind === "error" ? "alert-error" : ui.getSnapshot().notification.kind === "success" ? "alert-success" : ""}`,
    }, ui.getSnapshot().notification.message),
  );
}
function trackDraft(event) {
  if (
    event.target.matches("input, textarea, select") &&
    !event.target.hasAttribute("data-search") &&
    !["events", "members", "bans"].includes(state.tab)
  )
    markDirty();
}
function Content() {
  const components = {
    dashboard: tabDashboard,
    rules: tabRules,
    badwords: tabBadwords,
    "known-spam": tabKnownSpam,
    "known-bad-images": tabKnownBadImages,
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
      state.me?.role_discovery_incomplete ? "Server discovery incomplete" : "No servers to manage yet",
      state.me?.role_discovery_incomplete
        ? "Some server access checks could not finish. Retry to check your servers again."
        : state.me?.role_scope_missing
        ? "Sign out and sign in again so jModGuard can check your server roles."
        : "Sign in with an account that has Manage Server permission, or a role a server admin granted Web UI access, in a server using jModGuard.",
    );
  if (!state.config)
    return emptyPanel(
      (state.configRead.activity === "loading") ? "Loading your workspace…" : "Configuration unavailable",
      state.configRead.error || "Fetching saved settings for this server.",
      state.configRead.activity !== "loading" &&
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
  return h((tabAllowed(state.tab) && components[state.tab]) || tabDashboard);
}
function renderTab() {
  const pending = ui.getSnapshot().pendingDrafts;
  const matching = pending && pending.guildId === state.guildId && pending.tab === state.tab &&
    (!state.config || pending.enrollmentId === state.config.enrollment_id);
  const drafts = matching ? pending.drafts : {};
  ui.set({
    contentRevision: ui.getSnapshot().contentRevision + 1,
    drafts,
    edits: Object.keys(drafts).length > 0 ? "dirty" : "clean",
    pendingDrafts: matching && !state.config ? pending : null,
  });
}

function setBusy(value) {
  ui.set({ activity: value ? "busy" : "idle" });
}
function showView(id) {
  ui.set({ view: id });
}
function setStartupStatus(text) {
  ui.set({ startupStatus: text });
}
function showPreviewUnavailable() {
  ui.set({ view: "startup-view", previewAvailability: "unavailable", startupStatus: "This preview is no longer signed in. Return to the owner dashboard and launch a new preview." });
}
function toast(message, kind = "info") {
  ui.set({ notification: { message, kind } });
  clearTimeout(toastTimer);
  toastTimer = setTimeout(
    () => {
      ui.set({ notification: null });
    },
    kind === "error" ? 5000 : 2600,
  );
}
function closeOverlay() {
  ui.set({ editor: null, edits: "clean" });
}
function editRule(record) {
  if (isCustomRuleId(record.rule_id)) {
    ui.set({ editor: h(CustomRuleEditor, { record, creating: false }) });
    return;
  }
  ui.set({ editor: h(RuleEditor, { record }) });
}
function editCommand(record) {
  ui.set({ editor: h(CommandEditor, { record }) });
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
  ui.set({ edits: "clean" });
  stopPrivateWatchers();
  state.guildId = null;
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
  configRead: { activity: "idle", error: null },
};

// Request generation fences publication; retained configuration is independent
// of read activity and the most recent read error.
let configRequest = 0;
function ownsConfigRead(request, guildId) {
  return request === configRequest && guildId === state.guildId;
}

const pendingLogChannelActions = new Map();
const logChannelPolls = new Map();
const authLossCleanups = new Set();

const actionReconcileDelays = [1000, 1500, 2500, 5000, 10000, 20000, 40000, 80000, 120000, 150000];

const tabs = [
  "dashboard",
  "rules",
  "badwords",
  "known-spam",
  "known-bad-images",
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
  state.tab = tabs.includes(saved.tab) && tabAllowed(saved.tab) ? saved.tab : "dashboard";
}

function persistWorkspace() {
  try {
    window.localStorage.setItem(workspaceKey(), JSON.stringify({ guildId: state.guildId, tab: state.tab }));
  } catch (_) {
    /* Session still works without storage. */
  }
}

function canLeave() {
  return ui.getSnapshot().activity !== "busy" && (ui.getSnapshot().edits !== "dirty" || confirm("Discard unsaved changes?"));
}

function markDirty() {
  ui.set({ edits: "dirty" });
}

function currentDrafts() {
  return ui.getSnapshot().drafts;
}

function captureDrafts(savedFields, group) {
  return Object.fromEntries(Object.entries(currentDrafts()).filter(([key, draft]) =>
    savedFields ? !savedFields.includes(key) : !group || draft.group !== group));
}

function retainDrafts(guildId, tab, drafts, enrollmentId) {
  if (guildId !== state.guildId || tab !== state.tab) return;
  if (state.config && enrollmentId !== state.config.enrollment_id) return;
  ui.set({ drafts, edits: Object.keys(drafts).length > 0 ? "dirty" : "clean",
    pendingDrafts: state.config ? null : { guildId, tab, drafts, enrollmentId } });
}

async function handleEvent(handler, event, savedFields, group) {
  if ((ui.getSnapshot().activity === "busy")) return;
  const guildId = state.guildId, tab = state.tab, enrollmentId = state.config?.enrollment_id ?? ui.getSnapshot().pendingDrafts?.enrollmentId;
  const revision = ui.getSnapshot().contentRevision;
  const drafts = captureDrafts(savedFields, group);
  try {
    const result = handler(event);
    if (result && typeof result.then === "function") {
      setBusy(true);
      await result;
      if (revision !== ui.getSnapshot().contentRevision) retainDrafts(guildId, tab, drafts, enrollmentId);
    }
  } catch (e) {
    reportApiError(e);
  } finally {
    if ((ui.getSnapshot().activity === "busy")) setBusy(false);
  }
}

function navigate(tab) {
  if (!canLeave()) return false;
  ui.set({ view: "main-view", pendingDrafts: null, drafts: {}, draftBaselines: {}, copySource: null });
  state.tab = tabs.includes(tab) && tabAllowed(tab) ? tab : "dashboard";
  persistWorkspace();
  renderTab();
  return true;
}

function isBotOwner() {
  return !!(state.me && state.me.is_bot_owner);
}

// Web UI access in the selected server: "manage" (owner or Manage Server),
// or a level a server admin mapped to one of the user's Discord roles. The API
// enforces every level; hiding controls here is only presentation.
function guildAccess() {
  return selectedGuild()?.access || "manage";
}

function canEditSettings() {
  return guildAccess() !== "dashboard";
}

function canManageServer() {
  return guildAccess() === "manage";
}

const settingsTabs = ["rules", "badwords", "known-spam", "known-bad-images", "commands", "templates", "logchannels", "settings"];

function tabAllowed(tab) {
  return canEditSettings() || !settingsTabs.includes(tab);
}

let previewCapability = { kind: "absent" };

function capturedPreviewToken() {
  return previewCapability.kind === "captured" ? previewCapability.token : null;
}

function capturePreviewToken() {
  const fragment = new URLSearchParams(location.hash.replace(/^#/, ""));
  const token = fragment.get("preview");
  if (!token) return;
  // Keep an in-memory source of truth before touching browser storage. A
  // blocked sessionStorage must never silently restore the owner's write access.
  previewCapability = { kind: "captured", token };
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
    return window.sessionStorage.getItem("jmodguard_preview") || capturedPreviewToken();
  } catch (_) {
    return capturedPreviewToken();
  }
}

function clearPreviewToken() {
  previewCapability = { kind: "absent" };
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
    err.code === "auth_lost" ||
    msg.includes("discord authorization failed") ||
    msg.includes("not authenticated") ||
    msg === "session expired" ||
    msg.includes("401")
  );
}

function stopPrivateWatchers() {
  for (const dispose of authLossCleanups) dispose();
  for (const key of pendingLogChannelActions.keys()) clearPendingLogChannelEntry(key);
}

function handleAuthLoss(error) {
  if (!isUnauthenticated(error)) return false;
  stopPrivateWatchers();
  if (currentPreviewToken()) {
    showPreviewUnavailable();
    return true;
  }
  if (ui.getSnapshot().view !== "login-view") {
    state.me = null;
    state.guilds = [];
    state.guildId = null;
    showView("login-view");
  }
  return true;
}

function reportApiError(error) {
  if (!handleAuthLoss(error)) toast(error.message || String(error), error.code === "save_pending" ? "info" : "error");
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
    /^\/api\/guilds\/(\d+)\/(settings|rules|badwords|testbadwords|known-spam|known-bad-images|commands|templates|copy|logchannels|logfilters|unban|actions|events|members|member-sync|bans|stats|cases|case-delete-roles|access-roles)(?:\/|\?|$)/,
  );
  if (
    edit &&
    (method !== "GET" ||
      [
        "rules",
        "badwords",
        "known-spam",
        "known-bad-images",
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
        "access-roles",
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

async function api(path, method = "GET", body = null, signal, reconcileRecovered) {
  const save = ["POST", "PUT"].includes(method) &&
    /^\/api\/guilds\/\d+\/(settings|rules|badwords|known-spam|known-bad-images|commands|templates|copy|logfilters|case-delete-roles|access-roles)(?:\/|$)/.test(path) &&
    (method !== "POST" || body !== null);
  if (save) return submitSave(path, method, body, signal, reconcileRecovered);
  return apiRequest(path, method, body, signal);
}

function canonicalSaveBody(value) {
  if (Array.isArray(value)) return value.map(canonicalSaveBody);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.keys(value).sort().map(key => [key, canonicalSaveBody(value[key])]));
  }
  return value;
}

// Failed/pending requests retain identity; only the existing explicit
// expiry or reviewed-unknown paths may discard it.
function saveFailureState(error) {
  if (error.code === "save_expired") return "expired";
  if (error.code === "save_unknown") return "unknown";
  if (error.code === "save_pending") return "pending";
  return "unresolved";
}

async function submitSave(path, method, body, signal, reconcileRecovered) {
  // A remount reads the current role revision while retaining an unresolved
  // role draft. Match that intent by its roles, then look up the captured revision.
  const roleSave = method === "PUT" && /^\/api\/guilds\/\d+\/(case-delete-roles|access-roles)$/.test(path);
  const canonical = canonicalSaveBody(roleSave ? { roles: body.roles } : body);
  const key = `${workspaceKey()}:save:${method}:${path}:${JSON.stringify(canonical)}`;
  const enrollment = pageEnrollment(path, method);
  let raw, saved;
  /** @type {"submit" | "lookup"} */
  let saveMode;
  try {
    raw = window.sessionStorage.getItem(key);
    saveMode = raw !== null ? "lookup" : "submit";
  } catch (_) {
    throw new Error("Browser session storage must be available and valid to safely save changes.");
  }
  // Only a new save needs the initial read. An unresolved save already owns
  // its original revision and must remain lookup-only while that read is pending.
  if (roleSave && saveMode === "submit" && !body.revision) {
    throw new Error("Roles have not loaded yet. Wait for them to load before saving.");
  }
  try {
    saved = raw ? JSON.parse(raw) : {
      id: (crypto.randomUUID() + crypto.randomUUID()).replaceAll("-", ""),
      enrollment,
      ...(roleSave ? { revision: body.revision } : {}),
    };
    window.sessionStorage.setItem(key, JSON.stringify(saved));
  } catch (_) {
    throw new Error("Browser session storage must be available and valid to safely save changes.");
  }
  if (!saved || !/^[0-9a-f]{64}$/.test(saved.id) || saved.enrollment !== enrollment ||
    (roleSave && typeof saved.revision !== "string")) {
    if (confirm("This saved request belongs to an earlier page membership. Check its outcome before clearing it. Clear the saved request without submitting another save?")) {
      window.sessionStorage.removeItem(key);
    }
    throw new Error("Saved request was not submitted. Reload and review this server before saving again.");
  }
  try {
    const capturedBody = roleSave ? { ...body, revision: saved.revision } : body;
    let result = await apiRequest(path, method, capturedBody, signal, {
      "x-jmodguard-save": saved.id,
      "x-jmodguard-save-lookup": saveMode === "lookup" ? "1" : "0",
    });
    // Specialized editors also consume fields outside config_patch. Keep the
    // identity until their current-state read succeeds, so a failed read retries
    // the acknowledgement without submitting another mutation.
    if (saveMode === "lookup" && reconcileRecovered) result = await reconcileRecovered();
    try { window.sessionStorage.removeItem(key); } catch (_) { /* A retained ID only queries its result. */ }
    // The page may already include this commit, or a newer edit. Reconcile
    // current configuration instead of applying a historical patch again.
    return saveMode === "lookup" ? { ...result, config_patch: null } : result;
  } catch (error) {
    // A lookup-only attempt never submits a new save, whether or not the server answered it.
    if (saveMode === "lookup" && error && typeof error === "object") error.saveLookup = true;
    if (saveFailureState(error) === "expired") {
      try { window.sessionStorage.removeItem(key); } catch (_) { /* Keep lookup-only behavior if storage fails. */ }
    }
    if (saveFailureState(error) === "unknown" && confirm("The earlier save may already have completed, but its result is unavailable. Have you checked the current configuration and want to clear this saved request? This will not submit another save.")) {
      window.sessionStorage.removeItem(key);
      throw new Error("Saved request cleared. Review the current configuration before saving again.");
    }
    throw error;
  }
}

async function apiRequest(path, method = "GET", body = null, signal, headers = {}) {
  const opts = credentialedOpts(path, method);
  Object.assign(opts.headers, headers);
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
    if (!res.ok || data?.code === "save_pending") throwApiFailure(data, res.status, parseRetryAfterMs(res.headers.get("retry-after"), Number.MAX_SAFE_INTEGER / 1000));
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

// Receipt progress is distinct from reconciliation of the saved mapping.
function botActionPhase(job) {
  if (["pending", "claimed", "running"].includes(job.state)) return "active";
  if (job.state === "succeeded") return "succeeded";
  if (["failed", "uncertain", "cancelled", "expired"].includes(job.state)) return "terminal";
  return "invalid";
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
    botActionPhase(job) === "invalid"
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
  const waiting = botActionPhase(job) === "active";
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
  pendingLogChannelActions.delete(key);
  if (!entry) return;
  const groupKey = logChannelPollKey(entry.job);
  if (![...pendingLogChannelActions.values()].some(item => logChannelPollKey(item.job) === groupKey)) {
    clearTimeout(logChannelPolls.get(groupKey)?.timer);
    logChannelPolls.delete(groupKey);
  }
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
  ui.set({ pendingDrafts: {
    guildId: state.guildId, tab: state.tab, drafts: currentDrafts(), enrollmentId: state.config?.enrollment_id,
  } });
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
  const active = botActionPhase(job) === "active";
  const terminal = ["succeeded", "terminal"].includes(botActionPhase(job));
  if (!active && !terminal) return false;
  const key = logChannelActionKey(job);
  const existing = pendingLogChannelActions.get(key);
  if (
    existing &&
    existing.job.id !== job.id &&
    terminal &&
    botActionPhase(existing.job) === "active"
  ) {
    // A retained request key can replay an older terminal receipt after a
    // newer request has become this row's active owner. Keep watching newer.
    return true;
  }
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
    };
    pendingLogChannelActions.set(key, entry);
    const poll = logChannelPolls.get(logChannelPollKey(job));
    if (poll && existing?.job.id !== job.id) {
      poll.attempt = 0;
      ++poll.admission;
      poll.dueAt = Math.max(poll.dueAt, Date.now() + actionReconcileDelays[0]);
    }
    row.pending_action = { id: job.id, state: job.state, previous };
  }
  // A snapshot started before this acknowledged request must not replace its
  // queued representation after the button has already returned success.
  ++configRequest;
  state.configRead.activity = "idle";
  state.configRead.error = null;
  return true;
}

function logChannelPollKey(job) {
  return `${job.guild_id}:${job.enrollment_id}`;
}

function logChannelPollVisible(job) {
  return document.visibilityState === "visible" && state.tab === "logchannels" &&
    state.guildId === job.guild_id && state.config?.enrollment_id === job.enrollment_id;
}

function pauseLogChannelPolls(invalidate = false) {
  for (const poll of logChannelPolls.values()) {
    clearTimeout(poll.timer);
    poll.timer = null;
    if (invalidate) ++poll.generation;
  }
}

function resumeLogChannelPolls() {
  for (const { job } of pendingLogChannelActions.values()) scheduleLogChannelReconcile(job);
}

function scheduleLogChannelReconcile(job) {
  const key = logChannelPollKey(job);
  let poll = logChannelPolls.get(key);
  if (!poll) {
    poll = { timer: null, dueAt: Date.now() + actionReconcileDelays[0], retryUntil: 0, attempt: 0, flight: { kind: "idle" }, generation: 0, admission: 0 };
    logChannelPolls.set(key, poll);
  }
  if (poll.timer !== null || poll.flight.kind === "running" || !logChannelPollVisible(job) ||
    poll.attempt >= actionReconcileDelays.length) return;
  poll.timer = setTimeout(() => {
    poll.timer = null;
    if (!logChannelPollVisible(job)) return;
    if (Date.now() < poll.dueAt) return scheduleLogChannelReconcile(job);
    const current = [...pendingLogChannelActions.values()].find(item => logChannelPollKey(item.job) === key);
    if (current) return reconcileQueuedLogChannel(current.job);
  }, Math.min(Math.max(0, poll.dueAt - Date.now()), 2147483647));
}

async function reconcileQueuedLogChannel(original) {
  const entry = pendingLogChannelActions.get(logChannelActionKey(original));
  if (!entry || entry.job.id !== original.id || original.guild_id !== state.guildId ||
    original.enrollment_id !== state.config?.enrollment_id || document.visibilityState !== "visible") return;
  const key = logChannelPollKey(original);
  scheduleLogChannelReconcile(original);
  const poll = logChannelPolls.get(key);
  if (poll.flight.kind === "running") return poll.flight.promise;
  if (Date.now() < poll.retryUntil || ((poll.attempt > 0 || poll.admission > 0) && Date.now() < poll.dueAt)) return;
  clearTimeout(poll.timer);
  poll.timer = null;
  const generation = poll.generation;
  const admission = poll.admission;
  const current = () => logChannelPolls.get(key) === poll && poll.generation === generation;
  poll.flight = { kind: "running", promise: (async () => {
    let retryUntil = 0;
    try {
      const entries = [...pendingLogChannelActions.values()].filter(item => logChannelPollKey(item.job) === key);
      const jobs = entries.some(item => botActionPhase(item.job) === "active")
        ? await apiRead(`/api/guilds/${original.guild_id}/actions`) : [];
      for (const item of entries) {
        if (!current() || document.visibilityState !== "visible") break;
        try {
          await reconcileLogChannelEntry(item.job, jobs, current);
        } catch (error) {
          if (isUnauthenticated(error)) throw error;
          // One unavailable mapping must not strand other terminal receipts.
          retryUntil = Math.max(retryUntil, Date.now() + (error.retryAfterMs || 0));
        }
      }
    } catch (error) {
      if (isUnauthenticated(error)) return;
      retryUntil = Math.max(retryUntil, Date.now() + (error.retryAfterMs || 0));
    } finally {
      poll.flight = { kind: "idle" };
      if (logChannelPolls.get(key) === poll) {
        if (poll.admission === admission) ++poll.attempt;
        poll.retryUntil = retryUntil;
        poll.dueAt = Math.max(Date.now() + (actionReconcileDelays[poll.attempt] || 0), retryUntil);
        if (poll.attempt >= actionReconcileDelays.length && current())
          toast("Could not confirm the log-channel result. See Bot actions for details.", "info");
        else scheduleLogChannelReconcile(original);
      }
    }
  })() };
  return poll.flight.promise;
}

async function reconcileLogChannelEntry(original, jobs, current) {
  const key = logChannelActionKey(original);
  let entry = pendingLogChannelActions.get(key);
  let row = logChannelRow(original.action.event);
  if (
    !current() ||
    !entry ||
    entry.job.id !== original.id ||
    !row?.pending_action ||
    row.pending_action.id !== original.id ||
    original.guild_id !== state.guildId ||
    original.enrollment_id !== state.config?.enrollment_id
  )
    return;
  const originalTerminal = ["succeeded", "terminal"].includes(botActionPhase(original));
  let job = original;
  if (!originalTerminal) {
    job = jobs.find((candidate) => candidate.id === original.id);
    entry = pendingLogChannelActions.get(key);
    row = logChannelRow(original.action.event);
    if (
      !current() ||
      !entry ||
      entry.job.id !== original.id ||
      !row?.pending_action ||
      row.pending_action.id !== original.id ||
      original.guild_id !== state.guildId ||
      original.enrollment_id !== state.config?.enrollment_id
    )
      return;
  }
  if (!job || botActionPhase(job) === "active") {
    if (job) {
      entry.job = job;
      row.pending_action.state = job.state;
    }
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
    if (current()) renderLogChannelsPreservingDrafts();
    throw error;
  }
  entry = pendingLogChannelActions.get(key);
  row = logChannelRow(original.action.event);
  if (
    !current() ||
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
  let warmup = { kind: "unused" };
  let reachability = { kind: "unused" };
  let delay = 500;
  let startupDisplay = "unshown";
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
        reachability = { kind: "unused" };
        if (warmup.kind === "unused") warmup = { kind: "running", startedAt: attemptStarted };
        if (now - warmup.startedAt >= API_WARMUP_BUDGET_MS) {
          throw apiError(originGaveUpMessage(e), e.status || 0, 0);
        }
      } else {
        if (reachability.kind === "unused") reachability = { kind: "running", startedAt: attemptStarted };
        if (now - reachability.startedAt >= ORIGIN_REACHABILITY_BUDGET_MS) {
          throw apiError(originGaveUpMessage(e), e.status || 0, 0);
        }
      }
      if (startupDisplay === "unshown") {
        startupDisplay = "shown";
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
  state.configRead.activity = "loading";
  state.configRead.error = null;
  if (!state.config) renderTab();
  try {
    const config = await apiRead(`/api/guilds/${guildId}/config`);
    if (!ownsConfigRead(request, guildId)) return;
    if (state.config && state.config.enrollment_id !== config.enrollment_id) {
      ui.set({ pendingDrafts: null, drafts: {}, draftBaselines: {}, copySource: null, edits: "clean" });
      for (const [key, entry] of pendingLogChannelActions) {
        if (entry.job.guild_id === guildId && entry.job.enrollment_id !== config.enrollment_id) {
          clearPendingLogChannelEntry(key);
        }
      }
      toast("Bot membership changed. Configuration reloaded; review it before making changes.", "info");
    }
    state.config = config;
    for (const { entry } of overlayPendingLogChannels(config, guildId)) scheduleLogChannelReconcile(entry.job);
    return true;
  } catch (e) {
    if (!ownsConfigRead(request, guildId)) return;
    if (isUnauthenticated(e)) return;
    state.config = null;
    ui.set({ editor: null });
    state.configRead.error = e.message || String(e);
    toast("Could not refresh configuration: " + state.configRead.error, "error");
  } finally {
    if (request === configRequest) {
      state.configRead.activity = "idle";
      if (state.configRead.error) renderTab();
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
  state.configRead.activity = "idle";
  state.configRead.error = null;
  if (patch.kind === "rule") {
    const custom = isCustomRuleId(patch.id);
    if (custom && patch.value == null) {
      config.rules = (config.rules || []).filter((rule) => rule.rule_id !== patch.id);
      return true;
    }
    let row = config.rules?.find((rule) => rule.rule_id === patch.id);
    if (!row && custom && patch.value) {
      config.rules ||= [];
      config.rules.push({
        rule_id: patch.id,
        name: patch.value.name,
        description: patch.value.description || "",
        effective: patch.value,
        guild_override: patch.value,
        global: null,
      });
      return true;
    }
    if (!row || (patch.value == null && (patch.scope === "global" || row.global == null))) return false;
    if (patch.scope === "global") {
      row.global = patch.value;
      if (patch.cleared_guild_override) row.guild_override = null;
      if (!row.guild_override) row.effective = patch.value;
    } else {
      row.guild_override = patch.value;
      row.effective = patch.value ?? row.global;
    }
    if (custom) row.name = row.effective.name;
    return true;
  }
  if (patch.kind === "badword") {
    if (!Array.isArray(config.badwords)) return false;
    if (patch.operation === "add" && patch.value) config.badwords.push(patch.value);
    // Batch acknowledgements contain only IDs. Reload authoritative rows/notes.
    else if (patch.operation === "add_many") return false;
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
      { className: "hero-content" },
      h("div", { className: "hero-copy" }, h("h2", {}, title), h("p", {}, body), action || null),
    ),
  );
}

function field(label, ...children) {
  return h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, label), ...children);
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
        ...[
          ["events", "Moderation records"],
          ["warns", "Warns"],
          ["bans", "Bans"],
          ["cases", "Cases"],
        ].map(([key, label]) =>
          stat(label, result.loading ? "Loading…" : result.data?.[key] == null ? "Unavailable" : String(result.data[key])),
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

function isCustomRuleId(id) {
  const rest = String(id || "").slice("custom:".length);
  return id?.startsWith("custom:") && rest.length >= 8 && rest.length <= 32 && /^[0-9a-f]+$/.test(rest);
}

function newCustomRuleDraft() {
  const hex = crypto.randomUUID().replace(/-/g, "").slice(0, 16);
  return {
    rule_id: `custom:${hex}`,
    name: "Custom rule",
    description: "",
    effective: {
      name: "Custom rule",
      description: "",
      enabled: true,
      dry_run: true,
      params: { kind: "pattern", pattern: "" },
      actions: [{ at: 1, action: "delete", duration_minutes: 0, delete_days: 0, reason: "" }],
      whitelist_roles: [],
      whitelist_channels: [],
      whitelist_users: [],
      skip_moderators: false,
      include_channels: [],
      log_automod: true,
      open_case: true,
    },
    guild_override: null,
    global: null,
  };
}

function tabRules() {
  const rules = state.config.rules.filter((r) => r.rule_id !== "known_spam");
  return h(
    "div",
    {},
    h("h2", {}, "Automod rules"),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      h(
        ActionButton,
        {
          className: "btn btn-primary btn-sm",
          onClick: () => ui.set({ editor: h(CustomRuleEditor, { record: newCustomRuleDraft(), creating: true }) }),
        },
        "Add custom rule",
      ),
    ),
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
  const spam = !isCustomRuleId(r.rule_id) && spamRule(r.rule_id);
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
          h(IconButton, { icon: "edit", label: "Edit", onClick: () => editRule(r) }),
        ),
      ),
      h("p", {}, r.description),
    ),
  );
}

function spamRule(id) {
  return id === "attachment_flood" || id === "repeat_text";
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

function buildCustomRuleSaveBody(rule, creating) {
  const body = JSON.parse(JSON.stringify(rule));
  if (creating) body.dry_run = true;
  return body;
}

function CustomRuleEditor({ record: r, creating }) {
  const [rule] = useState(() => JSON.parse(JSON.stringify(r.effective)));
  const [, bumpForm] = useState(0);
  const kind = rule.params?.kind || "pattern";
  return h(
    DialogPanel,
    { className: "modal-box rule-editor", onInput: trackDraft },
    h("h3", { className: "card-title" }, creating ? "Add custom rule" : `Edit custom rule: ${r.name || r.rule_id}`),
    field("Name", h(Input, { type: "text", defaultValue: rule.name, className: "input", onInput: (e) => (rule.name = e.target.value) })),
    field(
      "Type",
      h(
        Select,
        {
          className: "select",
          defaultValue: kind,
          onChange: (e) => {
            const next = e.target.value;
            const prev = rule.params || {};
            rule.params = { kind: next };
            if (next === "pattern") rule.params.pattern = prev.pattern || "";
            if (next === "word") rule.params.words = Array.isArray(prev.words) ? prev.words : [];
            if (next === "channel_content") {
              rule.params.links = prev.links !== false;
              rule.params.attachments = prev.attachments === true;
            }
            if (next === "honeypot_role") {
              rule.params.role_id = prev.role_id || "";
              const current = rule.actions?.[0]?.action;
              if (!current || current === "delete") {
                rule.actions = [{
                  at: 1,
                  action: "kick",
                  duration_minutes: 0,
                  delete_days: 0,
                  reason: rule.actions?.[0]?.reason || "",
                }];
              }
            }
            markDirty();
            bumpForm((n) => n + 1);
          },
        },
        h("option", { value: "pattern" }, "Pattern (regex)"),
        h("option", { value: "word" }, "Word list"),
        h("option", { value: "channel_content" }, "Channel content"),
        h("option", { value: "everyone_attempt" }, "@everyone / @here attempt"),
        h("option", { value: "honeypot_role" }, "Honeypot role"),
      ),
    ),
    kind === "pattern"
      ? field("Pattern", h(Input, { type: "text", className: "input mono", defaultValue: rule.params.pattern || "", onInput: (e) => (rule.params.pattern = e.target.value) }))
      : null,
    kind === "word"
      ? field("Words (comma-separated)", h(Input, { type: "text", className: "input", defaultValue: (rule.params.words || []).join(","), onInput: (e) => (rule.params.words = e.target.value.split(",").map((w) => w.trim()).filter(Boolean)) }))
      : null,
    kind === "channel_content"
      ? h(
          "div",
          { className: "row checkbox-group" },
          h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.params.links, onChange: (e) => (rule.params.links = e.target.checked) }), "Links"),
          h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.params.attachments, onChange: (e) => (rule.params.attachments = e.target.checked) }), "Attachments"),
        )
      : null,
    kind === "honeypot_role"
      ? h(
          "p",
          { className: "muted" },
          "Default action is kick. For a hidden case, turn on Open a case and turn off Post to automod log. Role id is a snowflake string.",
          field("Role id", h(Input, { type: "text", className: "input mono", defaultValue: String(rule.params.role_id || ""), onInput: (e) => (rule.params.role_id = e.target.value.trim()) })),
        )
      : null,
    field(
      "Action",
      h(
        Select,
        {
          key: `action-${rule.actions?.[0]?.action || "delete"}`,
          className: "select",
          value: rule.actions?.[0]?.action || "delete",
          onChange: (e) => {
            const action = e.target.value;
            const prev = rule.actions?.[0] || {};
            let duration = prev.duration_minutes || 0;
            if (action === "timeout" && !duration) duration = 60;
            rule.actions = [{
              at: 1,
              action,
              duration_minutes: duration,
              delete_days: action === "ban" ? prev.delete_days || 0 : 0,
              purge_last_hour: action === "ban" && prev.purge_last_hour === true,
              reason: prev.reason || "",
            }];
            markDirty();
            bumpForm((n) => n + 1);
          },
        },
        ["delete", "warn", "timeout", "kick", "ban"].map((a) => h("option", { key: a, value: a }, a)),
      ),
    ),
    rule.actions?.[0]?.action === "timeout"
      ? field("Timeout (minutes)", h(Input, { type: "number", className: "input", defaultValue: rule.actions[0].duration_minutes || 60, onInput: (e) => (rule.actions[0].duration_minutes = Number(e.target.value)) }))
      : null,
    rule.actions?.[0]?.action === "ban"
      ? h(BanOptions, { action: rule.actions[0] })
      : null,
    field("Reason template", h(Input, { type: "text", className: "input", defaultValue: rule.actions?.[0]?.reason || "", onInput: (e) => (rule.actions[0].reason = e.target.value) })),
    h(
      "div",
      { className: "row checkbox-group" },
      h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.enabled, onChange: (e) => (rule.enabled = e.target.checked) }), "Enabled"),
      h(
        "label",
        { className: "checkbox-label" },
        h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.dry_run, disabled: creating, onChange: (e) => (rule.dry_run = e.target.checked) }),
        "Dry-run (log only)",
      ),
      h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.log_automod, onChange: (e) => (rule.log_automod = e.target.checked) }), "Post to automod log"),
      h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.open_case, onChange: (e) => (rule.open_case = e.target.checked) }), "Open a case"),
      h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.skip_moderators, onChange: (e) => (rule.skip_moderators = e.target.checked) }), "Skip moderators"),
    ),
    field("Only these channels (empty means all)", h(Input, { type: "text", className: "input", defaultValue: (rule.include_channels || []).join(","), onInput: (e) => (rule.include_channels = splitIds(e.target.value)) })),
    h(
      "div",
      { className: "row form-row" },
      field("Skip roles (ids, comma)", h(Input, { type: "text", className: "input", defaultValue: (rule.whitelist_roles || []).join(","), onInput: (e) => (rule.whitelist_roles = splitIds(e.target.value)) })),
      field("Skip channels (ids)", h(Input, { type: "text", className: "input", defaultValue: (rule.whitelist_channels || []).join(","), onInput: (e) => (rule.whitelist_channels = splitIds(e.target.value)) })),
      field("Skip users (ids)", h(Input, { type: "text", className: "input", defaultValue: (rule.whitelist_users || []).join(","), onInput: (e) => (rule.whitelist_users = splitIds(e.target.value)) })),
    ),
    h(
      "div",
      { className: "modal-action" },
      h(
        ActionButton,
        {
          className: "btn btn-primary",
          onClick: async () => {
            const data = buildCustomRuleSaveBody(rule, creating);
            const saved = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", { scope: "guild", data });
            if (!applyConfigPatch(saved) && !(await loadConfig())) return;
            toast("Rule saved", "success");
            closeOverlay();
            renderTab();
          },
        },
        "Save",
      ),
      !creating
        ? h(
            ActionButton,
            {
              className: "btn btn-error",
              onClick: async () => {
                await reloadAfter(() => api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" }), () => {
                  toast("Rule deleted", "success");
                  closeOverlay();
                });
              },
            },
            "Delete",
          )
        : null,
      h(ActionButton, { className: "btn btn-outline", onClick: () => canLeave() && closeOverlay() }, "Cancel"),
    ),
  );
}

function RuleEditor({ record: r }) {
  const [rule] = useState(() => JSON.parse(JSON.stringify(r.effective)));
  const [, setActionsRevision] = useState(0);
  const renderActions = () => setActionsRevision((value) => value + 1);
  const spam = spamRule(r.rule_id);
  const textRule = r.rule_id === "repeat_text";
  const image = r.rule_id === "attachment_flood";
  const knownSpam = r.rule_id === "known_spam";
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
  const [spamBan] = useState(() => ({
    ...(rule.actions.find((action) => action.action === "ban") || rule.actions[0] || {}),
  }));
  // Inactive legacy image rows should convert to the safe dry-run mode by default.
  const dryRunRef = useRef(null);
  const dryRun = h(Input, {
    ref: dryRunRef,
    type: "checkbox",
    defaultChecked: spam ? !spamConfigured(r.rule_id, rule) || rule.dry_run : rule.dry_run,
    onChange: (e) => (rule.dry_run = e.target.checked),
    className: "checkbox",
  });
  const numberParams = {
    mention_spam: [
      ["window_secs", "Window (seconds)", 10],
      ["max_mentions_in_window", "Maximum mentions in window", 5],
    ],
    new_member_url: [["grace_secs", "Seconds after joining", 120]],
    velocity: [
      ["window_secs", "Window (seconds)", 10],
      ["max_messages", "Maximum messages in window", 8],
    ],
    account_age: [["min_age_days", "Minimum account age (days)", 3]],
  }[r.rule_id] || [];
  const numberParamsRef = useRef({});
  const inviteCodesRef = useRef(null);
  const inviteCodes = (value) => Array.isArray(value)
    ? value.filter((code) => typeof code === "string") : [];
  const detectionFields = numberParams.map(([key, label, fallback]) =>
    h(React.Fragment, { key }, field(label, h(Input, {
      ref: (node) => { numberParamsRef.current[key] = node; },
      type: "number",
      min: key === "window_secs" ? 1 : 0,
      max: key === "window_secs" ? 600 : Number.MAX_SAFE_INTEGER,
      step: 1,
      required: true,
      defaultValue: rule.params?.[key] ?? fallback,
      className: "input",
    }))),
  );
  if (r.rule_id === "invite") {
    detectionFields.push(h(React.Fragment, { key: "invite_codes" }, field("Allowed invite codes (comma-separated)", h(Input, {
      ref: inviteCodesRef,
      type: "text",
      defaultValue: inviteCodes(rule.params?.allowlist_codes).join(", "),
      placeholder: "code-one, code-two",
      className: "input",
    }))));
  }

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
  const skipModRef = useRef(null);
  const skipMod = h(Input, {
    ref: skipModRef,
    type: "checkbox",
    defaultChecked: rule.skip_moderators,
    onChange: (e) => (rule.skip_moderators = e.target.checked),
    className: "checkbox",
  });
  const includeChannelsRef = useRef(null);
  const includeChannels = h(Input, {
    ref: includeChannelsRef,
    type: "text",
    defaultValue: (rule.include_channels || []).join(","),
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
          field("Distinct channels", imageChannels),
          image
            ? field("Common distinct images per message", imageCount)
            : null,
          textRule
            ? field("Minimum length (Unicode scalars after trim)", textLength)
            : null,
          field("Window (seconds)", imageWindow),
          h(BanOptions, { action: spamBan }),
        )
      : r.rule_id === "badword"
        ? h("p", { className: "field-help" }, "Manage words and advanced regex in Badwords. These settings apply to that saved list.")
        : knownSpam
          ? h("p", { className: "field-help" }, "First match: Ban and purge the previous hour (3600 seconds). No strike ladder. Manage samples and URLs on Known spam.")
          : detectionFields,
    spam || knownSpam
      ? null
      : field(
          "Action escalation",
          h(
            "p",
            { className: "field-help" },
            "Strike count is the threshold for an action. Timeout minutes apply to timeout. Choose Ban or Ban and purge for each ban action.",
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
      field("Whitelist roles (ids, comma)", whitelistText),
      field("Whitelist channels (ids)", channelsText),
      field("Whitelist users (ids)", usersText),
    ),
    h(
      "div",
      { className: "row checkbox-group" },
      h("label", { className: "checkbox-label" }, skipMod, "Skip moderators"),
      field("Only these channels (empty means all)", includeChannels),
    ),
    h(
      "div",
      { className: "modal-action" },
      field("Applies to", scopeSel),
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
                      { at: 1, action: "ban", reason: "spam match", duration_minutes: 0, delete_days: spamBan.delete_days || 0, purge_last_hour: spamBan.purge_last_hour === true },
                    ]
                  : [
                      {
                        at: 1,
                        action: "log",
                        reason: textRule ? "Repeated text test" : "Exact image spam test",
                        duration_minutes: 0,
                        delete_days: spamBan.delete_days || 0,
                        purge_last_hour: spamBan.purge_last_hour === true,
                      },
                    ];
                if (!spamConfigured(r.rule_id, rule))
                  throw new Error(
                    "Channels must be 2–10 and window 1–600 seconds, all whole numbers. Images 1–10. Text length 1–2000.",
                  );
              } else if (r.rule_id !== "badword" && !knownSpam) {
                const params = {};
                for (const [key, label] of numberParams) {
                  const input = numberParamsRef.current[key];
                  const value = Number(input.value);
                  if (input.value.trim() === "" || !Number.isSafeInteger(value) || value < 0)
                    throw new Error(`${label} must be a non-negative whole number.`);
                  if (key === "window_secs" && (value < 1 || value > 600))
                    throw new Error("Window must be 1–600 seconds.");
                  params[key] = value;
                }
                if (r.rule_id === "invite") {
                  // Legacy allowlist_guilds was never read by the bot. Do not
                  // reinterpret those guild IDs as invite codes on edit.
                  params.allowlist_codes = inviteCodes(inviteCodesRef.current.value
                    .split(",")).map((code) => code.trim()).filter(Boolean);
                }
                rule.params = params;
              }
            } catch (e) {
              toast("Invalid detection settings: " + e.message, "error");
              return;
            }
            rule.whitelist_roles = splitIds(whitelistTextRef.current.value);
            rule.whitelist_channels = splitIds(channelsTextRef.current.value);
            rule.whitelist_users = splitIds(usersTextRef.current.value);
            rule.skip_moderators = skipModRef.current?.checked ?? rule.skip_moderators;
            rule.include_channels = splitIds(includeChannelsRef.current?.value || "");
            const scope = scopeSelRef.current.value;
            const saved = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", {
              scope,
              data: rule,
              clear_guild_override: scope === "global" && state.guildId !== "0",
            });
            const savedLocally = applyConfigPatch(saved);
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
  const { drafts } = useStore(ui);
  const mode = drafts["badwords:mode"]?.value || "words";
  const badwordRule = state.config.rules.find((rule) => rule.rule_id === "badword");
  const patternRef = useRef(null);
  const pattern = h(Textarea, {
    ref: patternRef,
    draftKey: "badwords:pattern",
    rows: 3,
    maxLength: mode === "regex" ? 512 : 4096,
    "aria-label": mode === "regex" ? "Advanced regex pattern" : "Words or phrases",
    placeholder: mode === "regex" ? "Advanced regex, e.g. (scam|pump and dump)" : "Words or phrases, separated by commas or newlines",
    style: { flex: "2" },
    className: "textarea",
  });
  const noteRef = useRef(null);
  const note = h(Input, {
    ref: noteRef,
    draftKey: "badwords:note",
    type: "text",
    placeholder: "note (optional)",
    style: { flex: "1" },
    className: "input",
  });
  const scopeRef = useRef(null);
  const scope = scopeSelect(scopeRef, "This server", "Global", true, "badwords:scope");
  const sampleRef = useRef(null);
  const sample = h(Input, {
    ref: sampleRef,
    draftKey: "badwords:sample",
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
      "Add ordinary words or phrases. Whole-word matching and bypass resistance are automatic, including case, accents, zero-width characters, spacing and supported leetspeak. " +
        "Add up to 32 entries at once, up to 200 bytes each (compiled pattern limit 512 bytes). Advanced regex keeps its own matching semantics. " +
        (isBotOwner()
          ? "Global patterns apply to every server."
          : "Global patterns (shown below) are enforced for all servers; only the bot owner can edit them."),
    ),
    badwordRule
      ? h(
          "div",
          { className: "card bg-base-100 card-border" },
          h(
            "div",
            { className: "card-body" },
            h("h3", { className: "card-title" }, "What happens on match"),
            h(
              "p",
              {},
              `${badwordRule.effective.enabled ? "Enabled" : "Disabled"} · ${badwordRule.effective.dry_run ? "Dry-run (log only)" : "Enforcing"}`,
            ),
            h(
              ActionButton,
              { className: "btn", onClick: () => editRule(badwordRule) },
              "Actions and exemptions",
            ),
          ),
        )
      : null,
    h(
      DraftCard,
      { group: "badwords", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h(
          "div",
          { className: "row" },
          field(
            "Entry type",
            h(
              Select,
              { draftKey: "badwords:mode", defaultValue: "words", className: "select" },
              h("option", { value: "words" }, "Words or phrases"),
              h("option", { value: "regex" }, "Advanced regex"),
            ),
          ),
          pattern,
          note,
          scope,
          h(
            ActionButton,
            {
              className: "btn btn-primary",
              onClick: async () => {
                if (!patternRef.current.value.trim()) return toast("Enter words, phrases or an advanced regex", "error");
                await reloadAfter(
                  () => api(`/api/guilds/${state.guildId}/badwords`, "POST", {
                    scope: scopeRef.current.value,
                    [mode === "regex" ? "pattern" : "words"]: patternRef.current.value.trim(),
                    note: noteRef.current.value || null,
                  }),
                  () => toast("Badwords saved", "success"),
                );
              },
            },
            "Add",
          ),
        ),
      ),
    ),
    h(
      DraftCard,
      { group: "badwords-test", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Live tester"),
        h("p", { className: "field-help" }, "Tests the saved server and global list plus the draft above. Detection only. Enable, dry-run and exemptions determine enforcement."),
        h(
          "div",
          { className: "row" },
          sample,
          h(
            ActionButton,
            {
              className: "btn",
              onClick: async () => {
                if (!sampleRef.current.value) return toast("Enter sample text", "error");
                const draft = patternRef.current.value.trim();
                const res = await api(`/api/guilds/${state.guildId}/testbadwords`, "POST", {
                  sample: sampleRef.current.value,
                  ...(draft ? { [mode === "regex" ? "pattern" : "words"]: draft } : {}),
                });
                const summary = res.matched
                  ? `Matched ${res.matches.map((match) => `${match.label} (${match.source})`).join(", ")}`
                  : "No match in the saved list or draft.";
                setResult(summary + (res.invalid_patterns ? `\nSkipped ${res.invalid_patterns} invalid saved patterns.` : ""));
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
        h("h3", { className: "card-title" }, `Words and patterns (${words.length})`),
        words.length === 0
          ? h("p", { className: "muted" }, "No words or patterns yet.")
          : h(
              DataTable,
              { headings: ["Scope", "Word, phrase or advanced regex", "Note", "Status", ""] },
              words.map((w) => {
                const isGlobal = String(w.guild_id) === "0";
                const canDelete = !isGlobal || isBotOwner();
                const invalid = w.valid === false;
                return h(
                  "tr",
                  { key: w.id },
                  h("td", {}, isGlobal ? "global" : "server"),
                  h("td", { className: "mono" }, w.word || `Regex · ${w.pattern}`),
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
                      : h("span", { className: "badge badge-soft badge-sm badge-success" }, "valid"),
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
      canManageServer() &&
        h(
          ActionButton,
          { className: "btn btn-primary", onClick: () => editCommand(null) },
          "+ New custom command",
        ),
    ),
    !canManageServer() &&
      h("p", { className: "muted" }, "Editing commands needs Manage Server: command plans can grant roles and run moderation actions."),
    h(
      "p",
      { className: "muted", style: { margin: "6px 0 14px" } },
      "Every command lives in config: rename, reword responses, change permissions or cooldowns. " +
        "Custom commands run multi-step action plans (reply, dm, log, roles, timeout, kick, ban…). " +
        "Enabled commands are synced to Discord as slash commands.",
    ),
    h(
      DataTable,
      { headings: ["Command", "Category", "Description", "Permission", "State", ""], className: "commands-table table" },
      cmds.map((c) => {
        const d = c.effective;
        return h(
          "tr",
          { key: c.name },
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
            canManageServer() && h(IconButton, { icon: "edit", label: "Edit", onClick: () => editCommand(c) }),
          ),
        );
      }),
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
  const [, setStepsRevision] = useState(0);
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
  return h(
    DialogPanel,
    { className: "modal-box", onInput: trackDraft },
    h("h3", { className: "card-title" }, isNew ? "New custom command" : `Edit command: ${c.name}`),
    h(
      "div",
      { className: "row form-row" },
      field("Name", nameIn),
      field("Aliases", aliasesIn),
      field("Permission", permIn),
    ),
    field("Description", descIn),
    field("Usage", usageIn),
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
    field("Behavior (advanced JSON)", behaviorArea),
    field("Response template (in-chat reply; {target} {reason} {response} …)", respIn),
    h(
      "div",
      { className: "row form-row" },
      field("Cooldown (secs)", cdIn),
      field("Role restriction (ids)", roleIn),
      field("Channel restriction (ids)", chanIn),
      field("Log event", logIn),
    ),
    h(
      "div",
      { className: "modal-action" },
      field("Applies to", scopeSel),
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
        "Templates render into the configured log channels. Basic message reports supply {message_id} and {message_url}; updates also supply {after} and {attachments}. A single-message delete may supply {content} and author fields from a bounded recent in-process snippet when this process observed the create or content update; otherwise those values are unavailable. This is not persistent history or a Discord history lookup. Other event variables include {user} {mention} {user_id} {guild} {channel} {actor} {reason} {rule} {action} {content} {count} {account_age} {duration} {time}. Activity reports also supply {added}, {removed}, {role}, {role_id}, {role_name}, {detail}, {before_global}, {after_global}, {thread}, {thread_id}, {thread_name}, {notice}, {archived}, {locked}, {auto_archive}, and {invitable} when that event has them. They do not supply message text. Custom templates keep their wording. Log posts use colored embeds. Shipped defaults become fields; customized text fills the description. The embed upgrade updates exact previous defaults once; dates include whole seconds and UTC. The preview uses sample data and the same embed limits.",
      ),
    ),
    ...state.config.templates.map((t) => h(TemplateCard, { key: t.event, record: t })),
  );
}

// Same UTF-16 budget and substitution order as the shared log renderer.
function clipEmbedText(text, limit) {
  if (text.length <= limit) return text;
  if (!limit) return "";
  let out = "";
  for (const ch of text) {
    if (out.length + ch.length > limit - 1) break;
    out += ch;
  }
  return out + "…";
}

function renderEmbedText(template, vars, limit) {
  let out = "";
  for (const chunk of template.match(/[^}]*}|[^}]+$/g) || []) {
    const start = chunk.endsWith("}") ? chunk.lastIndexOf("{") : -1;
    const key = start >= 0 ? chunk.slice(start + 1, -1) : "";
    const value = Object.hasOwn(vars, key) ? vars[key] : undefined;
    for (const text of value === undefined ? [chunk] : [chunk.slice(0, start), value]) {
      for (const ch of text) {
        if (out.length + ch.length > limit) {
          if (!limit) return "";
          // Rust pops the last scalar only when the budget is exhausted.
          if (out.length === limit) out = Array.from(out).slice(0, -1).join("");
          return out + "…";
        }
        out += ch;
      }
    }
  }
  return out;
}

function templateEmbedPreview(record, template, vars) {
  const style = record.embed_style || { title: "Log event", color: 0xffb347 };
  const action = (vars.action || "").trim().toLowerCase();
  const color = style.removal_color && vars.removed && vars.removed !== "none"
    ? style.removal_color : style.action_colors?.[action] ?? style.color;
  const title = clipEmbedText(style.title, 256);
  let left = 6000 - title.length;
  const fields = [];
  let description = "";
  if (template === record.shipped) {
    for (const line of template.split("\n").slice(1)) {
      const match = line.match(/^\*\*(.*?):\*\* (.*)$/);
      if (!match || fields.length === 25 || left < 2) continue;
      const name = clipEmbedText(match[1], Math.min(256, left - 1));
      left -= name.length;
      const value = clipEmbedText(renderEmbedText(match[2], vars, 1024) || "—", Math.min(1024, left));
      left -= value.length;
      fields.push({ name, value });
    }
  } else {
    description = renderEmbedText(template, vars, Math.min(4096, left));
  }
  return { title, color, description, fields };
}

function logPreviewText(text) {
  return previewMarkdown(text);
}

function LogEmbedPreview({ embed }) {
  return h("div", {
    className: "log-embed",
    style: { borderLeftColor: `#${embed.color.toString(16).padStart(6, "0")}` },
  },
  h("strong", { className: "log-embed-title" }, embed.title),
  embed.description ? h("div", { className: "log-embed-description discord-markdown" }, logPreviewText(embed.description)) : null,
  ...embed.fields.map((field, key) => h("div", { className: "log-embed-field", key },
    h("strong", {}, field.name), h("div", { className: "discord-markdown" }, logPreviewText(field.value)),
  )));
}

function TemplateCard({ record: t }) {
  const textRef = useRef(null);
  const text = h(Textarea, {
    ref: textRef,
    draftKey: `template:${t.event}:text`,
    className: "mono template-input textarea",
    rows: 8,
    spellCheck: false,
    defaultValue: t.effective,
  });
  const template = ui.getSnapshot().drafts[`template:${t.event}:text`]?.value ?? t.effective;
  const preview = templateEmbedPreview(t, template, {
    user: "Alice", user_id: "123456789012345678", mention: "@alice", channel: "#general",
    actor: "Bob (mod)", reason: "spam", rule: "mention_spam", action: t.event === "role_change" ? "updated" : "ban",
    content: "FREE NITRO https://example.test", count: "3", removed: "none", added: "@Member",
    before: "Previous text", after: "Updated text", attachments: "none", coverage: "retained snapshot",
    message_url: "https://discord.com/channels/1/2/3", time: "Nov 09, 2026 • 22:35:54 UTC",
    account_created: "Nov 01, 2026 • 10:00:00 UTC", account_age: "1w 1d", duration: "1d",
    joined: "Nov 08, 2026 • 22:35:54 UTC", cases: "Case #1: warning", notice: t.event === "thread_create" ? "created" : "Prior moderation",
    retained: "2", unavailable: "1", unknown: "0", filtered: "0", omitted: "0",
    details: "transcript attached", rule_id: "mention_spam", command: "example",
    role: "@Member", role_id: "123456789012345679", role_name: "Member", detail: "Permissions updated",
    before_global: "Alice", after_global: "Alice Example", thread: "#discussion",
    thread_id: "123456789012345680", thread_name: "Discussion", archived: "false", locked: "false",
    auto_archive: "1d", invitable: "true",
  });
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef, "This server", "Global", true, `template:${t.event}:scope`);
  return h(
    DraftCard,
    { group: `template:${t.event}`, className: "card bg-base-100 card-border template-card" },
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
          h(LogEmbedPreview, { embed: preview }),
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
  useEffect(() => {
    const guildId = state.guildId, enrollmentId = state.config?.enrollment_id;
    const visibility = () => {
      pauseLogChannelPolls(document.visibilityState !== "visible");
      resumeLogChannelPolls();
    };
    document.addEventListener("visibilitychange", visibility);
    resumeLogChannelPolls();
    return () => {
      document.removeEventListener("visibilitychange", visibility);
      pauseLogChannelPolls(state.tab !== "logchannels" || state.guildId !== guildId ||
        state.config?.enrollment_id !== enrollmentId);
    };
  }, [state.guildId, state.config?.enrollment_id]);
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
  const enrollmentId = state.config.enrollment_id;
  const inputRef = useRef(null);
  const input = h(Input, {
    ref: inputRef,
    draftKey: `log:${lc.event}:channel`,
    type: "text",
    defaultValue: lc.effective && String(lc.effective) !== "0" ? String(lc.effective) : "",
    placeholder: "channel id",
    className: "input",
  });
  const ignoredUsersRef = useRef(null);
  const ignoredUsers = h(Textarea, {
    ref: ignoredUsersRef,
    draftKey: `log:${lc.event}:users`,
    placeholder: "One Discord user or bot ID per line",
    rows: 2,
    className: "textarea",
    defaultValue: (lc.ignore?.users || []).join("\n"),
  });
  const ignoredChannelsRef = useRef(null);
  const ignoredChannels = h(Textarea, {
    ref: ignoredChannelsRef,
    draftKey: `log:${lc.event}:channels`,
    placeholder: "One source channel ID per line",
    rows: 2,
    className: "textarea",
    defaultValue: (lc.ignore?.channels || []).join("\n"),
  });
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef, "This server", "Global", false);
  return h(
    DraftCard,
    { group: `log:${lc.event}`, className: "card bg-base-100 card-border log-channel-card" },
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
              savedFields: [`log:${lc.event}:channel`],
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
              savedFields: [`log:${lc.event}:channel`],
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
            savedFields: [`log:${lc.event}:users`, `log:${lc.event}:channels`],
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
              }, undefined, async () => {
                const config = await apiRead(`/api/guilds/${guildId}/config`);
                if (config.enrollment_id !== enrollmentId) {
                  throw new Error("Bot membership changed. Reload and review this server before saving again.");
                }
                return { ignore: config.log_channels.find(row => row.event === lc.event).ignore };
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
  if (!job || (actionIsLogChannel(job) && !canEditSettings())) return false;
  if (job.state === "pending" || job.state === "claimed") return true;
  return actionIsLogChannel(job) && (job.state === "running" || job.state === "uncertain");
}

function actionCancelLabel(job) {
  return job && (job.state === "running" || job.state === "uncertain") ? "Force cancel" : "Cancel";
}

function actionCancelNeedsConfirm(job) {
  return actionIsLogChannel(job) && (job.state === "running" || job.state === "uncertain");
}

// Gap reasons are stored by the bot. Connection reasons get a label; anything
// else is a member or ban change the bot could not save.
const MEMBER_CONNECTION_GAPS = {
  "bot restarted": "planned bot restart",
  "bot stopped unexpectedly": "the bot stopped unexpectedly",
  "gateway continuity interrupted": "Discord connection was interrupted",
  "guild gateway unavailable": "Discord reported the server as unavailable",
};

function memberGapCause(reason, when) {
  const label = Object.hasOwn(MEMBER_CONNECTION_GAPS, reason || "") ? MEMBER_CONNECTION_GAPS[reason] : null;
  return label
    ? `The bot was offline or reconnected${when} (${label})`
    : `The bot could not save a member or ban change${when}`;
}

function memberTime(value) {
  if (value === null || value === undefined || value === "") return "Unknown";
  const date = new Date(typeof value === "string" && !/^\d+$/.test(value) ? value : Number(value));
  return Number.isNaN(date.getTime()) ? "Unknown" : date.toLocaleString(undefined, { timeZoneName: "short" });
}

const memberSyncStatusLabels = {
  queued: "Queued",
  running: "Running",
  waiting: "Waiting",
  paused: "Paused",
  needs_attention: "Needs attention",
  complete: "Complete",
};

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
      lifecycle = "active",
      after = "",
      nextAfter = null;
    let latestStatus = null,
      latestStatusRevision = -1,
      statusRead = "current",
      lastStatusError = null;
    let latestStatusReceivedAt = 0;
    let pollAttempt = 0, pollDueAt = 0;
    let syncControl = { kind: "idle" };
    let syncActionOutcome = null;
    const controlPending = () => syncControl.kind !== "idle";
    const observeControl = (job, operation) => {
      syncControl = {
        kind: job.state === "succeeded" ? "awaiting-status" : "awaiting-receipt",
        job: { id: job.id, operation, state: job.state },
      };
    };
    let emptyMembersCell = null;
    const emptyMembersText = () =>
      latestStatus?.status === "complete"
        ? "No members match these filters."
        : "No saved members match yet. Import may still be incomplete.";
    const aliveHere = () => lifecycle === "active" && alive();
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
      if (!aliveHere()) return;
      const active =
        statusRead === "retry-needed" ||
        controlPending() ||
        (latestStatus &&
          (["queued", "running", "waiting"].includes(latestStatus.status) ||
            ["queued", "running", "waiting"].includes(latestStatus.audit_recovery_status)));
      if (!active) {
        pollDueAt = 0;
        return;
      }
      if (!pollDueAt) {
        let delay = Math.min(10000 * 2 ** Math.min(pollAttempt, 5), 300000);
        const statusNow = Number(latestStatus?.as_of) || latestStatusReceivedAt;
        const elapsed = Date.now() - latestStatusReceivedAt;
        const attempts = [];
        if (["queued", "running", "waiting"].includes(latestStatus?.status))
          attempts.push(Number(latestStatus.next_attempt_at) || 0);
        if (["queued", "running", "waiting"].includes(latestStatus?.audit_recovery_status))
          attempts.push(Number(latestStatus.audit_recovery_next_attempt_at) || 0);
        // A queued control can change the schedule before the worker's next page.
        if (attempts.length && !controlPending())
          delay = Math.max(delay, Math.min(...attempts) - statusNow - elapsed);
        delay = Math.max(delay, lastStatusError?.retryAfterMs || 0);
        pollDueAt = Date.now() + delay;
      }
      if (document.visibilityState !== "visible") return;
      timer = setTimeout(() => {
        timer = null;
        if (Date.now() < pollDueAt) return schedulePoll();
        pollDueAt = 0;
        ++pollAttempt;
        loadStatus();
      }, Math.min(Math.max(0, pollDueAt - Date.now()), 2147483647));
    };
    const asDate = (value) => memberTime(value);
    const renderStatus = (sync) => {
      clearCountdown();
      const parts = [];
      const title = sync.run_kind === "reconcile" ? "Reconcile from Discord" : "Initial member import";
      parts.push(h("h3", { className: "card-title" }, title));
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
      const statusLabel = memberSyncStatusLabels[sync.status || "needs_attention"] || String(sync.status);
      const statusTone =
        { complete: "badge-success", needs_attention: "badge-error", paused: "badge-warning", waiting: "badge-warning" }[
          sync.status
        ] || "badge-info";
      parts.push(
        h(
          "p",
          { role: "status", className: "row" },
          h("span", { className: `badge badge-soft ${statusTone}` }, statusLabel),
          sync.status !== "complete" && currentPhase >= 0
            ? h("span", null, `Stage: ${phaseLabels[phases[currentPhase]]}`)
            : null,
          sync.status_reason ? h("span", { className: "muted" }, sync.status_reason) : null,
        ),
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
            { className: "alert alert-soft alert-warning" },
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
      // Local case history records only completion, never a committed count.
      const phaseNotCounted = (phase) => phase === "local_history";
      const processed = Number(sync.processed ?? sync.committed_count ?? sync.committed ?? 0);
      const currentPhaseKey = phases[currentPhase];
      const phaseProcessed = phaseCounts[currentPhaseKey] == null ? null : Number(phaseCounts[currentPhaseKey]);
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
              ? phaseNotCounted(currentPhaseKey)
                ? `${phaseLabels[currentPhaseKey]} stage; records not counted`
                : `${phaseProcessed == null ? "Unknown number of" : phaseProcessed} records committed in current phase; total unknown`
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
          DataTable,
          { headings: ["Phase", "State", "Committed"], className: "table table-sm" },
          phases.map((phase, index) =>
            h(
              "tr",
              { key: phase },
              h("td", null, phaseLabels[phase]),
              h(
                "td",
                null,
                sync.status === "complete" || (currentPhase >= 0 && index < currentPhase)
                  ? "Done"
                  : index === currentPhase
                    ? statusLabel
                    : "Pending",
              ),
              h(
                "td",
                null,
                phaseNotCounted(phase)
                  ? "Not counted"
                  : phaseCounts[phase] == null
                    ? "Unknown"
                    : Number(phaseCounts[phase]).toLocaleString(),
              ),
            ),
          ),
          h(
            "tr",
            { key: "summary" },
            h("th", { scope: "row" }, "Total"),
            h(
              "td",
              { colSpan: 2 },
              `${Number.isFinite(processed) ? processed.toLocaleString() : "0"} records committed in this run · ${totalDescription}${progressText}`,
            ),
          ),
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
      if (sync.status !== "complete") {
        parts.push(
          h(
            "p",
            { className: "muted" },
            `Elapsed: ${elapsed == null ? "Unknown" : `${Math.round(Number(elapsed) / 60)} minutes`} · Remaining: ${remaining} · Expected finish: ${finish}`,
          ),
        );
      } else if (sync.started_at && Number(sync.last_success_at) >= Number(sync.started_at)) {
        // A completed run records its finish as last_success_at; never measure it against now.
        const took = (Number(sync.last_success_at) - Number(sync.started_at)) / 60000;
        parts.push(h("p", { className: "muted" }, `Took ${Math.round(took)} minutes`));
      }
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
            statusRead !== "retry-needed" &&
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
      const details = [];
      if (sync.initial_completed_at_ms || sync.initial_completed_at)
        details.push(["Initial import completed", asDate(sync.initial_completed_at_ms ?? sync.initial_completed_at)]);
      if (sync.last_success_at) {
        const counts = [
          ["processed", "processed"],
          ["inserted", "inserted"],
          ["updated", "updated"],
          ["unchanged", "unchanged"],
        ]
          .filter(([field]) => sync[`last_success_${field}`] != null)
          .map(([field, label]) => `${Number(sync[`last_success_${field}`]).toLocaleString()} ${label}`);
        details.push([
          "Last successful run",
          `${asDate(sync.last_success_at)}${counts.length ? ` · ${counts.join(" · ")}` : ""}`,
        ]);
      }
      details.push(["Last committed progress", asDate(sync.last_progress_at)]);
      if (sync.as_of_ms || sync.as_of) details.push(["Status read at", asDate(sync.as_of_ms ?? sync.as_of)]);
      function syncControlButton(operation, label) {
        // Mirror the API's validate_member_sync_operation: omit controls this state can never accept.
        const valid = {
          pause: ["queued", "running", "waiting"].includes(sync.status),
          resume: sync.status === "paused",
          retry: memberSyncRetryEnabled(sync),
          reconcile: sync.status === "complete" && !!sync.initial_completed_at,
        }[operation];
        if (!valid) return null;
        const disabled = !sync.run_id || controlPending();
        return h(
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
                observeControl(job, operation);
              } else {
                syncActionOutcome = `${label} action ${job.state}. Check Bot actions for details.`;
              }
              renderStatus(latestStatus);
              schedulePoll();
              loadStatus();
            },
          },
          label,
        );
      }
      // A short planned restart clears itself when audit recovery finishes, unless
      // that recovery stopped for attention. Anything else needs Reconcile.
      const autoClearing = sync.gap_auto_clear && sync.audit_recovery_status !== "needs_attention";
      const reconcileInNotice = sync.reconciliation_needed && !autoClearing && !isPreview() && canManageServer();
      const reconcileButton = reconcileInNotice && syncControlButton("reconcile", "Reconcile from Discord");
      if (sync.reconciliation_needed) {
        const when = sync.gap_at ? ` at ${asDate(sync.gap_at)}` : "";
        if (autoClearing)
          parts.push(
            h(
              "p",
              { id: "member-coverage-notice", className: "muted", role: "status" },
              `The bot restarted${when}. Ban and unban actions from the restart are being caught up automatically; no action is needed.`,
            ),
          );
        else
          parts.push(
            h(
              "div",
              { id: "member-coverage-notice", className: "alert alert-soft alert-warning", role: "status" },
              h(
                "div",
                {},
                h("strong", {}, "Member list may be out of date"),
                h(
                  "p",
                  {},
                  `${memberGapCause(sync.reconciliation_reason, when)}, so it may have missed some joins, leaves, or bans. Select Reconcile from Discord to re-check the full member and ban lists. This can take a while on large servers.`,
                ),
              ),
              reconcileButton && h("div", { className: "row" }, reconcileButton),
            ),
          );
      }
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
      parts.push(
        h(
          "dl",
          { className: "member-sync-details" },
          ...details.flatMap(([label, value]) => [h("dt", { key: `${label}-t` }, label), h("dd", { key: `${label}-d` }, value)]),
        ),
      );
      if (controlPending())
        parts.push(
          h(
            "p",
            { role: "status" },
            `${syncControl.job.operation[0].toUpperCase()}${syncControl.job.operation.slice(1)} requested · bot action ${syncControl.job.state}. Waiting for the import status to reflect it.`,
          ),
        );
      if (syncActionOutcome) parts.push(h("p", { className: "err", role: "status" }, syncActionOutcome));
      const controls = [];
      if (!isPreview() && canManageServer()) {
        for (const [operation, label] of [
          ["pause", "Pause"],
          ["resume", "Resume"],
          ["retry", "Retry"],
          ["reconcile", "Reconcile from Discord"],
        ]) {
          const control = operation === "reconcile" && reconcileInNotice ? null : syncControlButton(operation, label);
          if (control) controls.push(control);
        }
      }
      if (controls.length) parts.push(h("div", { className: "row" }, ...controls));
      setSyncParts(parts);
    };
    const loadStatus = async () => {
      if (!aliveHere()) return;
      clearTimer();
      pollDueAt = 0;
      // A succeeded receipt is durable before this primary status read starts.
      // Its effect (or a later state) must therefore be visible in the result.
      const succeededId = syncControl.kind === "awaiting-status" ? syncControl.job.id : null;
      const request = ++statusRequest;
      statusAbort?.abort();
      statusAbort = new AbortController();
      try {
        const sync = await apiRead(`/api/guilds/${guildId}/member-sync`, statusAbort.signal);
        if (!aliveHere() || request !== statusRequest) return;
        const revision = Number(sync.progress_revision ?? 0);
        if (latestStatus && revision < latestStatusRevision && controlPending()) {
          syncActionOutcome =
            "Import status changed after a restore. Check Bot actions before repeating the requested control.";
          syncControl = { kind: "idle" };
        }
        if (latestStatus && (revision !== latestStatusRevision ||
          sync.status !== latestStatus.status ||
          sync.audit_recovery_status !== latestStatus.audit_recovery_status ||
          sync.audit_recovery_processed !== latestStatus.audit_recovery_processed)) pollAttempt = 0;
        statusRead = "current";
        lastStatusError = null;
        if (succeededId && syncControl.job?.id === succeededId) syncControl = { kind: "idle" };
        latestStatus = sync;
        latestStatusRevision = revision;
        latestStatusReceivedAt = Date.now();
        if (emptyMembersCell)
          setMemberRows([h("tr", null, h("td", { colSpan: 5, className: "muted" }, emptyMembersText()))]);
        renderStatus(sync);
        schedulePoll();
        if (controlPending()) {
          const pendingId = syncControl.job.id;
          try {
            const jobs = await apiRead(`/api/guilds/${guildId}/actions`, statusAbort.signal);
            if (!aliveHere() || request !== statusRequest || syncControl.job?.id !== pendingId) return;
            const job = jobs.find((item) => item.id === pendingId);
            if (job && ["failed", "uncertain", "cancelled", "expired"].includes(job.state)) {
              syncActionOutcome = `${syncControl.job.operation} action ${job.state}. Check Bot actions for details.`;
              syncControl = { kind: "idle" };
            } else if (job) {
              observeControl(job, syncControl.job.operation);
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
        statusRead = "retry-needed";
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
      } finally {
        if (request === statusRequest) statusAbort = null;
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
      if (document.visibilityState === "visible" && aliveHere()) {
        if (latestStatus) renderStatus(latestStatus);
        if (!pollDueAt && !statusAbort) loadStatus();
        else schedulePoll();
      } else {
        clearTimer();
        clearCountdown();
      }
    };
    document.addEventListener("visibilitychange", visibility);
    const dispose = () => {
      authLossCleanups.delete(dispose);
      lifecycle = "disposed";
      clearTimer();
      clearCountdown();
      clearTimeout(debounce);
      listAbort?.abort();
      statusAbort?.abort();
      detailAbort?.abort();
      document.removeEventListener("visibilitychange", visibility);
    };
    authLossCleanups.add(dispose);
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
      h("div", { className: "card-body" }, ...syncParts),
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
    detailParts.length
      ? h(
          "section",
          { className: "card bg-base-100 card-border", "aria-label": "Member history" },
          h("div", { className: "card-body" }, ...detailParts),
        )
      : null,
  );
}

function caseCleanupState(cleanupPending) {
  return cleanupPending ? "pending" : "complete";
}

function tabCases() {
  const guildId = state.guildId;
  const handlers = useRef({});
  const [rows, setRows] = useState([]);
  const [loading, setLoading] = useState(true);
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
    let lifecycle = "active";
    let request = 0;
    let listController;
    let detailRequest = 0;
    let after = "";
    let listMode = "live";
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
        if (lifecycle === "disposed" || currentDetail !== detailRequest || state.guildId !== guildId) return;
        const c = payload.case || {};
        let noteIntent = { kind: "empty" };
        let noteActivity = "idle";
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
                      caseCleanupState(result?.cleanup_pending) === "pending"
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
          canManageServer() &&
          field(
            "Note",
            note,
            h(
              ActionButton,
              {
                className: "btn btn-sm",
                type: "button",
                onClick: async () => {
                  if (noteActivity === "submitting") return;
                  const text = noteRef.current.value.trim();
                  if (noteIntent.kind === "empty" || noteIntent.body.text !== text) {
                    noteIntent = { kind: "retained", body: { entry_id: crypto.randomUUID().replaceAll("-", ""), text } };
                  }
                  noteActivity = "submitting";
                  try {
                    await api(`/api/guilds/${guildId}/cases/${id}/history`, "POST", noteIntent.body);
                    noteIntent = { kind: "empty" };
                    toast("Note added", "success");
                    await openCase(id);
                    ui.set({ edits: "clean" });
                  } finally {
                    noteActivity = "idle";
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
        if (lifecycle === "disposed" || currentDetail !== detailRequest || state.guildId !== guildId) return;
        setDetailParts([h("p", { className: "err" }, e.message || String(e))]);
      }
    };
    const run = async () => {
      const current = ++request;
      listController?.abort();
      const controller = new AbortController();
      listController = controller;
      const signal = controller.signal;
      const timeout = setTimeout(() => controller.abort(), 30000);
      setLoading(true);
      ++detailRequest;
      setDetailParts([]);
      setPagination((value) => ({ ...value, next: true }));
      setRows([]);
      try {
        const query = new URLSearchParams({ limit: String(pageSize), removed: String(listMode === "removed") });
        if (after) query.set("after", after);
        const payload = await apiRead(`/api/guilds/${guildId}/cases?${query}`, signal);
        if (lifecycle === "disposed" || current !== request || state.guildId !== guildId) return;
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
                listMode === "removed" ? "No more removed records." : "No selected cases on this page.",
              ),
            ),
          ]);
          return;
        }
        const rows = [];
        for (const id of ids) {
          if (listMode === "removed") {
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
                          caseCleanupState(result.cleanup_pending) === "pending"
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
          const c = payload.summaries?.[id];
          rows.push(
            h(
              "tr",
              {},
              h(
                "td",
                {},
                h(ActionButton, { className: "btn btn-sm", onClick: () => openCase(id) }, id.slice(0, 8)),
              ),
              h("td", {}, c?.action || "Summary unavailable"),
              h("td", {}, c?.outcome || ""),
              h("td", {}, String(c?.subject_id || "")),
              h(
                "td",
                { className: "muted" },
                c?.occurred_at_ms ? new Date(Number(c.occurred_at_ms)).toLocaleString() : "",
              ),
            ),
          );
        }
        setRows(rows);
      } catch (e) {
        if (lifecycle === "disposed" || current !== request || state.guildId !== guildId) return;
        setRows([
          h(
            "tr",
            {},
            h(
              "td",
              { colSpan: 5, className: "err", role: "alert" },
              "Could not load cases: " + (signal.aborted
                ? "Request timed out. Refresh to verify saved state before trying again."
                : e.message || String(e)),
            ),
          ),
        ]);
      } finally {
        clearTimeout(timeout);
        if (lifecycle === "active" && current === request && state.guildId === guildId) setLoading(false);
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
        listMode = listMode === "removed" ? "live" : "removed";
        setRemovedView(listMode === "removed");
        after = "";
        run();
      },
    };
    run();
    if (handoff?.guildId === guildId) openCase(handoff.caseId);
    return () => {
      lifecycle = "disposed";
      listController?.abort();
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
    loading
      ? h(
        "div",
        { className: "card bg-base-100 card-border", role: "status", "aria-live": "polite" },
        h("div", { className: "card-body" }, "Loading cases…"),
      )
      : h(DataTable, { headings: ["Case", "Action", "Outcome", "Subject", "When"] }, rows),
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
    draftKey: "settings:prefix",
    type: "text",
    defaultValue: cfg.settings.prefix,
    className: "input",
  });
  const botNameRef = useRef(null);
  const botName = h(Input, {
    ref: botNameRef,
    draftKey: "settings:bot_name",
    type: "text",
    defaultValue: cfg.settings.bot_name,
    className: "input",
  });

  const srcSelRef = useRef(null);
  const srcSel = h(
    Select,
    {
      ref: srcSelRef,
      draftKey: "copy:source",
      defaultValue: "",
      className: "select",
      onChange: (event) =>
        handleEvent(
          async () => {
            ui.set({ copySource: null });
            const guildId = srcSelRef.current.value;
            if (!guildId) return;
            const source = await apiRead(`/api/guilds/${guildId}/config`);
            if (srcSelRef.current?.value === guildId && source.enrollment_id) {
              ui.set({ copySource: { guildId, id: source.enrollment_id } });
            }
          },
          event,
        ),
    },
    h("option", { value: "" }, "Select a source server"),
    ...state.guilds
      .filter((g) => g.id !== state.guildId && (g.access || "manage") === "manage")
      .map((g) => h("option", { value: g.id }, g.name)),
  );
  const sectionSelRef = useRef(null);
  const sectionSel = h(
    Select,
    { ref: sectionSelRef, className: "select", draftKey: "copy:section", defaultValue: "all" },
    ["all", "rules", "badwords", "commands", "templates", "settings"].map((s) =>
      h("option", { value: s }, s === "all" ? "everything" : s),
    ),
  );
  const globalPrefixRef = useRef(null);
  const globalPrefix = h(Input, {
    ref: globalPrefixRef,
    draftKey: "global:prefix",
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
    return field(
      labelText,
      input,
      hint ? h("p", { className: "muted", style: { margin: "4px 0 0", fontSize: "12px" } }, hint) : null,
      hasOv && allowInheritance
        ? h(
            ActionButton,
            {
              className: "btn btn-sm",
              type: "button",
              savedFields: [input.props.draftKey],
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
      DraftCard,
      { group: "settings", className: "card bg-base-100 card-border" },
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
              savedFields: [],
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

  const [roleBaseline, setRoleBaseline] = useState("");
  const roleInputRef = useRef(null);
  const roleInput = h(Input, {
    ref: roleInputRef,
    draftKey: "settings:roles",
    defaultValue: roleBaseline,
    type: "text",
    placeholder: "role id, <@&id> mention, comma-separated",
    className: "input",
  });
  const roleRevision = useRef("");
  const roleWriteGeneration = useRef(0);
  useEffect(() => {
    if (!canManageServer()) return undefined;
    let active = true;
    const initialRoleGeneration = roleWriteGeneration.current;
    apiRead(`/api/guilds/${state.guildId}/case-delete-roles`)
      .then((roles) => {
        if (!active || initialRoleGeneration !== roleWriteGeneration.current) return;
        const loadedValue = (roles.roles || []).join(", ");
        setRoleBaseline(loadedValue);
        roleRevision.current = roles.revision || "";
      })
      .catch((e) => {
        if (active && !isUnauthenticated(e)) reportApiError(e);
      });
    return () => {
      active = false;
    };
  }, []);
  if (canManageServer()) cards.push(
    h(AccessRolesCard, { key: "access-roles", revision: roleRevision, generation: roleWriteGeneration }),
    h(
      DraftCard,
      { group: "roles", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Case removal"),
        h(
          "p",
          { className: "muted" },
          "Server owners and Administrators can remove case records. Stored role IDs are remembered for a later member-role check and are not applied yet. Removal never undoes a Discord action.",
        ),
        field("Delegated case-removal role IDs", roleInput),
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
                }, undefined, () => apiRead(`/api/guilds/${state.guildId}/case-delete-roles`));
                roleWriteGeneration.current++;
                roleRevision.current = saved.revision;
                setRoleBaseline(saved.roles ? saved.roles.join(", ") : roleInputRef.current.value);
                const drafts = { ...currentDrafts() };
                delete drafts["settings:roles"];
                ui.set({ drafts, edits: Object.keys(drafts).length > 0 ? "dirty" : "clean" });

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
        DraftCard,
        { group: "global", className: "card bg-base-100 card-border" },
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
            field("Global prefix", globalPrefix),
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

  if (canManageServer()) cards.push(
    h(
      DraftCard,
      { group: "copy", className: "card bg-base-100 card-border" },
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
                if (!ui.getSnapshot().copySource || ui.getSnapshot().copySource.guildId !== srcSelRef.current.value)
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
                      from_enrollment_id: ui.getSnapshot().copySource.id,
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

const ACCESS_LEVELS = [
  ["dashboard", "Dashboard (read-only)"],
  ["settings", "Dashboard and settings"],
];

// Role -> access mappings for this server. Only Manage Server sees this card;
// the API refuses the read and the save for everyone else.
// Access and case-removal roles share one server revision, so both cards use
// the same revision and write-generation refs from the Settings tab.
function AccessRolesCard({ revision, generation }) {
  const guildId = state.guildId;
  const [baselineRows, setBaselineRows] = useState(null);
  const [ttl, setTtl] = useState(30);
  const { drafts } = useStore(ui);
  const draftKey = "settings:access-roles";
  const rows = baselineRows === null ? null : drafts[draftKey]?.value ?? baselineRows;
  const withKeys = (roles) => roles.map((role) => ({ ...role, key: crypto.randomUUID() }));
  const changeRows = (next) => {
    const values = (items) => JSON.stringify(items.map(({ role_id, level }) => ({ role_id, level })));
    const nextDrafts = { ...currentDrafts() };
    if (baselineRows !== null && values(next) === values(baselineRows)) delete nextDrafts[draftKey];
    else nextDrafts[draftKey] = { value: next, group: "access-roles" };
    ui.set({ drafts: nextDrafts, edits: Object.keys(nextDrafts).length > 0 ? "dirty" : "clean" });
  };
  useEffect(() => {
    let active = true;
    const initialGeneration = generation.current;
    apiRead(`/api/guilds/${guildId}/access-roles`)
      .then((loaded) => {
        if (!active) return;
        if (initialGeneration === generation.current) revision.current = loaded.revision || "";
        setTtl(loaded.ttl_secs || 30);
        setBaselineRows(withKeys(loaded.roles || []));
      })
      .catch((e) => {
        if (active && !isUnauthenticated(e)) reportApiError(e);
      });
    return () => {
      active = false;
    };
  }, []);
  const update = (key, patch) => {
    changeRows(rows.map((row) => (row.key === key ? { ...row, ...patch } : row)));
  };
  return h(
    DraftCard,
    { group: "access-roles", className: "card bg-base-100 card-border" },
    h(
      "div",
      { className: "card-body" },
      h("h3", { className: "card-title" }, "Web UI access by role"),
      h(
        "p",
        { className: "muted" },
        `Members with a listed Discord role can sign in and open this server only. Dashboard access is read-only. Settings access can also edit rules, badwords, known spam and images, templates, log channels and bot settings. Commands, copying configuration, member import controls, case notes, case removal and this list stay with Manage Server. Roles are checked with Discord; a removed role stops working within ${ttl} seconds.`,
      ),
      rows === null
        ? h("p", { className: "muted", role: "status" }, "Loading roles…")
        : h(
            React.Fragment,
            null,
            rows.length === 0 && h("p", { className: "muted" }, "No roles have Web UI access."),
            ...rows.map((row) =>
              h(
                "div",
                { key: row.key, className: "row form-row access-role-row" },
                field(
                  "Role ID",
                  h("input", {
                    type: "text",
                    className: "input",
                    value: row.role_id,
                    placeholder: "Discord role (ID or <@&id> mention)",
                    onChange: (event) => update(row.key, { role_id: event.target.value }),
                  }),
                ),
                field(
                  "Access",
                  h(
                    "select",
                    {
                      className: "select",
                      value: row.level,
                      onChange: (event) => update(row.key, { level: event.target.value }),
                    },
                    ACCESS_LEVELS.map(([value, label]) => h("option", { key: value, value }, label)),
                  ),
                ),
                h(
                  ActionButton,
                  {
                    className: "btn btn-sm",
                    type: "button",
                    onClick: () => {
                      changeRows(rows.filter((item) => item.key !== row.key));
                    },
                  },
                  "Remove",
                ),
              ),
            ),
            h(
              "div",
              { className: "card-actions" },
              h(
                ActionButton,
                {
                  className: "btn",
                  type: "button",
                  disabled: rows.length >= 32,
                  onClick: () => changeRows([...rows, ...withKeys([{ role_id: "", level: "dashboard" }])]),
                },
                "Add role",
              ),
              h(
                ActionButton,
                {
                  className: "btn btn-primary",
                  onClick: async () => {
                    const roles = rows.map((row) => {
                      const id = parseSnowflake(String(row.role_id).trim());
                      if (!id) throw new Error("Enter each role as a Discord role ID or <@&id> mention");
                      return { role_id: id, level: row.level };
                    });
                    const saved = await api(`/api/guilds/${guildId}/access-roles`, "PUT", {
                      roles,
                      revision: revision.current,
                    }, undefined, () => apiRead(`/api/guilds/${guildId}/access-roles`));
                    generation.current++;
                    revision.current = saved.revision;
                    setBaselineRows(withKeys(saved.roles || roles));
                    const nextDrafts = { ...currentDrafts() };
                    delete nextDrafts[draftKey];
                    ui.set({ drafts: nextDrafts, edits: Object.keys(nextDrafts).length > 0 ? "dirty" : "clean" });
                    toast("Web UI access roles saved", "success");
                  },
                },
                "Save access roles",
              ),
            ),
          ),
    ),
  );
}

function scopeSelect(
  ref,
  guildLabel = "This server only",
  globalLabel = "Global (all servers)",
  allowGlobal = true,
  draftKey,
) {
  return h(
    Select,
    { ref, className: "select", draftKey, defaultValue: "guild" },
    h("option", { value: "guild" }, guildLabel),
    allowGlobal && isBotOwner() && h("option", { value: "global" }, globalLabel),
  );
}
function BanOptions({ action }) {
  const legacyDays = action.delete_days || 0;
  const selected = action.purge_last_hour ? "purge" : legacyDays ? "legacy" : "ban";
  return field(
    "Ban behavior",
    h(
      Select,
      {
        className: "select",
        defaultValue: selected,
        onChange: (event) => {
          const mode = event.target.value;
          action.purge_last_hour = mode === "purge";
          action.delete_days = mode === "legacy" ? legacyDays : 0;
        },
      },
      h("option", { value: "ban" }, "Ban"),
      h("option", { value: "purge" }, "Ban and purge (previous hour)"),
      legacyDays > 0 && h("option", { value: "legacy" }, `Keep existing: delete ${legacyDays} day(s)`),
    ),
    h("p", { className: "field-help" }, "Ban keeps earlier messages. Ban and purge asks Discord to delete this member’s messages across the server from the previous hour. Applies only to ban actions; dry-run never bans or purges."),
  );
}

function RuleAction({ action, index, remove }) {
  return h(
    "div",
    { className: "card card-border escalation-step" },
    h(
      "div",
      { className: "row title-row" },
      h("strong", null, `Action ${index + 1}`),
      h(
        IconButton,
        { icon: "remove", label: `Remove action ${index + 1}`, onClick: remove },
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
      h(BanOptions, { action }),
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
  const [result, setResult] = useState({ kind: "loading" });
  useEffect(() => {
    const controller = new AbortController();
    let publication = "current";
    const timeout = setTimeout(() => controller.abort(), 30000);
    setResult({ kind: "loading" });
    apiRead(path, controller.signal)
      .then((data) => {
        if (publication === "current") setResult({ kind: "loaded", data });
      })
      .catch((error) => {
        if (publication === "current")
          setResult({
            kind: "failed",
            error: controller.signal.aborted
              ? apiError("Request timed out. Refresh to verify saved state before trying again.")
              : error,
          });
      })
      .finally(() => clearTimeout(timeout));
    return () => {
      publication = "disposed";
      clearTimeout(timeout);
      controller.abort();
    };
  }, [path, revision]);
  return {
    loading: result.kind === "loading",
    data: result.kind === "loaded" ? result.data : null,
    error: result.kind === "failed" ? result.error : null,
    refresh: () => setRevision((value) => value + 1),
  };
}
function DataTable({ headings, children, className = "table table-zebra" }) {
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
      { className },
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
const STOCKS_SPAM_EXAMPLE = "📈 **Looking for a community focused on stocks and trading?**\n\nJoin our growing community to learn from experienced investors, share market insights, discuss stock ideas, and exchange trading strategies.\nNo spam or aggressive promotion—just practical market discussions.\nWe welcome active investors and value your thoughts and feedback.\n👇 Interested? Join us here:";
const AI_SPAM_EXAMPLE = "I used to work as Full Stack AI developer at several software companies but I’m currently available and working remotely because my last contract ended. Right now I am resting at home. If you have any AI projects or need support, I’d be happy to help, just reach out.";

// A failed add's reason belongs to its retained draft: it survives remounts and
// the other entry's add, and goes away when the draft is edited or cleared.
function setKnownSpamAddError(key, error) {
  const drafts = ui.getSnapshot().drafts;
  if (!drafts[key]) return;
  const draft = { value: drafts[key].value, group: drafts[key].group };
  ui.set({ drafts: { ...drafts, [key]: error ? { ...draft, error } : draft } });
}

function tabKnownSpam() {
  const { drafts } = useStore(ui);
  const scopeRef = useRef(null);
  const guildId = state.guildId;
  const rows = state.config.known_spam || [];
  const rule = (state.config.rules || []).find((r) => r.rule_id === "known_spam");
  const add = async (kind) => {
    const key = kind === "sample" ? "known-spam:sample" : "known-spam:url";
    const value = drafts[key]?.value || "";
    if (!value.trim()) return toast("Enter a full sample or URL pattern", "error");
    const scope = scopeRef.current.value;
    setKnownSpamAddError(key, null);
    try {
      await reloadAfter(async () => {
        const res = await api(`/api/guilds/${guildId}/known-spam`, "POST", {
          scope, kind, value, note: drafts["known-spam:note"]?.value || null,
        });
        // Acknowledged: keep this scope selected, without a draft, even if the refresh fails.
        ui.set({ draftBaselines: { ...ui.getSnapshot().draftBaselines, "known-spam:scope": scope } });
        return res;
      }, () => toast("Known spam entry added", "success"));
    } catch (error) {
      // The browser cannot tell a rejected save from one whose outcome is unknown
      // or pending, so state only that the add was not confirmed.
      const label = kind === "sample" ? "Sample" : "URL pattern";
      const lookup = error?.saveLookup ? " Retrying the same entry, note and scope only looks up the earlier request; no new save was sent." : "";
      setKnownSpamAddError(key, `${label} not confirmed as added: ${error?.message || error}${lookup}`);
      throw error;
    }
  };
  const addErrorFor = (key) => drafts[key]?.error ? h("p", { className: "error", role: "alert" }, drafts[key].error) : null;
  return h("div", null,
    h("h2", null, "Known spam"),
    h("p", { className: "muted" }, "Save full confirmed spam messages or URL wildcards. First match bans and purges Discord’s Previous Hour (3600 seconds). Dry-run only logs. The list uses the rule’s enable, skip lists and skip-moderators settings below. Global entries apply to every server; only the bot owner can edit them."),
    h("p", { className: "field-help" }, "Samples match the whole message, using the same raw, normalized and whitespace-stripped forms as Badwords (case, accents, zero-width characters and supported leetspeak). A lightly rewritten pitch may not match. Quoting or adding different text does not match a sample; URL patterns still apply."),
    rule ? ruleCard(rule) : null,
    h(DraftCard, { group: "known-spam", className: "card bg-base-100 card-border" },
      h("div", { className: "card-body" },
        field("Applies to", scopeSelect(scopeRef, "This server", "Global", true, "known-spam:scope")),
        field("Optional note (up to 512 bytes)", h(Input, { className: "input w-full", draftKey: "known-spam:note", "data-note": true, maxLength: 512 })),
        field("Full sample message (up to 4096 bytes)", h(Textarea, { className: "textarea w-full", draftKey: "known-spam:sample", rows: 7, maxLength: 4096, placeholder: "Paste the entire confirmed spam message. No regex needed." })),
        h(ActionButton, { className: "btn btn-primary", savedFields: ["known-spam:sample", "known-spam:note", "known-spam:scope"], onClick: () => add("sample") }, "Add sample"),
        addErrorFor("known-spam:sample"),
        field("URL wildcard (up to 512 bytes)", h(Input, { className: "input w-full", draftKey: "known-spam:url", "data-url": true, maxLength: 512, placeholder: "tinyurl.com/*" })),
        h("p", { className: "field-help" }, "HTTP/HTTPS scheme is optional and ignored. Hosts ignore case; tinyurl.com/* matches that exact host, not subdomains. *.example.com/* matches subdomains only. Omitted path means /*. Paths are case-sensitive; * matches zero or more characters, including /. Query and fragment are ignored in links and forbidden in patterns. No regex, credentials or ports in patterns. Bare domain links also match. Up to 1024 entries per scope; normalized duplicates are rejected."),
        h(ActionButton, { className: "btn btn-primary", savedFields: ["known-spam:url", "known-spam:note", "known-spam:scope"], onClick: () => add("url") }, "Add URL pattern"),
        addErrorFor("known-spam:url"),
      ),
    ),
    h("details", { className: "card bg-base-100 card-border" },
      h("summary", null, "Paste-as-sample examples (not active until saved)"),
      h("p", null, "Copy the full owner sample you want to save into the sample field. These examples are not preloaded entries."),
      h("pre", { style: { whiteSpace: "pre-wrap" } }, STOCKS_SPAM_EXAMPLE),
      h("pre", { style: { whiteSpace: "pre-wrap" } }, AI_SPAM_EXAMPLE),
    ),
    h(DataTable, { headings: ["Type", "Sample or URL pattern", "Note", "Scope", "Actions"] },
      rows.length ? rows.map((row) => h("tr", { key: row.id },
        h("td", null, row.kind === "url" ? "URL" : "Sample"),
        h("td", { style: { whiteSpace: "pre-wrap", overflowWrap: "anywhere" } }, row.value),
        h("td", null, row.note || "—"), h("td", null, row.guild_id === "0" ? "Global" : "This server"),
        h("td", null, row.guild_id !== "0" || isBotOwner() ? h(IconButton, { icon: "remove", label: "Remove",
          onClick: async () => {
            if (!confirm("Remove this known spam entry?")) return;
            await api(`/api/guilds/${guildId}/known-spam/${row.id}`, "DELETE");
            if (state.guildId === guildId && await loadConfig()) renderTab();
          },
        }) : "Owner only"),
      )) : h("tr", null, h("td", { colSpan: 5, className: "muted" }, "No known spam entries.")),
    ),
  );
}

function tabKnownBadImages() {
  const [query, setQuery] = useState("");
  const { drafts } = useStore(ui);
  const hashDraft = drafts["known-bad:hash"]?.value ?? "";
  const guildId = state.guildId;
  const rows = (state.config.known_bad_images || []).filter((row) =>
    `${row.hash} ${row.note || ""} ${row.added_via} ${row.actor}`.toLowerCase().includes(query.toLowerCase()),
  );
  if (guildId === "0") return h("p", { className: "muted" }, "Known-bad images are managed per server. Select a server.");
  return h("div", null,
    h("h2", null, "Known-bad images"),
    h("p", { className: "muted" }, "Add a confirmed SHA-256 digest from an exact-image notice. A listed image matches on its first post under the exact-image rule. Dry-run logs only; normal mode deletes and bans. Up to 1024 digests per server. Only hashes and safe labels are retained."),
    field("Search hashes or notes", h(Input, {
      className: "input w-full", "data-search": true, value: query, onChange: (event) => setQuery(event.target.value),
    })),
    h(
      DraftCard,
      { group: "known-bad-images", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        field("SHA-256 digest", h(Input, {
          className: "input w-full", draftKey: "known-bad:hash", maxLength: 64,
        })),
        field("Optional safe note", h(Input, {
          className: "input w-full", draftKey: "known-bad:note", maxLength: 256,
        })),
        h(ActionButton, {
          className: "btn btn-sm",
          disabled: !/^[a-fA-F0-9]{64}$/.test(hashDraft),
          savedFields: ["known-bad:hash", "known-bad:note"],
          onClick: async () => {
            const hash = drafts["known-bad:hash"]?.value ?? "";
            const note = drafts["known-bad:note"]?.value ?? "";
            await reloadAfter(
              () => api(`/api/guilds/${guildId}/known-bad-images`, "POST", { hash, note: note || null }),
              (result) => toast(result.changed ? "Hash added." : "Hash already listed.", "success"),
            );
          },
        }, "Add hash"),
      ),
    ),
    h(DataTable, { headings: ["SHA-256", "Added", "Source", "Actor", "Note", "Actions"] },
      rows.length ? rows.map((row) => h("tr", { key: row.hash },
        h("td", { style: { overflowWrap: "anywhere" } }, row.hash),
        h("td", null, row.added_at), h("td", null, row.added_via),
        h("td", null, row.actor), h("td", null, row.note || "—"),
        h("td", null, h(IconButton, { icon: "remove", label: "Remove",
          onClick: async () => {
            if (!confirm(`Remove SHA-256 ${row.hash}?`)) return;
            await api(`/api/guilds/${guildId}/known-bad-images/${row.hash}`, "DELETE");
            if (state.guildId === guildId && await loadConfig()) renderTab();
          },
        })),
      )) : h("tr", null, h("td", { colSpan: 6, className: "muted" }, "No matching hashes.")),
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
                    IconButton,
                    {
                      icon: "unban",
                      label: "Unban",
                      onClick: async () => {
                        if (confirm(`Unban ${record.user_id}?`))
                          await submitBotAction(`/api/guilds/${guildId}/unban`, "POST", {
                            user_id: String(record.user_id),
                            reason: "unbanned via WebUI",
                          });
                      },
                    },
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
                    IconButton,
                    {
                      icon: "cancel",
                      label: actionCancelLabel(job),
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
                  ),
              ),
            ),
          ),
    ),
  );
}

// Render only fixed React elements and text children: React escapes HTML before
// it reaches the DOM. Never parse user text as HTML or use innerHTML here.
function previewInline(text, depth = 0) {
  if (depth >= 20) return [text];
  const out = [];
  // A code opener runs to the end of its backtick run, and its closer is the
  // next run with at least that many ticks. Index every run once, right to
  // left, so unmatched ticks do not rescan the rest of the text.
  let codeSpans;
  const codeSpan = start => {
    if (!codeSpans) {
      codeSpans = new Map();
      const next = [];
      for (let end = text.length; end > 0;) {
        if (text[end - 1] !== "`") { end--; continue; }
        let run = end - 1;
        while (run > 0 && text[run - 1] === "`") run--;
        for (let i = run; i < end; i++) codeSpans.set(i, [end - i, next[end - i] ?? -1]);
        for (let ticks = 1; ticks <= end - run; ticks++) next[ticks] = run;
        end = run;
      }
    }
    return codeSpans.get(start);
  };
  // Like Discord, italics skip a nested bold or underline pair, and an emphasis
  // closer must end its delimiter run, so `*a **b** c*` and `**a *b***` nest.
  const closing = (marker, start, skipCode = false) => {
    const emphasis = skipCode && ["*", "_", "**", "__"].includes(marker);
    for (let i = start; i < text.length; i++) {
      if (text[i] === "\\") { i++; continue; }
      if (skipCode && text[i] === "`") {
        const [ticks, end] = codeSpan(i);
        if (end >= 0) { i = end + ticks - 1; continue; }
      }
      if (i > start && emphasis && marker.length === 1 && text.startsWith(marker + marker, i)) { i++; continue; }
      if (text.startsWith(marker, i) && (i === start || !emphasis || text[i + marker.length] !== marker[0])) return i;
    }
    return -1;
  };
  // Bracket queries advance monotonically. Reuse the next label closer,
  // including a failed search, until the caller passes it.
  let labelEnd;
  let linkEnds;
  const linkEnd = start => {
    if (!linkEnds) {
      linkEnds = new Map();
      const open = [];
      // Match destination parentheses once, with the same escape and balance
      // rules as the forward scan. An unmatched outer '(' can contain a
      // matched inner link, so a failed outer search cannot skip the suffix.
      for (let i = start; i < text.length; i++) {
        if (text[i] === "\\") { i++; continue; }
        if (text[i] === "(") open.push(i);
        if (text[i] === ")" && open.length) linkEnds.set(open.pop(), i);
      }
    }
    return linkEnds.get(start) ?? -1;
  };
  for (let i = 0; i < text.length;) {
    const key = i;
    if (text[i] === "\\" && /[\\`*_{}[\]()#+\-.!>~|]/.test(text[i + 1] || "")) {
      out.push(text[i + 1]); i += 2; continue;
    }
    if (text[i] === "`") {
      const [ticks, end] = codeSpan(i);
      if (end >= 0) {
        out.push(h("code", { key }, text.slice(i + ticks, end)));
        i = end + ticks; continue;
      }
    }
    if (text[i] === "[") {
      if (labelEnd === undefined || (labelEnd >= 0 && labelEnd < i + 1)) labelEnd = closing("](", i + 1);
      if (labelEnd >= 0) {
        const end = linkEnd(labelEnd + 1);
        if (end >= 0) {
          const url = text.slice(labelEnd + 2, end).replace(/\\([()])/g, "$1");
          if (/^https?:\/\/[^\s<>\\]+$/i.test(url)) {
            out.push(h("a", { key, href: url, target: "_blank", rel: "noreferrer noopener" },
              previewInline(text.slice(i + 1, labelEnd), depth + 1)));
            i = end + 1; continue;
          }
          // An unsafe target stays literal, including any Markdown within it.
          out.push(text.slice(i, end + 1)); i = end + 1; continue;
        }
      }
    }
    let matched = false;
    for (const [marker, tag] of [["***", "strong"], ["**", "strong"], ["__", "u"], ["~~", "s"], ["||", "details"], ["*", "em"], ["_", "em"]]) {
      if (!text.startsWith(marker, i)) continue;
      const end = closing(marker, i + marker.length, true);
      if (end <= i + marker.length) continue;
      let children = previewInline(text.slice(i + marker.length, end), depth + 1);
      if (marker === "***") children = h("em", {}, children);
      out.push(marker === "||"
        ? h(tag, { key, className: "preview-spoiler" }, h("summary", {}, "Spoiler"), children)
        : h(tag, { key }, children));
      i = end + marker.length; matched = true; break;
    }
    if (!matched) out.push(text[i++]);
  }
  return out;
}

function previewMarkdown(text, depth = 0) {
  if (depth >= 20) return [text];
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const out = [];
  const listItem = line => line.match(/^( *)([-*]|\d+\.) (.*)$/);
  for (let i = 0; i < lines.length;) {
    const key = i, line = lines[i];
    const fence = line.match(/^(`{3,})([^`]*)$/);
    if (fence) {
      const code = [];
      i++;
      while (i < lines.length && !new RegExp("^`{" + fence[1].length + ",}\\s*$").test(lines[i])) code.push(lines[i++]);
      if (i < lines.length) i++;
      out.push(h("pre", { key }, h("code", {}, code.join("\n")))); continue;
    }
    if (line.startsWith(">>> ")) {
      const quoted = [line.slice(4), ...lines.slice(i + 1)].join("\n");
      out.push(h("blockquote", { key }, previewMarkdown(quoted, depth + 1))); break;
    }
    if (line.startsWith("> ")) {
      const quoted = [];
      while (i < lines.length && lines[i].startsWith("> ")) quoted.push(lines[i++].slice(2));
      out.push(h("blockquote", { key }, previewMarkdown(quoted.join("\n"), depth + 1))); continue;
    }
    const item = listItem(line);
    if (item) {
      const ordered = item[2].endsWith("."), indent = item[1].length, items = [];
      while (i < lines.length) {
        const next = listItem(lines[i]);
        if (!next || next[1].length !== indent || next[2].endsWith(".") !== ordered) break;
        const body = [next[3]], itemKey = i++;
        while (i < lines.length && lines[i].startsWith(" ".repeat(indent + 2))) body.push(lines[i++].slice(indent + 2));
        items.push(h("li", { key: itemKey }, previewMarkdown(body.join("\n"), depth + 1)));
      }
      out.push(h(ordered ? "ol" : "ul", { key, ...(ordered ? { start: Number.parseInt(item[2], 10) } : {}) }, items)); continue;
    }
    const heading = line.match(/^(#{1,3}|-#) (.*)$/);
    out.push(h(heading ? heading[1] === "-#" ? "small" : "h" + heading[1].length : "div", { key },
      line === "" ? h("br") : previewInline(heading ? heading[2] : line)));
    i++;
  }
  return out;
}

capturePreviewToken();
root.render(h(App));
init();
