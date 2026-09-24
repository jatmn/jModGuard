/* jModGuard WebUI — vanilla JS single-page app; daisyUI/Headless load at runtime */
"use strict";

const $ = (sel) => document.querySelector(sel);

function uiClass(value) {
  const names = String(value || "").split(/\s+/).filter(Boolean);
  const hasBtn = names.includes("btn");
  const hasTag = names.includes("tag");
  const out = [];
  for (const name of names) {
    if (hasBtn && name === "primary") { out.push("btn-primary"); continue; }
    if (hasBtn && name === "small") { out.push("btn-sm"); continue; }
    if (hasBtn && name === "danger") { out.push("btn-error"); continue; }
    if (hasBtn && name === "good") { out.push("btn-success"); continue; }
    if (hasBtn && name === "outline") { out.push("btn-outline"); continue; }
    if (hasBtn && name === "ghost") { out.push("btn-ghost"); continue; }
    if (name === "tag") { out.push("badge", "status-tag"); continue; }
    if (hasTag && name === "scope") { out.push("badge-info", "scope-tag"); continue; }
    if (hasTag && name === "override") { out.push("badge-secondary", "override-tag"); continue; }
    if (hasTag && name === "on") { out.push("badge-success"); continue; }
    if (hasTag && name === "off") { out.push("badge-error"); continue; }
    if (hasTag && name === "dry") { out.push("badge-warning"); continue; }
    if (name === "modal-overlay") { out.push("modal", "modal-open", "modal-overlay"); continue; }
    if (name === "card") { out.push("card", "bg-base-100", "card-border", "shadow-sm"); continue; }
    out.push(name);
  }
  return [...new Set(out)].join(" ");
}

function decorateControl(n) {
  const tag = n.tagName;
  if (tag === "INPUT") {
    const type = n.type || "text";
    if (type === "checkbox") n.classList.add("checkbox");
    else if (type !== "hidden" && type !== "file") n.classList.add("input", "input-bordered");
  } else if (tag === "SELECT" && !n.classList.contains("search-select-native")) {
    n.classList.add("select", "select-bordered");
  } else if (tag === "TEXTAREA") {
    n.classList.add("textarea", "textarea-bordered");
  } else if (tag === "H3") {
    n.classList.add("card-title");
  }
}

const el = (tag, attrs, ...children) => {
  const n = document.createElement(tag);
  if (attrs) Object.entries(attrs).forEach(([k, v]) => {
    if (k === "class") n.className = uiClass(v);
    else if (k === "text") n.textContent = v;
    else if (k === "savedInputs") n._savedInputs = v;
    else if (k.startsWith("on")) n.addEventListener(k.slice(2), (event) => handleEvent(n, v, event));
    else if (k === "checked" || k === "selected" || k === "hidden" || k === "disabled") n[k] = Boolean(v);
    else if (v !== null && v !== undefined) n.setAttribute(k, v);
  });
  decorateControl(n);
  children.flat(Infinity).forEach((c) => {
    if (c !== null && c !== undefined) n.append(c.nodeType ? c : document.createTextNode(c));
  });
  if (n.classList.contains("card") && ![...n.children].some((child) => child.classList?.contains("card-body"))) {
    const body = document.createElement("div");
    body.className = "card-body";
    while (n.firstChild) body.append(n.firstChild);
    n.append(body);
  }
  return n;
};

let state = { me: null, guilds: [], guildId: null, config: null, tab: "dashboard", loading: false, error: null };
let configRequest = 0;
let busy = false;
let dirty = false;
let pendingDrafts = null;
const pendingLogChannelActions = new Map();
// Ten action-specific checks span the default 300-second queue lifetime plus
// the maximum 120-second dispatch timeout without polling the general Actions page.
const actionReconcileDelays = [1000, 1500, 2500, 5000, 10000, 20000, 40000, 80000, 120000, 150000];
const tabs = ["dashboard", "rules", "badwords", "commands", "templates", "logchannels", "events", "members", "bans", "actions", "cases", "settings"];
let disposeMembersView = null;
let pendingCaseId = null;

function workspaceKey() {
  return `jmodguard.workspace:${apiBase()}:${state.me.user.id}`;
}
function restoreWorkspace() {
  let saved = {};
  try { saved = JSON.parse(window.localStorage.getItem(workspaceKey())) || {}; } catch (_) { /* Storage may be unavailable. */ }
  state.guildId = state.guilds.find((g) => g.id === saved.guildId)?.id || state.guilds[0]?.id || null;
  state.tab = tabs.includes(saved.tab) ? saved.tab : "dashboard";
}
function persistWorkspace() {
  try { window.localStorage.setItem(workspaceKey(), JSON.stringify({ guildId: state.guildId, tab: state.tab })); } catch (_) { /* Session still works without storage. */ }
}
function canLeave() {
  return !busy && (!dirty || confirm("Discard unsaved changes?"));
}
function updateWorkspaceStatus() {
  const status = $("#workspace-status");
  if (status) status.textContent = busy ? "Working…" : dirty ? "Unsaved changes" : "Changes saved with each action";
}
function markDirty() {
  dirty = true;
  updateWorkspaceStatus();
}
function currentDraftInputs() {
  return [...$("#content").querySelectorAll("input, select, textarea")]
    .filter((input) => input._initialValue !== undefined && input.value !== input._initialValue);
}
function currentDrafts() {
  return currentDraftInputs().map((input) => ({ key: input.dataset.inputKey, value: input.value }));
}
function captureDrafts(node) {
  const form = node.closest?.(".card, .modal-box");
  if (!form) return [];
  return currentDraftInputs()
    .filter((input) => !(node._savedInputs ? node._savedInputs.includes(input) : form.contains(input)))
    .map((input) => ({ key: input.dataset.inputKey, value: input.value }));
}
function retainDrafts(guildId, tab, drafts, enrollmentId = state.config?.enrollment_id) {
  if (!drafts.length || guildId !== state.guildId || tab !== state.tab) return;
  pendingDrafts = { guildId, tab, drafts, enrollmentId };
  restoreDrafts();
}
function restoreDrafts() {
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
function setBusy(value) {
  busy = value;
  document.querySelectorAll("#main-view button, #main-view input, #main-view select, #main-view textarea").forEach((node) => {
    if (value) { node.dataset.wasDisabled = String(node.disabled); node.disabled = true; }
    else if (node.dataset.wasDisabled !== undefined) { node.disabled = node.dataset.wasDisabled === "true"; delete node.dataset.wasDisabled; }
  });
  updateWorkspaceStatus();
}
async function handleEvent(node, handler, event) {
  if (busy) return;
  const guildId = state.guildId, tab = state.tab, enrollmentId = state.config?.enrollment_id;
  const drafts = captureDrafts(node);
  try {
    const result = handler(event);
    if (result && typeof result.then === "function") {
      setBusy(true);
      await result;
      if (!node.isConnected) retainDrafts(guildId, tab, drafts, enrollmentId);
    }
  } catch (e) {
    reportApiError(e);
  }
  finally { if (busy) setBusy(false); }
}
function navigate(tab) {
  if (!canLeave()) return;
  pendingDrafts = null;
  state.tab = tabs.includes(tab) ? tab : "dashboard";
  persistWorkspace();
  renderTab();
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
  } catch (_) { /* The in-memory fallback remains active for this tab. */ }
  // The tab-bound fragment remains the reload-safe source if storage later
  // disappears. Fragments are not sent in HTTP requests or Referer headers.
}

function currentPreviewToken() {
  try { return window.sessionStorage.getItem("jmodguard_preview") || previewTokenFallback; }
  catch (_) { return previewTokenFallback; }
}

function clearPreviewToken() {
  previewTokenFallback = null;
  try { window.sessionStorage.removeItem("jmodguard_preview"); } catch (_) {}
  const fragment = new URLSearchParams(location.hash.replace(/^#/, ""));
  if (fragment.has("preview")) {
    fragment.delete("preview");
    const hash = fragment.toString();
    try {
      history.replaceState({}, "", location.pathname + location.search + (hash ? `#${hash}` : ""));
    } catch (_) { /* The following explicit navigation still drops the fragment. */ }
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

/** Scope <select> options — Global only for the configured bot owner. */
function scopeSelect(guildLabel = "This server only", globalLabel = "Global (all servers)", allowGlobal = true) {
  const opts = [el("option", { value: "guild", text: guildLabel })];
  if (allowGlobal && isBotOwner()) {
    opts.push(el("option", { value: "global", text: globalLabel }));
  }
  return el("select", {}, ...opts);
}

function apiBase() {
  return ((typeof window !== "undefined" && window.JMODGUARD_API_BASE) || "").replace(/\/$/, "");
}

/** Runtime CDN pins. daisyUI / Headless / React are not stored in this repository. */
const VENDOR = {
  daisyuiCss: "https://cdn.jsdelivr.net/npm/daisyui@5.7.43/daisyui.css",
  esm: "https://esm.sh",
  react: "19.3.0",
  headless: "2.2.10",
};

function vendorCssHref() {
  const href = typeof window !== "undefined" ? window.JMODGUARD_DAISYUI_HREF : "";
  return String(href || VENDOR.daisyuiCss);
}

function applyVendorCss() {
  if (typeof document === "undefined" || typeof document.getElementById !== "function") return;
  const link = document.getElementById("jmodguard-daisyui");
  if (!link) return;
  const href = vendorCssHref();
  if (!href) return;
  const current = typeof link.getAttribute === "function" ? link.getAttribute("href") : link.href;
  if (current === href) return;
  if (typeof link.setAttribute === "function") link.setAttribute("href", href);
  else link.href = href;
}

function vendorEsmBase() {
  const base = typeof window !== "undefined" ? window.JMODGUARD_ESM_BASE : "";
  return String(base || VENDOR.esm).replace(/\/$/, "");
}

function vendorWidgetsEnabled() {
  try {
    if (typeof window === "undefined" || window.JMODGUARD_VENDOR === "off") return false;
    return typeof HTMLElement === "function" && document.createElement("div") instanceof HTMLElement;
  } catch (_) {
    return false;
  }
}

function hasClass(node, name) {
  if (node?.classList?.contains?.(name)) return true;
  return String(node?.className || "").split(/\s+/).includes(name);
}

function shouldEnhanceSelect(select) {
  if (!select) return false;
  const tag = String(select.tagName || select.tag || "").toLowerCase();
  if (tag !== "select") return false;
  if (hasClass(select, "theme-select") || hasClass(select, "search-select-native")) return false;
  return select.id === "guild-select" || hasClass(select, "searchable");
}

let vendorRuntimePromise = null;

function vendorModule(mod, names) {
  if (!mod) return null;
  const usable = (value) => typeof value === "function" || (value && typeof value === "object");
  const has = (obj) => obj && names.every((name) => usable(obj[name]));
  if (has(mod)) return mod;
  if (has(mod.default)) return Object.assign({}, mod, mod.default);
  return null;
}

function loadSearchSelectRuntime() {
  if (!vendorRuntimePromise) {
    const esm = vendorEsmBase();
    const reactV = VENDOR.react;
    const headlessV = VENDOR.headless;
    vendorRuntimePromise = Promise.all([
      import(`${esm}/react@${reactV}`),
      import(`${esm}/react-dom@${reactV}/client?deps=react@${reactV}`),
      import(`${esm}/@headlessui/react@${headlessV}?deps=react@${reactV},react-dom@${reactV}`),
    ]).then(([reactMod, reactDomMod, headlessMod]) => {
      const React = vendorModule(reactMod, ["createElement", "useState"]);
      const ReactDOM = vendorModule(reactDomMod, ["createRoot"]);
      const Headless = vendorModule(headlessMod, ["Combobox", "ComboboxInput", "ComboboxOptions", "ComboboxOption"]);
      if (!React || !ReactDOM || !Headless) {
        throw new Error("vendor runtime missing Combobox exports");
      }
      return { React, ReactDOM, Headless };
    });
  }
  return vendorRuntimePromise;
}

function selectOptions(select) {
  return [...(select.options || [])].map((option) => ({
    value: option.value,
    label: option.textContent || option.label || option.value,
  }));
}

function makeSearchSelect(React, Headless, select) {
  const useState = React.useState;
  const Combobox = Headless.Combobox;
  const ComboboxInput = Headless.ComboboxInput;
  const ComboboxButton = Headless.ComboboxButton;
  const ComboboxOptions = Headless.ComboboxOptions;
  const ComboboxOption = Headless.ComboboxOption;
  return function SearchSelect() {
    const [query, setQuery] = useState("");
    const [rev, setRev] = useState(0);
    select._searchRefresh = () => setRev((n) => n + 1);
    const items = selectOptions(select);
    void rev;
    const selected = items.find((item) => item.value === select.value) || null;
    const q = query.trim().toLowerCase();
    const filtered = !q ? items : items.filter((item) => item.label.toLowerCase().includes(q) || item.value.toLowerCase().includes(q));
    const labeledId = select.dataset?.searchSelectId || "";
    return React.createElement(Combobox, {
      value: selected,
      by: "value",
      onChange: (item) => {
        if (!item) return;
        select.value = item.value;
        select.dispatchEvent(new Event("input", { bubbles: true }));
        select.dispatchEvent(new Event("change", { bubbles: true }));
      },
      onClose: () => setQuery(""),
    },
      React.createElement("div", { className: `select select-bordered w-full search-select-control${select.closest(".server-picker") ? " select-sm" : ""}` },
        React.createElement(ComboboxInput, {
          id: labeledId || undefined,
          className: "search-select-input",
          "aria-label": labeledId ? undefined : (select.getAttribute("aria-label") || "Search options"),
          displayValue: (item) => item?.label || "",
          onChange: (event) => setQuery(event.target.value),
        }),
        ComboboxButton ? React.createElement(ComboboxButton, { className: "search-select-button", "aria-label": "Show options" }, "▾") : null,
      ),
      React.createElement(ComboboxOptions, {
        className: "search-select-options menu bg-base-100 rounded-box shadow-lg",
        anchor: "bottom start",
      },
        filtered.length
          ? filtered.map((item) => React.createElement(ComboboxOption, {
              key: `${item.value}:${item.label}`,
              value: item,
              className: "search-select-option",
            }, item.label))
          : React.createElement("div", { className: "search-select-empty" }, "No matches"),
      ),
    );
  };
}

function restoreNativeSelect(select, wrap, labeledId) {
  select._vendorUnmount = null;
  select._searchRefresh = null;
  select.classList.remove("search-select-native");
  select.removeAttribute("aria-hidden");
  select.tabIndex = 0;
  const id = labeledId || select.dataset?.searchSelectId || "";
  if (id && !select.id) select.id = id;
  if (select.dataset) {
    delete select.dataset.searchSelectId;
    select.dataset.searchEnhanced = "";
  }
  if (wrap && wrap.parentNode && select.parentNode === wrap) {
    wrap.parentNode.insertBefore(select, wrap);
    wrap.remove();
  }
}

function mountSearchSelect(select, runtime) {
  if (typeof document === "undefined" || !select.parentNode) return;
  const wrap = document.createElement("div");
  wrap.className = "search-select";
  const host = document.createElement("div");
  host.className = "search-select-host";
  const labeledId = select.id || "";
  try {
    select.parentNode.insertBefore(wrap, select);
    wrap.append(select, host);
    select.classList.add("search-select-native");
    select.setAttribute("aria-hidden", "true");
    select.tabIndex = -1;
    if (labeledId) {
      select.dataset.searchSelectId = labeledId;
      select.removeAttribute("id");
    }
    const App = makeSearchSelect(runtime.React, runtime.Headless, select);
    const root = runtime.ReactDOM.createRoot(host);
    root.render(runtime.React.createElement(App));
    select._vendorUnmount = () => {
      try { root.unmount(); } catch (_) { /* already detached */ }
      restoreNativeSelect(select, wrap, labeledId);
    };
  } catch (err) {
    restoreNativeSelect(select, wrap, labeledId);
    throw err;
  }
}

function listedSelects(root) {
  const selects = [];
  if (!root) return selects;
  const tag = String(root.tagName || root.tag || "").toLowerCase();
  if (tag === "select") selects.push(root);
  const listed = root.querySelectorAll?.("input, select, textarea");
  if (listed) {
    for (const node of listed) {
      if (String(node.tagName || node.tag || "").toLowerCase() === "select") selects.push(node);
    }
  }
  return selects;
}

function unmountVendorWidgets(root) {
  listedSelects(root).forEach((select) => {
    const unmount = select._vendorUnmount;
    if (unmount) {
      try { unmount(); } catch (_) { /* host already gone */ }
    }
    if (select.dataset) select.dataset.searchEnhanced = "";
  });
}

function enhanceVendorWidgets(root) {
  if (!vendorWidgetsEnabled() || !root) return;
  listedSelects(root).forEach((select) => {
    if (select.dataset?.searchEnhanced === "1" || select._vendorUnmount) {
      if (select._searchRefresh) select._searchRefresh();
      return;
    }
    if (!shouldEnhanceSelect(select)) return;
    if (select.dataset) select.dataset.searchEnhanced = "1";
    loadSearchSelectRuntime().then((runtime) => {
      if (select.dataset?.searchEnhanced !== "1") return;
      if (select.isConnected === false) {
        if (select.dataset) select.dataset.searchEnhanced = "";
        return;
      }
      try {
        mountSearchSelect(select, runtime);
      } catch (_) {
        if (select.dataset) select.dataset.searchEnhanced = "";
      }
    }).catch(() => {
      if (select.dataset) select.dataset.searchEnhanced = "";
      const unmount = select._vendorUnmount;
      if (unmount) {
        try { unmount(); } catch (_) { /* leave native select */ }
      }
    });
  });
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

function parseRetryAfterMs(header) {
  if (!header) return 0;
  const seconds = Number(header);
  if (!Number.isFinite(seconds) || seconds < 0) return 0;
  return Math.min(seconds, 30) * 1000;
}

function isUnauthenticated(err) {
  const msg = String(err && err.message);
  return err.status === 401 || msg.includes("not authenticated") || msg === "session expired" || msg.includes("401");
}

/** Cached GET /api/me must not keep a write UI after the session is gone. */
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

/** Pre-axum JSON warmup, not arbitrary proxy/origin 503s. */
function isApiWarmup(err) {
  return err.status === 503 && String(err && err.message) === "not ready";
}

/** Origin has not handed off to axum yet.
 *  Warmup JSON, Bunny 502/504, a dropped fetch, or a proxy HTML 503
 *  (`api()` reports those as "HTTP 503"). A JSON 503 with a different
 *  `error` field is an application response and must not spin. */
function isOriginStarting(err) {
  if (isApiWarmup(err)) return true;
  if (!err || !err.status) return true;
  if (err.status === 502 || err.status === 504) return true;
  return err.status === 503 && String(err && err.message) === "HTTP 503";
}

/** Keep retrying API `not ready` longer than the remote open window (~6 min). */
const API_WARMUP_BUDGET_MS = 8 * 60 * 1000;
/** Misrouted / dead origin should not look like warmup forever. */
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

function showView(id) {
  ["startup-view", "login-view", "main-view"].forEach((name) => {
    const node = $("#" + name);
    if (node) node.classList.toggle("hidden", name !== id);
  });
}

function setStartupStatus(text) {
  const node = $("#startup-status");
  if (node) node.textContent = text;
}

function showPreviewUnavailable() {
  showView("startup-view");
  setStartupStatus("This preview is no longer signed in. Return to the owner dashboard and launch a new preview.");
  const link = $("#preview-return-link");
  if (link) link.classList.remove("hidden");
}

function returnFromPreview() {
  clearPreviewToken();
}

function pageEnrollment(path, method = "GET") {
  // Bind edits and scoped reads to the membership this page loaded. Never
  // fetch a new token or retry against a replacement installation.
  const edit = path.match(/^\/api\/guilds\/(\d+)\/(settings|rules|badwords|commands|templates|copy|logchannels|logfilters|unban|actions|events|members|member-sync|bans|stats|cases|case-delete-roles)(?:\/|\?|$)/);
  if (edit && (method !== "GET" || ["rules", "badwords", "commands", "templates", "logchannels", "actions", "events", "members", "member-sync", "bans", "stats", "cases", "case-delete-roles"].includes(edit[2])) && edit[1] !== "0" && edit[1] === state.guildId && state.config?.enrollment_id) {
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
  const msg = (data && (data.error || data.message)) ? (data.error || data.message) : `HTTP ${status}`;
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
    if (body !== null) { opts.headers["content-type"] = "application/json"; opts.body = JSON.stringify(body); }
    const res = await fetch(apiUrl(path), opts);
    const text = await res.text();
    let data = null;
    try { data = text ? JSON.parse(text) : null; } catch (_) { data = text; }
    if (!res.ok) throwApiFailure(data, res.status, parseRetryAfterMs(res.headers.get("retry-after")));
    return data;
  } catch (e) {
    if (e && (e.name === "AbortError" || e.name === "TimeoutError")) {
      throw apiError(signal ? "Failed to fetch" : "Request timed out. Refresh to verify saved state before trying again.");
    }
    throw e;
  } finally { if (timeout !== null) clearTimeout(timeout); }
}

/** Workspace JSON reads use POST so a CDN cannot reuse a cached GET. */
function apiRead(path, signal) {
  return api(path, "POST", null, signal);
}

async function downloadPrivate(path, filename) {
  const opts = credentialedOpts(path, "GET");
  const res = await fetch(apiUrl(path), opts);
  if (!res.ok) {
    let data = null;
    try { data = JSON.parse(await res.text()); } catch (_) {}
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

// Persist an unacknowledged request in this browser tab so a lost response or
// page reload cannot silently create a second intent. Accepted work is visible
// in Bot actions; this is admission feedback, not a Discord success claim.
async function submitBotAction(path, method, body) {
  const canonicalBody = Object.fromEntries(Object.keys(body).sort().map(key => [key, body[key]]));
  const key = `${workspaceKey()}:action:${method}:${path}:${JSON.stringify(canonicalBody)}`;
  const enrollmentId = state.config?.enrollment_id;
  const guildId = path.split("/")[3];
  if (!enrollmentId || guildId !== state.guildId) throw new Error("Reload this server before requesting an action.");
  let saved;
  try {
    const raw = window.sessionStorage.getItem(key);
    saved = raw ? JSON.parse(raw) : { request_id: `${Math.floor(Date.now() / 1000)}_${crypto.randomUUID()}`, enrollment_id: enrollmentId };
    window.sessionStorage.setItem(key, JSON.stringify(saved));
  } catch (_) {
    throw new Error("Browser session storage must be available and valid to safely submit bot actions.");
  }
  if (!saved || typeof saved.request_id !== "string" || saved.enrollment_id !== enrollmentId) {
    if (confirm("This saved request belongs to an earlier bot membership. Check Discord for its outcome. Clear the saved request without submitting another action?")) {
      window.sessionStorage.removeItem(key);
    }
    throw new Error("Saved request was not submitted. Review the current server before making a new request.");
  }
  const requestId = saved.request_id;
  let job;
  try {
    job = await api(path, method, { ...body, request_id: requestId });
  } catch (error) {
    if (((error.status === 410 && error.code === "action_request_expired") ||
         (error.status === 400 && error.code === "action_request_invalid") ||
         (error.status === 409 && error.code === "action_enrollment_changed")) &&
        confirm("The earlier action may already have run, and its result may no longer be listed. Have you checked Bot actions and Discord, and want to clear this saved request? This will not submit another action.")) {
      window.sessionStorage.removeItem(key);
      throw new Error("Saved request cleared. Review the current state, then submit again only if another action is needed.");
    }
    throw error;
  }
  if (!job || typeof job.id !== "string" || job.guild_id !== guildId || job.actor_id !== state.me.user.id || job.enrollment_id !== enrollmentId || !["pending", "claimed", "running", "succeeded", "failed", "uncertain", "cancelled", "expired"].includes(job.state)) {
    throw new Error("Action acknowledgment was invalid. Check Bot actions; retrying here will reuse the same request ID.");
  }
  try { window.sessionStorage.removeItem(key); } catch (_) { /* Retaining the same ID is safe. */ }
  const completed = job.state === "succeeded";
  const waiting = ["pending", "claimed", "running"].includes(job.state);
  toast(completed ? "Bot action completed. See Bot actions for details." :
    waiting ? "Action queued. Check Bot actions for the result." :
      `Action ${job.state}: ${job.outcome || "Check Bot actions for details."}`,
    completed ? "success" : waiting ? "info" : "error");
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
  const drafts = currentDrafts();
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

/** Reflect an acknowledged log-channel intent without claiming that queued bot
 *  validation has completed. Replacements retain the last authoritative value
 *  so a cancelled/failed newer request cannot resurrect an older queued value. */
function applyQueuedLogChannel(job) {
  if (!actionIsLogChannel(job) || job.guild_id !== state.guildId ||
      job.enrollment_id !== state.config?.enrollment_id) return false;
  const row = logChannelRow(job.action.event);
  if (!row) return false;
  const active = ["pending", "claimed", "running"].includes(job.state);
  const terminal = ["succeeded", "failed", "uncertain", "cancelled", "expired"].includes(job.state);
  if (!active && !terminal) return false;
  const key = logChannelActionKey(job);
  const existing = pendingLogChannelActions.get(key);
  if (existing && existing.job.id !== job.id && terminal &&
      ["pending", "claimed", "running"].includes(existing.job.state)) {
    // A retained request key can replay an older terminal receipt after a
    // newer request has become this row's active owner. Keep watching newer.
    return true;
  }
  if (existing && existing.job.id !== job.id && existing.timer !== null) clearTimeout(existing.timer);
  const previous = existing?.previous || row.pending_action?.previous || {
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
    toast("Log-channel validation is still pending. Its requested value remains marked queued; see Bot actions for details.", "info");
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

/** Reconcile only the action the current page admitted. The Actions tab remains
 *  manual; this bounded watcher exists so a save does not require a second
 *  configuration refresh. */
async function reconcileQueuedLogChannel(original, attempt = 0) {
  const key = logChannelActionKey(original);
  let entry = pendingLogChannelActions.get(key);
  let row = logChannelRow(original.action.event);
  if (!entry || entry.job.id !== original.id || !row?.pending_action || row.pending_action.id !== original.id ||
      original.guild_id !== state.guildId || original.enrollment_id !== state.config?.enrollment_id) return;
  const originalTerminal = ["succeeded", "failed", "uncertain", "cancelled", "expired"].includes(original.state);
  let job = original;
  if (!originalTerminal) {
    const jobs = await apiRead(`/api/guilds/${original.guild_id}/actions`);
    job = jobs.find((candidate) => candidate.id === original.id);
    entry = pendingLogChannelActions.get(key);
    row = logChannelRow(original.action.event);
    if (!entry || entry.job.id !== original.id || !row?.pending_action || row.pending_action.id !== original.id ||
        original.guild_id !== state.guildId || original.enrollment_id !== state.config?.enrollment_id) return;
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
    authoritative = await apiRead(`/api/guilds/${original.guild_id}/logchannels/${encodeURIComponent(original.action.event)}`);
  } catch (error) {
    if (isUnauthenticated(error)) return;
    renderLogChannelsPreservingDrafts();
    scheduleLogChannelReconcile(job, attempt + 1);
    return;
  }
  entry = pendingLogChannelActions.get(key);
  row = logChannelRow(original.action.event);
  if (!entry || entry.job.id !== original.id || !row?.pending_action ||
      original.guild_id !== state.guildId || original.enrollment_id !== state.config?.enrollment_id) return;
  if (row.pending_action?.id !== original.id) return;
  row.effective = authoritative.effective;
  row.guild_override = authoritative.guild_override;
  clearPendingLogChannelEntry(key);
  row.pending_action = null;
  renderLogChannelsPreservingDrafts();
  toast(`Log-channel action ${job.state}: ${job.outcome || "the saved mapping was not changed"}`, "error");
}

/** CDN Storage can serve this page while the API origin is still binding or
 *  opening the database. Keep retrying `not ready` through the remote open
 *  window. Consecutive 502/network failures stop after
 *  ORIGIN_REACHABILITY_BUDGET_MS so a bad Pull Zone rule is not an infinite
 *  spinner. Each class has its own clock: a late gateway blip must not spend
 *  warmup elapsed time, and one `not ready` must not inherit eight minutes
 *  for a dead origin. Each attempt is aborted after ORIGIN_REACHABILITY_BUDGET_MS
 *  (not leftover consecutive time) so a pending origin pull cannot stall
 *  Date.now(), while a slow warmup 503 after 502s can still be classified. */
async function waitForApiMe() {
  let warmupStarted = null;
  let reachabilityStarted = null;
  let delay = 500;
  let sawStartup = false;
  while (true) {
    const attemptStarted = Date.now();
    const controller = new AbortController();
    const timer = setTimeout(
      () => controller.abort(),
      ORIGIN_REACHABILITY_BUDGET_MS,
    );
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

function toast(msg, kind = "info") {
  const t = $("#toast");
  t.textContent = msg;
  t.className = "show " + kind;
  clearTimeout(t._timer);
  t._timer = setTimeout(() => (t.className = ""), kind === "error" ? 5000 : 2600);
}

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

async function init() {
  try {
    const me = await waitForApiMe();
    state.me = me;
    state.guilds = me.guilds;
    showView("main-view");
    $("#user-chip").textContent = `${me.user.username} (${me.user.global_name || ""})`.trim();
    const sel = $("#guild-select");
    sel.innerHTML = "";
    me.guilds.forEach((g) => {
      sel.append(el("option", { value: g.id, text: g.name }));
    });
    restoreWorkspace();
    sel.value = state.guildId || "";
    if (!state.guildId) sel.append(el("option", { value: "", text: "No manageable servers" }));
    sel.addEventListener("change", async () => {
      if (!canLeave()) { sel.value = state.guildId || ""; if (sel._searchRefresh) sel._searchRefresh(); return; }
      pendingDrafts = null;
      state.guildId = sel.value;
      state.config = null;
      dirty = false;
      persistWorkspace();
      if (!await loadConfig()) return;
      renderTab();
    });
    enhanceVendorWidgets(sel);
    if (!await loadConfig()) return;
    renderTab();
    bindOwnerTools();
  } catch (e) {
    if (handleAuthLoss(e)) {
      /* Login / preview-unavailable view already shown. */
    } else {
      showView("startup-view");
      setStartupStatus("Failed to load: " + (e.message || String(e)));
      toast("Failed to load: " + e.message, "error");
    }
  }
}

async function loadConfig() {
  const guildId = state.guildId;
  const request = ++configRequest;
  if (!guildId) { renderTab(); return; }
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

/** Apply the exact representation acknowledged by a configuration mutation.
 *  This keeps ordinary saves to one request while preserving the server's
 *  validated/normalized value. Complex multi-section writes still reload. */
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
    else if (patch.operation === "delete") config.badwords = config.badwords.filter((word) => word.id !== patch.id);
    else return false;
    return true;
  }
  if (patch.kind === "command" && patch.value) {
    config.commands ||= [];
    let row = config.commands.find((command) => command.name === patch.name);
    if (!row) {
      row = { name: patch.name, scope: patch.scope, effective: patch.value, guild_override: null, global: null };
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
        config.settings_override[field] = patch[field] == null || patch.settings[field] === parent
          ? null
          : patch.settings[field];
      }
      if (patch.bot_name_present) config.nickname_reconcile = patch.nickname_reconcile;
    }
    return true;
  }
  return false;
}

/** Persist a mutation, apply its authoritative representation when available,
 *  otherwise reload this server's complete config. Errors go to handleEvent. */
async function reloadAfter(work, onSuccess) {
  const res = await work();
  if (!applyConfigPatch(res) && !await loadConfig()) return;
  if (onSuccess) onSuccess(res);
  renderTab();
}

// ---------------------------------------------------------------------------
// tabs
// ---------------------------------------------------------------------------

function renderTab() {
  const map = {
    dashboard: tabDashboard, rules: tabRules, badwords: tabBadwords, commands: tabCommands,
    templates: tabTemplates, logchannels: tabLogChannels,
    events: tabEvents, members: tabMembers, bans: tabBans, actions: tabActions, cases: tabCases, settings: tabSettings,
  };
  document.querySelectorAll("#tabs button").forEach((b) => {
    const on = b.dataset.tab === state.tab;
    b.classList.toggle("active", on);
    b.classList.toggle("menu-active", on);
    b.classList.toggle("btn-active", on);
    if (on) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  });
  const fn = map[state.tab] || tabDashboard;
  const content = $("#content");
  if (disposeMembersView) { disposeMembersView(); disposeMembersView = null; }
  unmountVendorWidgets(content);
  content.innerHTML = "";
  dirty = false;
  const guild = selectedGuild();
  const name = $("#workspace-name");
  if (name) name.textContent = guild?.name || "Your servers";
  if (!state.guildId) {
    content.append(emptyPanel("No servers to manage yet", "Sign in with an account that has Manage Server permission in a server using jModGuard."));
  } else if (!state.config) {
    content.append(emptyPanel(
      state.loading ? "Loading your workspace…" : "Configuration unavailable",
      state.error || "Fetching saved settings for this server.",
      !state.loading ? el("button", { class: "btn primary", text: "Retry", onclick: async () => { if (!await loadConfig()) return; renderTab(); } }) : null,
      { role: "status" },
    ));
  } else {
    content.append(fn());
    content.querySelectorAll("table").forEach((table) => {
      table.classList.add("table");
      if (!table.classList.contains("commands-table")) table.classList.add("table-zebra");
      const wrap = el("div", { class: "table-scroll rounded-box bg-base-100", tabindex: "0", role: "region", "aria-label": "Scrollable data table" });
      table.replaceWith(wrap); wrap.append(table);
    });
    content.querySelectorAll("input, select, textarea").forEach((input, i) => {
      input.dataset.inputKey = String(i);
      input._initialValue = input.value;
    });
    content.querySelectorAll(".field").forEach((field, i) => {
      const label = field.querySelector("label");
      const input = field.querySelector("input, select, textarea");
      if (label && input) { input.id ||= `field-${i}`; label.htmlFor = input.id; }
    });
  }
  restoreDrafts();
  enhanceVendorWidgets(content);
  enhanceVendorWidgets($("#guild-select"));
  updateWorkspaceStatus();
  const banner = $("#preview-banner");
  if (banner) banner.classList.toggle("hidden", !isPreview());
  if (isPreview()) {
    const status = $("#workspace-status");
    if (status) status.textContent = "Preview — changes disabled";
  }
}

function bindOwnerTools() {
  const tools = $("#owner-tools");
  if (!tools) return;
  if (isPreview()) {
    tools.classList.remove("hidden");
    tools.replaceChildren(el("button", { class: "btn small", text: "Exit preview", onclick: async () => {
      try { await api("/api/preview", "DELETE"); } catch (_) {}
      clearPreviewToken();
      window.location = "/";
    } }));
    return;
  }
  if (!isBotOwner()) {
    tools.classList.add("hidden");
    return;
  }
  tools.classList.remove("hidden");
  const profile = el("select", { "aria-label": "Preview profile" },
    el("option", { value: "administrator", text: "Administrator" }),
    el("option", { value: "moderator", text: "Moderator" }),
    el("option", { value: "member", text: "Member" }),
  );
  const children = [
    el("a", { class: "btn small", href: "/administration/index.html", text: "Administration" }),
    el("button", { class: "btn small", text: "View as…", onclick: async () => {
      if (!state.guildId) { toast("Select a server first", "error"); return; }
      const launched = await api("/api/preview", "POST", { guild_id: String(state.guildId), profile: profile.value });
      // Fragments are not sent in HTTP requests or Referer headers.
      const url = `${location.pathname}#preview=${encodeURIComponent(launched.token)}`;
      window.open(url, "_blank", "noopener");
    } }),
    profile,
  ];
  if (state.me.conductor_url) {
    children.push(el("a", { class: "btn small", href: state.me.conductor_url, target: "_blank", rel: "noopener noreferrer", text: "Legacy conductor" }));
  }
  tools.replaceChildren(...children);
}

function bindTabButtons() {
  document.querySelectorAll("#tabs button").forEach((b) => b.addEventListener("click", () => {
    navigate(b.dataset.tab);
  }));
}

function emptyPanel(title, body, action, attrs) {
  return el("div", { class: "hero bg-base-200 rounded-box", ...(attrs || {}) },
    el("div", { class: "hero-content text-center" },
      el("div", { class: "hero-copy" },
        el("h2", { text: title }),
        el("p", { text: body }),
        action || null)));
}

function field(label, control) {
  control.setAttribute("aria-label", label);
  return el("div", { class: "field" }, el("label", {}, label, control));
}

function eventHeading(event) {
  return el("div", { class: "event-heading" },
    el("h3", { text: event.replaceAll("_", " ").replace(/^./, (c) => c.toUpperCase()) }),
    el("code", { class: "muted", text: event }));
}

function statsRow(...items) {
  return el("div", { class: "stats bg-base-100 shadow w-full" }, ...items);
}

function tabDashboard() {
  const g = selectedGuild();
  const cfg = state.config;
  const rulesOn = cfg.rules.filter((r) => r.effective.enabled).length;
  const rulesDry = cfg.rules.filter((r) => r.effective.dry_run).length;
  const commandsOn = cfg.commands.filter((c) => c.effective.enabled).length;
  const chans = cfg.log_channels.filter((l) => l.effective && String(l.effective) !== "0").length;
  const moderation = [['events', 'Moderation records'], ['warns', 'Warns'], ['bans', 'Bans'], ['cases', 'Cases']]
    .map(([key, label]) => ({ key, value: el("div", { class: "stat-value", text: "Loading…" }), label }));
  const guildId = state.guildId;
  apiRead(`/api/guilds/${guildId}/stats`).then(counts => {
    moderation.forEach(({ key, value }) => { value.textContent = counts[key] == null ? 'Unavailable' : String(counts[key]); });
  }).catch(() => {
    moderation.forEach(({ value }) => { value.textContent = 'Unavailable'; });
  });
  return el("div", { class: "contents" },
    el("section", { class: "hero dashboard-hero" },
      el("div", { class: "hero-content" },
        el("div", { class: "hero-copy" },
          el("span", { class: "eyebrow", text: "COMMUNITY AT A GLANCE" }),
          el("h2", { text: g ? `${g.name}, under your care.` : "Your moderation workspace" }),
          el("p", { text: "Review your server’s protection, configure your tools, and keep track of moderation activity." }),
          el("div", { class: "row" },
            el("button", { class: "btn primary", text: "Review protection →", onclick: () => navigate("rules") }),
            el("button", { class: "btn outline", text: "View audit log", onclick: () => navigate("events") }))))),
    el("section", { class: "card getting-started" },
      el("h3", { text: "Getting started" }),
      el("p", { class: "muted", text: "Set up protection, choose your tools, then decide where reports go." }),
      el("div", { class: "setup-links" },
        ...[["01", "Tune automod", "Review rules and test them in dry-run.", "rules"],
          ["02", "Configure commands", "Choose commands and moderator permissions.", "commands"],
          ["03", "Route your logs", "Connect event reports to Discord channels.", "logchannels"]].map(([number, title, description, tab]) =>
          el("button", { class: "setup-link", onclick: () => navigate(tab) },
            el("span", { class: "setup-number", text: number }),
            el("span", {}, el("strong", { text: title }), el("span", { text: description })),
            el("span", { "aria-hidden": "true", text: "→" })))),
      el("p", { class: "setup-note", text: "Use each editor’s Save action to apply changes. The bot usually picks up configuration changes within a few seconds. " +
        (isBotOwner() ? "As bot owner, you can also edit Global defaults for all servers." : "Global defaults can only be edited by the bot owner.") })),
    el("section", { class: "dashboard-section" },
      el("h3", { text: "Protection & tools" }),
      statsRow(stat("Rules enabled", `${rulesOn} / ${cfg.rules.length}`), stat("Rules in dry-run", rulesDry),
        stat("Commands enabled", `${commandsOn} / ${cfg.commands.length}`), stat("Log channels", chans))),
    el("section", { class: "dashboard-section" },
      el("h3", { text: "Moderation activity" }),
      statsRow(...moderation.map(({ value, label }) => el("div", { class: "stat" }, el("div", { class: "stat-title", text: label }), value))),
      el("aside", { class: "dashboard-note" },
        el("strong", { text: "About these counts" }),
        el("p", { text: "Moderation records exclude routine history and older plan summaries whose action type was not recorded. Those summaries remain in the Audit Log." }))),
    el("section", { class: "dashboard-section" },
      el("h3", { text: "Server configuration" }),
      statsRow(stat("Prefix", cfg.settings.prefix), stat("Bot name", cfg.settings.bot_name || "default"),
        stat("Badword patterns", cfg.badwords.length), stat("Routine chat collection", "Off"))),
  );
}

function stat(label, value) {
  return el("div", { class: "stat" },
    el("div", { class: "stat-title", text: label }),
    el("div", { class: "stat-value", text: value }));
}

// ---------------------------------------------------------------------------
// rules
// ---------------------------------------------------------------------------

function tabRules() {
  const rules = state.config.rules;
  return el("div", {},
    el("h2", { text: "Automod rules" }),
    el("p", { class: "muted", style: "margin-bottom:14px" },
      "Each rule has its own parameters, whitelists, and an escalation table " +
      "(`at` = strike count at which the action fires; the highest-severity fired action is executed). " +
      "Exact image and repeated-text rules use separate matchers: dry-run logs matches only, " +
      "while normal mode deletes matched messages and bans. Use dry-run while tuning."),
    ...rules.map((r) => ruleCard(r)),
  );
}

function ruleCard(r) {
  const rule = r.effective;
  const spam = spamRule(r.rule_id);
  const supported = !spam || spamConfigured(r.rule_id, rule);
  const badge = !rule.enabled || !supported ? "off" : rule.dry_run ? "dry" : "on";
  const label = !rule.enabled ? "disabled" : !supported ? "inactive — convert this rule" : rule.dry_run ? "dry-run" : spam ? "delete and ban" : "enabled";
  return el("div", { class: "card" },
    el("div", { class: "row", style: "justify-content:space-between" },
      el("div", { class: "rule-heading" },
        el("h3", { text: r.name }),
        el("div", { class: "tag-group", "aria-label": "Rule status and source" },
          el("span", { class: `tag ${badge}`, text: label }),
          r.guild_override ? el("span", { class: "tag override", text: "server override" }) : null,
          r.global ? el("span", { class: "tag scope", text: "global" }) : null)),
      el("div", { class: "card-actions" },
        el("button", { class: "btn small", onclick: () => editRule(r) }, "Edit")),
    ),
    el("p", { text: r.description }),
  );
}

function spamRule(id) {
  return id === "attachment_flood" || id === "repeat_text";
}

function imageTestConfigured(rule) {
  return spamConfigured("attachment_flood", Object.assign({}, rule, { dry_run: true, params: Object.assign({ mode: "exact_image_test" }, rule.params || {}), actions: [{ action: "log" }] })) ||
    (rule.params || {}).mode === "exact_image_test" && rule.dry_run === true &&
    (rule.actions || []).every(a => a.action === "log") &&
    spamThresholds(rule.params, "attachment_flood");
}

function spamConfigured(id, rule) {
  const p = rule.params || {};
  if (id === "attachment_flood") {
    const test = (p.mode === "exact_image_test" || (p.mode === "exact_image" && rule.dry_run === true)) &&
      (rule.actions || []).every(a => a.action === "log") && rule.dry_run === true;
    const enforce = p.mode === "exact_image" && rule.dry_run === false &&
      (rule.actions || []).some(a => a.action === "delete") &&
      (rule.actions || []).some(a => a.action === "ban") &&
      (rule.actions || []).every(a => ["delete", "ban", "log"].includes(a.action));
    return (test || enforce) && spamThresholds(p, id);
  }
  if (id === "repeat_text") {
    const test = p.mode === "exact_text" && rule.dry_run === true && (rule.actions || []).every(a => a.action === "log");
    const enforce = p.mode === "exact_text" && rule.dry_run === false &&
      (rule.actions || []).some(a => a.action === "delete") &&
      (rule.actions || []).some(a => a.action === "ban") &&
      (rule.actions || []).every(a => ["delete", "ban", "log"].includes(a.action));
    return (test || enforce) && spamThresholds(p, id);
  }
  return false;
}

function spamThresholds(p, id) {
  const keys = id === "repeat_text"
    ? ["mode", "window_secs", "min_channels", "min_length"]
    : ["mode", "window_secs", "min_channels", "min_attachments"];
  if (!Object.keys(p).every(k => keys.includes(k))) return false;
  const ranges = id === "repeat_text"
    ? [[p.window_secs, 1, 600], [p.min_channels, 2, 10], [p.min_length, 1, 2000]]
    : [[p.window_secs, 1, 600], [p.min_channels, 2, 10], [p.min_attachments, 1, 10]];
  return ranges.every(([n, min, max]) => Number.isInteger(n) && n >= min && n <= max);
}

function editRule(r) {
  const rule = JSON.parse(JSON.stringify(r.effective));
  const spam = spamRule(r.rule_id);
  const textRule = r.rule_id === "repeat_text";
  const image = r.rule_id === "attachment_flood";
  const imageChannels = el("input", { type: "number", min: 2, max: 10, value: rule.params?.min_channels ?? 3 });
  const imageCount = el("input", { type: "number", min: 1, max: 10, value: rule.params?.min_attachments ?? 2 });
  const imageWindow = el("input", { type: "number", min: 1, max: 600, value: rule.params?.window_secs ?? 60 });
  const textLength = el("input", { type: "number", min: 1, max: 2000, value: rule.params?.min_length ?? 20 });
  // Inactive legacy image rows should convert to the safe dry-run mode by default.
  const dryRun = el("input", { type: "checkbox", checked: spam ? !spamConfigured(r.rule_id, rule) || rule.dry_run : rule.dry_run,
    onchange: (e) => (rule.dry_run = e.target.checked) });
  const modal = overlay();
  const paramsText = el("textarea", { class: "mono", rows: 7, spellcheck: "false" });
  paramsText.value = JSON.stringify(rule.params ?? {}, null, 2);
  const actionsRows = el("div", {});
  const whitelistText = el("input", { type: "text", value: (rule.whitelist_roles || []).join(",") });
  const channelsText = el("input", { type: "text", value: (rule.whitelist_channels || []).join(",") });
  const usersText = el("input", { type: "text", value: (rule.whitelist_users || []).join(",") });
  const scopeSel = scopeSelect();

  const renderActions = () => {
    unmountVendorWidgets(actionsRows);
    actionsRows.innerHTML = "";
    rule.actions.forEach((a, i) => {
      const at = el("input", { type: "number", min: 1, value: a.at });
      const action = el("select", { class: "searchable" },
        ["log", "delete", "warn", "timeout", "kick", "ban"].map((x) => el("option", { value: x, text: x, selected: a.action === x })),
      );
      const dur = el("input", { type: "number", min: 0, value: a.duration_minutes ?? 0, title: "minutes (timeout)" });
      const dd = el("input", { type: "number", min: 0, max: 7, value: a.delete_days ?? 0, title: "days of messages deleted (ban)" });
      const reason = el("input", { type: "text", value: a.reason || "", placeholder: "reason template" });
      actionsRows.append(el("div", { class: "escalation-step" },
        el("div", { class: "row title-row" },
          el("strong", { text: `Action ${i + 1}` }),
          el("button", { class: "btn small outline", "aria-label": `Remove action ${i + 1}`, onclick: () => { rule.actions.splice(i, 1); markDirty(); renderActions(); } }, "Remove")),
        el("div", { class: "escalation-fields" },
          field("Strike count", at), field("Action", action),
          field("Timeout (minutes)", dur), field("Delete history (days)", dd)),
        field("Reason template", reason),
      ));
      at.onchange = () => (rule.actions[i].at = Math.max(1, parseInt(at.value) || 1));
      action.onchange = () => { rule.actions[i].action = action.value; renderActions(); };
      dur.onchange = () => (rule.actions[i].duration_minutes = parseInt(dur.value) || 0);
      dd.onchange = () => { rule.actions[i].delete_days = Math.min(7, Math.max(0, parseInt(dd.value) || 0)); };
      reason.onchange = () => (rule.actions[i].reason = reason.value);
    });
    enhanceVendorWidgets(actionsRows);
  };
  if (!spam) renderActions();

  const box = el("div", { class: "modal-box rule-editor" },
    el("div", { class: "editor-heading" },
      el("span", { class: "eyebrow", text: "AUTOMOD RULE" }),
      el("h3", { text: `Edit rule: ${r.name}` }),
      el("p", { class: "muted", text: r.description || "Configure detection, actions and exemptions for this rule." })),
    el("div", { class: "row checkbox-group" },
      el("label", { class: "checkbox-label" },
        el("input", { type: "checkbox", checked: rule.enabled, onchange: (e) => (rule.enabled = e.target.checked) }), "Enabled"),
      el("label", { class: "checkbox-label" }, dryRun, "Dry-run (log only)"),
    ),
    spam ? el("div", {},
      el("p", { text: textRule
        ? "Exact text after trimming ends only. No case folding. Empty text never matches. Default minimum length is 20 Unicode scalars."
        : "Exact common image bytes across channels. Extra images are allowed. Creates and relevant attachment edits participate. Work limits can reduce coverage." }),
      el("div", { class: "field" }, el("label", { text: "Distinct channels" }), imageChannels),
      image ? el("div", { class: "field" }, el("label", { text: "Common distinct images per message" }), imageCount) : null,
      textRule ? el("div", { class: "field" }, el("label", { text: "Minimum length (Unicode scalars after trim)" }), textLength) : null,
      el("div", { class: "field" }, el("label", { text: "Window (seconds)" }), imageWindow),
    ) : el("div", { class: "field" },
      el("label", { text: "Parameters (JSON)" }), paramsText,
    ),
    spam ? null : el("div", { class: "field" },
      el("label", { text: "Action escalation" }),
      el("p", { class: "field-help", text: "Strike count is the threshold for an action. Timeout minutes apply to timeout; delete-history days apply to ban." }), actionsRows,
      el("button", { class: "btn small", onclick: () => { rule.actions.push({ at: rule.actions.length + 1, action: "warn", duration_minutes: 0, delete_days: 0, reason: "" }); markDirty(); renderActions(); } }, "+ add action"),
    ),
    el("div", { class: "row form-row" },
      el("div", { class: "field" }, el("label", { text: "Whitelist roles (ids, comma)" }), whitelistText),
      el("div", { class: "field" }, el("label", { text: "Whitelist channels (ids)" }), channelsText),
      el("div", { class: "field" }, el("label", { text: "Whitelist users (ids)" }), usersText),
    ),
    el("div", { class: "modal-action" },
      el("div", { class: "field" }, el("label", { text: "Applies to" }), scopeSel),
      el("button", { class: "btn primary", text: "Save", onclick: async () => {
        try {
          if (spam) {
            const punish = !dryRun.checked;
            if (textRule) {
              rule.params = { mode: "exact_text", min_channels: Number(imageChannels.value), window_secs: Number(imageWindow.value), min_length: Number(textLength.value) };
            } else {
              rule.params = { mode: punish ? "exact_image" : "exact_image_test", min_channels: Number(imageChannels.value), min_attachments: Number(imageCount.value), window_secs: Number(imageWindow.value) };
            }
            rule.dry_run = !punish;
            rule.actions = punish
              ? [{ at: 1, action: "delete", reason: "spam match", duration_minutes: 0, delete_days: 0 }, { at: 1, action: "ban", reason: "spam match", duration_minutes: 0, delete_days: 0 }]
              : [{ at: 1, action: "log", reason: textRule ? "Repeated text test" : "Exact image spam test", duration_minutes: 0, delete_days: 0 }];
            if (!spamConfigured(r.rule_id, rule)) throw new Error("Channels must be 2–10 and window 1–600 seconds, all whole numbers. Images 1–10. Text length 1–2000.");
          } else {
            rule.params = JSON.parse(paramsText.value || "{}");
          }
        } catch (e) { toast("Invalid params JSON: " + e.message, "error"); return; }
        rule.whitelist_roles = splitIds(whitelistText.value);
        rule.whitelist_channels = splitIds(channelsText.value);
        rule.whitelist_users = splitIds(usersText.value);
        const scope = scopeSel.value;
        const saved = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", { scope, data: rule });
        const savedLocally = applyConfigPatch(saved);
        if (scope === "global") {
          try {
            const cleared = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" });
            applyConfigPatch(cleared);
          } catch (e) {
            // The global write already committed. A failed second request must
            // not leave the pre-save snapshot visible as current configuration.
            const refreshed = await loadConfig();
            if (refreshed) { closeOverlay(); renderTab(); }
            toast(`Global rule saved, but clearing this server's override could not be confirmed: ${e.message || String(e)}` +
              (refreshed ? "" : " Configuration refresh also failed. Use Retry to load the saved state."), "error");
            return;
          }
        }
        if (!savedLocally && !await loadConfig()) return;
        toast("Rule saved", "success");
        closeOverlay();
        renderTab();
      } }),
      el("button", { class: "btn danger", text: "Reset to global", onclick: async () => {
        await reloadAfter(
          () => api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" }),
          () => { toast("Rule reset", "success"); closeOverlay(); },
        );
      } }),
      el("button", { class: "btn outline", text: "Cancel", onclick: () => { if (canLeave()) closeOverlay(); } }),
    ),
  );
  modal.append(box);
  enhanceVendorWidgets(modal);
  return modal;
}

/** Discord snowflake from mention, channel URL, or raw digits. Never Number(). */
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
  const url = kind === "channel" ? value.match(/^https?:\/\/(?:canary\.|ptb\.)?discord(?:app)?\.com\/channels\/\d+\/(\d+)\/?$/i) : null;
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

// ---------------------------------------------------------------------------
// badwords
// ---------------------------------------------------------------------------

function tabBadwords() {
  const words = state.config.badwords;
  const pattern = el("input", { type: "text", placeholder: "regex pattern, e.g. (scam|pump and dump)", style: "flex:2" });
  const note = el("input", { type: "text", placeholder: "note (optional)", style: "flex:1" });
  const scope = scopeSelect("This server", "Global");
  const sample = el("input", { type: "text", placeholder: "sample text to test, e.g. 5c@m 🅿🆁🅾🅼🅾", style: "flex:2" });
  const result = el("pre", { class: "code" });

  return el("div", {},
    el("h2", { text: "Badwords (bypass-resistant)" }),
    el("p", { class: "muted", style: "margin-bottom:12px" },
      "Patterns are matched against both the raw message and a normalized form " +
      "(lowercase, unicode-decomposed, zero-width chars stripped, leetspeak mapped: 4/@→a, 3→e, 0→o, 1→i, 5→s, 7→t, $→s…). " +
      (isBotOwner() ? "Global patterns apply to every server." : "Global patterns (shown below) are enforced for all servers; only the bot owner can edit them.")),
    el("div", { class: "card" },
      el("div", { class: "row" }, pattern, note, scope,
        el("button", { class: "btn primary", text: "Add", onclick: async () => {
          if (!pattern.value.trim()) return toast("Enter a pattern", "error");
          await reloadAfter(
            async () => {
              const res = await api(`/api/guilds/${state.guildId}/badwords`, "POST", { scope: scope.value, pattern: pattern.value.trim(), note: note.value || null });
              pattern.value = ""; note.value = "";
              return res;
            },
            () => toast("Badword added", "success"),
          );
        } })),
    ),
    el("div", { class: "card" },
      el("h3", { text: "Live tester" }),
      el("div", { class: "row" },
        el("input", { type: "text", id: "test-pattern", placeholder: "regex", style: "flex:1" }), sample,
        el("button", { class: "btn", text: "Test", onclick: async () => {
          const p = $("#test-pattern").value;
          if (!p || !sample.value) return toast("Enter pattern and sample", "error");
          const res = await api(`/api/guilds/${state.guildId}/testregex`, "POST", { pattern: p, sample: sample.value });
          result.textContent = `raw match: ${res.raw_match}\nnormalized match: ${res.normalized_match}\nnormalized: ${res.normalized_sample}`;
        } })),
      result,
    ),
    el("div", { class: "card" },
      el("h3", { text: `Patterns (${words.length})` }),
      words.length === 0 ? el("p", { class: "muted", text: "No patterns yet." }) :
      el("table", {},
        el("thead", {}, el("tr", {}, el("th", { text: "Scope" }), el("th", { text: "Pattern" }), el("th", { text: "Note" }), el("th", { text: "Status" }), el("th", {}))),
        el("tbody", {}, ...words.map((w) => {
          const isGlobal = String(w.guild_id) === "0";
          const canDelete = !isGlobal || isBotOwner();
          const invalid = w.valid === false;
          return el("tr", {},
            el("td", { text: isGlobal ? "global" : "server" }),
            el("td", { class: "mono", text: w.pattern }),
            el("td", { class: "muted", text: w.note || "" }),
            el("td", {}, invalid
              ? el("span", { class: "tag off", title: w.error || "failed compile limits", text: "not enforced" })
              : el("span", { class: "tag on", text: "active" })),
            el("td", {}, canDelete ? el("button", { class: "btn small danger", text: "✕", onclick: async () => {
              await reloadAfter(
                () => api(`/api/guilds/${state.guildId}/badwords/${w.id}`, "DELETE"),
                () => toast("Removed", "success"),
              );
            } }) : el("span", { class: "muted", text: "owner only" })),
          );
        })),
      ),
    ),
  );
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

function tabCommands() {
  const cmds = state.config.commands;
  return el("div", {},
    el("div", { class: "row title-row" },
      el("h2", { text: "Commands" }),
      el("button", { class: "btn primary", text: "+ New custom command", onclick: () => editCommand(null) }),
    ),
    el("p", { class: "muted", style: "margin:6px 0 14px" },
      "Every command lives in config: rename, reword responses, change permissions or cooldowns. " +
      "Custom commands run multi-step action plans (reply, dm, log, roles, timeout, kick, ban…). " +
      "Enabled commands are synced to Discord as slash commands."),
    el("table", { class: "commands-table" },
      el("thead", {}, el("tr", {},
        el("th", { text: "Command" }), el("th", { text: "Category" }), el("th", { text: "Description" }),
        el("th", { text: "Permission" }), el("th", { text: "State" }), el("th", {})),
      ),
      el("tbody", {}, ...cmds.map((c) => {
        const d = c.effective;
        return el("tr", {},
          el("td", {}, el("div", { class: "command-identity" },
            el("strong", { class: "mono", text: d.name }),
            el("div", { class: "tag-group" },
              c.guild_override ? el("span", { class: "tag override", text: "server override" }) : null,
              c.scope === "global" ? el("span", { class: "tag scope", text: "global" }) : null))),
          el("td", { text: d.category }),
          el("td", { class: "muted command-description", text: d.description }),
          el("td", { text: d.permission }),
          el("td", {}, el("span", { class: `tag ${d.enabled ? "on" : "off"}`, text: d.enabled ? "enabled" : "disabled" })),
          el("td", {}, el("button", { class: "btn small", text: "Edit", onclick: () => editCommand(c) })),
        );
      })),
    ),
  );
}

function editCommand(c) {
  const isNew = c === null;
  const data = isNew ? {
    name: "", aliases: [], category: "Custom", description: "Custom command.", usage: "", enabled: true,
    permission: "everyone", roles: [], channels: [], cooldown_secs: 0,
    behavior: { kind: "plan", params: { steps: [{ type: "reply", template: "Hello {user}!" }] } },
    response: "", log_event: "",
  } : JSON.parse(JSON.stringify(c.effective));

  const modal = overlay();
  const nameIn = el("input", { type: "text", value: data.name, placeholder: "command name" });
  const descIn = el("input", { type: "text", value: data.description, placeholder: "description" });
  const usageIn = el("input", { type: "text", value: data.usage, placeholder: "usage" });
  const aliasesIn = el("input", { type: "text", value: (data.aliases || []).join(","), placeholder: "aliases, comma separated" });
  const respIn = el("textarea", { placeholder: "response template, e.g. Banned {target} | {reason}" });
  respIn.value = data.response || "";
  const permIn = el("select", {},
    ["everyone", "mod", "admin", "owner"].map((p) => el("option", { value: p, text: p, selected: data.permission === p })),
  );
  const cdIn = el("input", { type: "number", min: 0, value: data.cooldown_secs || 0 });
  const roleIn = el("input", { type: "text", value: (data.roles || []).join(","), placeholder: "role ids" });
  const chanIn = el("input", { type: "text", value: (data.channels || []).join(","), placeholder: "channel ids" });
  const logIn = el("input", { type: "text", value: data.log_event || "", placeholder: "mod_action" });
  const scopeSel = scopeSelect();
  const stepsBox = el("div", {});
  const behaviorArea = el("textarea", { class: "mono", style: "min-height:70px" });
  const isPlan = data.behavior && data.behavior.kind === "plan";
  if (isPlan) {
    behaviorArea.value = JSON.stringify(data.behavior.params || {}, null, 2);
  } else {
    behaviorArea.value = JSON.stringify(data.behavior || {}, null, 2);
  }

  // The structured step editor and advanced JSON editor represent the same
  // state. Keep structured edits reflected in the JSON before Save makes the
  // latter authoritative.
  const syncPlanJson = () => {
    if (isPlan) behaviorArea.value = JSON.stringify(data.behavior.params || {}, null, 2);
  };

  const renderSteps = () => {
    if (!isPlan) return;
    unmountVendorWidgets(stepsBox);
    stepsBox.innerHTML = "";
    const steps = data.behavior.params.steps;
    steps.forEach((s, i) => {
      const type = el("select", { class: "searchable" },
        ["reply", "dm", "log", "add_role", "remove_role", "timeout", "kick", "ban", "delete_command_message", "react", "wait"].map((t) =>
          el("option", { value: t, text: t, selected: s.type === t })),
      );
      const field = el("input", { type: "text", value: s.template ?? s.role_id ?? s.minutes ?? s.delete_days ?? s.emoji ?? s.secs ?? "", placeholder: typePlaceholder(s.type), style: "flex:1" });
      type.onchange = () => {
        const t = type.value;
        const n = { type: t };
        applyPlanStepInput(n, t, field.value);
        if (t === "log") n.event = "plan_executed";
        steps[i] = n;
        syncPlanJson();
        field.placeholder = typePlaceholder(t);
        field.value = field.placeholder === "…" ? "" : String(n.template ?? n.role_id ?? n.minutes ?? n.delete_days ?? n.emoji ?? n.secs ?? "");
      };
      field.onchange = () => {
        applyPlanStepInput(steps[i], steps[i].type, field.value);
        syncPlanJson();
      };
      stepsBox.append(el("div", { class: "step-card" },
        el("div", { class: "row control-row" }, type, field,
          el("button", { class: "btn small danger", text: "✕", onclick: () => { steps.splice(i, 1); markDirty(); syncPlanJson(); renderSteps(); } })),
      ));
    });
    enhanceVendorWidgets(stepsBox);
  };
  const typePlaceholder = (t) => ({
    reply: "message template, e.g. Hi {user}!", dm: "DM template", log: "log template",
    add_role: "role id", remove_role: "role id", timeout: "minutes", ban: "delete_days",
    delete_command_message: "…", react: "emoji", wait: "seconds",
  }[t] || "value");
  renderSteps();
  if (isPlan) {
    behaviorArea.onchange = () => {
      try {
        const params = JSON.parse(behaviorArea.value || "{}");
        if (!params || typeof params !== "object" || Array.isArray(params)) {
          throw new Error("plan parameters must be an object");
        }
        if (params.steps !== undefined && !Array.isArray(params.steps)) {
          throw new Error("plan steps must be an array");
        }
        params.steps = params.steps || [];
        data.behavior.params = params;
        renderSteps();
      } catch (e) {
        toast("Invalid behavior JSON: " + e.message, "error");
      }
    };
  }

  modal.append(el("div", { class: "modal-box" },
    el("h3", { text: isNew ? "New custom command" : `Edit command: ${c.name}` }),
    el("div", { class: "row form-row" },
      el("div", { class: "field" }, el("label", { text: "Name" }), nameIn),
      el("div", { class: "field" }, el("label", { text: "Aliases" }), aliasesIn),
      el("div", { class: "field" }, el("label", { text: "Permission" }), permIn),
    ),
    el("div", { class: "field" }, el("label", { text: "Description" }), descIn),
    el("div", { class: "field" }, el("label", { text: "Usage" }), usageIn),
    isPlan ? el("div", { class: "field" },
      el("label", { text: "Action plan steps" }), stepsBox,
      el("button", { class: "btn small", text: "+ add step", onclick: () => {
        data.behavior.params.steps.push({ type: "reply", template: "" });
        markDirty();
        syncPlanJson();
        renderSteps();
      } })) : null,
    el("div", { class: "field" },
      el("label", { text: "Behavior (advanced JSON)" }), behaviorArea,
    ),
    el("div", { class: "field" }, el("label", { text: "Response template (in-chat reply; {target} {reason} {response} …)" }), respIn),
    el("div", { class: "row form-row" },
      el("div", { class: "field" }, el("label", { text: "Cooldown (secs)" }), cdIn),
      el("div", { class: "field" }, el("label", { text: "Role restriction (ids)" }), roleIn),
      el("div", { class: "field" }, el("label", { text: "Channel restriction (ids)" }), chanIn),
      el("div", { class: "field" }, el("label", { text: "Log event" }), logIn),
    ),
    el("div", { class: "modal-action" },
      el("div", { class: "field" }, el("label", { text: "Applies to" }), scopeSel),
      el("label", { class: "checkbox-label" },
        el("input", { type: "checkbox", checked: data.enabled, onchange: (e) => (data.enabled = e.target.checked) }), "Enabled"),
      el("button", { class: "btn primary", text: "Save", onclick: async () => {
        try {
          const raw = JSON.parse(behaviorArea.value || "{}");
          if (isPlan) {
            data.behavior.params.steps = raw.steps || data.behavior.params.steps;
          } else {
            data.behavior = raw;
          }
        } catch (e) { toast("Invalid behavior JSON: " + e.message, "error"); return; }
        data.name = nameIn.value.trim().toLowerCase();
        if (!data.name) return toast("Name required", "error");
        data.aliases = aliasesIn.value.split(",").map((s) => s.trim()).filter(Boolean);
        data.description = descIn.value;
        data.usage = usageIn.value;
        data.response = respIn.value;
        data.permission = permIn.value;
        data.cooldown_secs = Number(cdIn.value) || 0;
        data.roles = splitIds(roleIn.value);
        data.channels = splitIds(chanIn.value);
        data.log_event = logIn.value;
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
          () => isNew
            ? api(`/api/guilds/${state.guildId}/commands`, "POST", { scope: scopeSel.value, data })
            : api(`/api/guilds/${state.guildId}/commands/${encodeURIComponent(c.name)}`, "PUT", { scope: scopeSel.value, data }),
          () => { toast("Command saved", "success"); closeOverlay(); },
        );
      } }),
      el("button", { class: "btn danger", text: "Delete", hidden: isNew, onclick: async () => {
        if (!confirm(`Delete command ${c.name}?`)) return;
        // Prefer clearing a guild override (reset to global). Only delete the
        // global row when there is no override and the caller is the bot owner.
        const delScope = c.guild_override
          ? "guild"
          : (c.scope === "global" && isBotOwner()) ? "global" : "guild";
        await reloadAfter(
          () => api(`/api/guilds/${state.guildId}/commands/${encodeURIComponent(c.name)}`, "DELETE", { scope: delScope }),
          () => { toast(c.guild_override ? "Override cleared" : "Deleted", "success"); closeOverlay(); },
        );
      } }),
      el("button", { class: "btn outline", text: "Cancel", onclick: () => { if (canLeave()) closeOverlay(); } }),
    ),
  ));
  enhanceVendorWidgets(modal);
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

function tabTemplates() {
  return el("div", {},
    el("h2", { text: "Event templates" }),
    el("p", { class: "muted", text: "Customize the messages sent to your log channels. Edit a template, review the sample, then save it for the selected scope." }),
    el("details", { class: "reference-panel" },
      el("summary", { text: "Template variables & message data" }),
      el("p", { class: "muted" },
      "Templates render into the configured log channels. Basic message reports supply {message_id} and {message_url}; updates also supply {after} and {attachments}. A single-message delete may supply {content} and author fields from a bounded recent in-process snippet when this process observed the create or content update; otherwise those values are unavailable. This is not persistent history or a Discord history lookup. Other event variables include {user} {mention} {user_id} {guild} {channel} {actor} {reason} {rule} {action} {content} {count} {account_age} {duration} {time}. Custom templates keep their wording. The readable-log upgrade updates exact previous defaults once; dates include whole seconds and UTC. Add fields to custom templates where needed.")),
    ...state.config.templates.map((t) => templateCard(t)),
  );
}

function templateCard(t) {
  const text = el("textarea", { class: "mono template-input", rows: 8, spellcheck: "false" });
  text.value = t.effective;
  const preview = el("pre", { class: "code" });
  preview.textContent = t.effective
    .replaceAll("{user}", "Alice")
    .replaceAll("{mention}", "@alice")
    .replaceAll("{channel}", "#general")
    .replaceAll("{actor}", "Bob (mod)")
    .replaceAll("{reason}", "spam")
    .replaceAll("{rule}", "mention_spam")
    .replaceAll("{action}", "ban")
    .replaceAll("{content}", "FREE NITRO http://scam.link")
    .replaceAll("{count}", "3")
    .replaceAll("{time}", new Date().toISOString());
  text.oninput = () => (preview.textContent = text.value);
  const scopeSel = scopeSelect("This server", "Global");
  return el("div", { class: "card template-card" },
    eventHeading(t.event),
    el("div", { class: "template-workspace" },
      field("Message template", text),
      el("div", { class: "template-preview" }, el("strong", { text: "Preview" }), preview)),
    el("div", { class: "editor-footer" },
      field("Save to", scopeSel),
      el("div", { class: "row" },
        el("button", { class: "btn primary small", text: "Save", onclick: async () => {
          await reloadAfter(
            () => api(`/api/guilds/${state.guildId}/templates/${t.event}`, "PUT", { scope: scopeSel.value, template: text.value }),
            () => toast("Template saved", "success"),
          );
        } }),
        el("button", { class: "btn small", text: "Reset to global", onclick: async () => {
          await reloadAfter(
            () => api(`/api/guilds/${state.guildId}/templates/${t.event}`, "DELETE", { scope: "guild" }),
            () => toast("Reset", "success"),
          );
        } }),
      ),
    ),
  );
}

// ---------------------------------------------------------------------------
// log channels
// ---------------------------------------------------------------------------

function tabLogChannels() {
  const guildId = state.guildId;
  return el("div", {},
    el("div", { class: "row title-row" },
      el("h2", { text: "Log channels" }),
      el("button", { class: "btn", text: "Refresh configuration", onclick: async () => {
        if (!canLeave()) return;
        if (await loadConfig()) renderTab();
      } })),
    el("p", { class: "muted", style: "margin-bottom:12px" },
      "Each event type routes to a channel in this server. Enter a channel ID, <#id> mention, or channel URL. Role and user ID fields on other tabs accept the same snowflake forms. Changes show as queued while the bot validates them, then reconcile automatically; Bot actions has the full receipt."),
    ...state.config.log_channels.map((lc) => {
      const input = el("input", { type: "text", value: (lc.effective && String(lc.effective) !== "0") ? String(lc.effective) : "", placeholder: "channel id" });
      const ignoredUsers = el("textarea", { placeholder: "One Discord user or bot ID per line", rows: 2 }, (lc.ignore?.users || []).join("\n"));
      const ignoredChannels = el("textarea", { placeholder: "One source channel ID per line", rows: 2 }, (lc.ignore?.channels || []).join("\n"));
      const scopeSel = scopeSelect("This server", "Global", false);
      return el("div", { class: "card log-channel-card" },
        el("div", { class: "row title-row" }, eventHeading(lc.event),
          lc.pending_action ? el("span", { class: "tag dry",
            text: ["pending", "claimed", "running"].includes(lc.pending_action.state) ? "queued" : lc.pending_action.state }) : null),
        el("div", { class: "routing-fields" },
        field("Destination channel", input), field("Applies to", scopeSel),
        el("div", { class: "row routing-actions" },
        el("button", { class: "btn small primary", text: "Set", savedInputs: [input], onclick: async () => {
          const v = parseSnowflake(input.value);
          if (!v) return toast("Enter a Discord channel ID, <#id> mention, or channel URL", "error");
          const job = await submitBotAction(`/api/guilds/${guildId}/logchannels/${lc.event}`, "PUT", { scope: scopeSel.value, channel_id: v });
          if (applyQueuedLogChannel(job)) {
            renderTab();
            if (["pending", "claimed", "running"].includes(job.state)) scheduleLogChannelReconcile(job);
            else if (job.state !== "succeeded") await reconcileQueuedLogChannel(job);
          }
        } }),
        el("button", { class: "btn small", text: "Clear", savedInputs: [input], onclick: async () => {
          const job = await submitBotAction(`/api/guilds/${guildId}/logchannels/${lc.event}`, "DELETE", { scope: scopeSel.value });
          if (applyQueuedLogChannel(job)) {
            renderTab();
            if (["pending", "claimed", "running"].includes(job.state)) scheduleLogChannelReconcile(job);
            else if (job.state !== "succeeded") await reconcileQueuedLogChannel(job);
          }
        } }),
        )),
        el("div", { class: "log-ignore-section" },
          el("h4", { text: "Event ignores" }),
          el("p", { class: "field-help", text: "Skip reports involving these users or source channels. Leave empty to report all." }),
          el("div", { class: "ignore-fields" },
            field("Ignore user or bot IDs", ignoredUsers),
            field("Ignore source channel IDs", ignoredChannels)),
          el("button", { class: "btn small", text: "Save ignores", savedInputs: [ignoredUsers, ignoredChannels], onclick: async () => {
            const users = ignoredUsers.value.split(/\r?\n|,/).map((id) => id.trim()).filter(Boolean).map((id) => parseLogFilterId(id, "user"));
            if (users.some((id) => !id)) return toast("Enter valid Discord user or bot IDs", "error");
            const channels = ignoredChannels.value.split(/\r?\n|,/).map((id) => id.trim()).filter(Boolean).map((id) => parseLogFilterId(id, "channel"));
            if (channels.some((id) => !id)) return toast("Enter a valid source channel ID, <#id> mention, or channel URL", "error");
            const saved = await api(`/api/guilds/${guildId}/logfilters/${lc.event}`, "PUT", { users, channels });
            lc.ignore = saved.ignore;
            toast("Ignore list saved", "success");
            renderTab();
          } }),
        ),
      );
    }),
  );
}

// ---------------------------------------------------------------------------
// events / bans
// ---------------------------------------------------------------------------

function tabEvents() {
  const typeSel = el("select", { class: "searchable" }, el("option", { value: "", text: "all types" }),
    ...state.config.event_types.map((t) => el("option", { value: t, text: t })));
  const userIn = el("input", { type: "text", placeholder: "user id" });
  const table = el("table", {}, el("thead", {}, el("tr", {}, ...["Time", "Event", "User ID", "Actor ID", "Details"].map((text) => el("th", { text })))), el("tbody", {}));
  const guildId = state.guildId;
  let request = 0;
  const tbody = () => table.querySelector("tbody");
  const run = async () => {
    const current = ++request;
    tbody().replaceChildren(el("tr", {}, el("td", { colspan: 5, text: "Loading…" })));
    try {
      const user = parseSnowflake(userIn.value);
      const rows = await apiRead(`/api/guilds/${guildId}/events?limit=100` +
        (typeSel.value ? `&event=${encodeURIComponent(typeSel.value)}` : "") +
        (user ? `&user=${encodeURIComponent(user)}` : ""));
      if (current !== request) return;
      tbody().innerHTML = "";
      if (!rows.length) tbody().append(el("tr", {}, el("td", { colspan: 5, class: "muted", text: "No matching records." })));
      rows.forEach((r) => {
        let data = "";
        try { data = JSON.stringify(JSON.parse(r.data || "{}"), null, 1); } catch (_) { data = r.data || ""; }
        tbody().append(el("tr", {},
          el("td", { class: "muted", text: new Date(r.created_at).toLocaleString() }),
          el("td", { text: r.event_type }),
          el("td", { class: "mono", text: r.user_id || "-" }),
          el("td", { class: "mono", text: r.actor_id || "-" }),
          el("td", { class: "mono", style: "font-size:11px", text: data }),
        ));
      });
    } catch (e) {
      if (current !== request) return;
      tbody().replaceChildren(el("tr", {}, el("td", { colspan: 5, class: "err", text: "Could not load records: " + (e.message || String(e)) })));
    }
  };
  typeSel.onchange = run;
  userIn.addEventListener("keydown", (e) => { if (e.key === "Enter") run(); });
  run().catch((e) => reportApiError(e));
  return el("div", {},
    el("h2", { text: "Audit log" }),
    el("div", { class: "row", style: "margin-bottom:12px" }, typeSel, userIn, el("button", { class: "btn", text: "Refresh", onclick: run })),
    table,
  );
}

function tabBans() {
  const table = el("table", {}, el("thead", {}, el("tr", {}, ...["Banned user", "Status", "Moderator", "Reason", "Time", "Actions"].map((text) => el("th", { text })))), el("tbody", {}));
  const guildId = state.guildId;
  let request = 0;
  const tbody = () => table.querySelector("tbody");
  const run = async () => {
    const current = ++request;
    tbody().replaceChildren(el("tr", {}, el("td", { colspan: 6, text: "Loading…" })));
    try {
      const rows = await apiRead(`/api/guilds/${guildId}/bans?limit=200`);
      if (current !== request) return;
      tbody().innerHTML = "";
      if (!rows.length) tbody().append(el("tr", {}, el("td", { colspan: 6, class: "muted", text: "No matching records." })));
      rows.forEach((r) => tbody().append(el("tr", {},
        el("td", {}, r.user_display_name ? `${r.user_display_name} (${r.user_name ? `${r.user_name}, ` : ""}${r.user_id})` : (r.user_name ? `${r.user_name} (${r.user_id})` : r.user_id)),
        el("td", { text: r.active ? "banned" : "unbanned" }),
        el("td", {}, r.moderator_id ? (r.moderator_name ? `${r.moderator_name} (${r.moderator_id})` : r.moderator_id) : "Unavailable"),
        el("td", { text: r.reason || "Reason unavailable" }),
        el("td", { class: "muted", text: new Date(r.created_at).toLocaleString() }),
        el("td", {}, r.active ? el("button", { class: "btn small", text: "Unban", onclick: async () => {
          if (!confirm(`Unban ${r.user_id}?`)) return;
          await submitBotAction(`/api/guilds/${guildId}/unban`, "POST", { user_id: String(r.user_id), reason: "unbanned via WebUI" });
        } }) : null),
      )));
    } catch (e) {
      if (current !== request) return;
      tbody().replaceChildren(el("tr", {}, el("td", { colspan: 6, class: "err", text: "Could not load records: " + (e.message || String(e)) })));
    }
  };
  run().catch((e) => reportApiError(e));
  return el("div", {},
    el("h2", { text: "Bans" }),
    el("p", { class: "muted", style: "margin-bottom:12px" }, "Recorded bans include bot actions and observed Discord bans. Names and moderators appear when known. Request an unban here or unban in Discord. Check Bot actions for queued results."),
    table,
  );
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

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

function tabActions() {
  const guildId = state.guildId;
  const body = el("tbody", {});
  const table = el("table", {}, el("thead", {}, el("tr", {},
    ...["Requested", "Action", "Requested by", "Status", "Result", "Controls"].map((text) => el("th", { text })))), body);
  let request = 0;
  const run = async () => {
    const current = ++request;
    body.replaceChildren(el("tr", {}, el("td", { colspan: 6, text: "Loading…" })));
    try {
      const jobs = await apiRead(`/api/guilds/${guildId}/actions`);
      if (current !== request) return;
      body.replaceChildren();
      if (!jobs.length) body.append(el("tr", {}, el("td", { colspan: 6, text: "No bot actions yet." })));
      for (const job of jobs) {
        const target = job.action.kind === "unban" ? `Unban ${job.action.user_id}` :
          job.action.kind === "member_sync" ? `Member sync: ${job.action.operation}` :
            `${job.action.event}: ${job.action.channel_id || "clear log channel"}`;
        body.append(el("tr", {},
          el("td", { text: new Date(job.created_at * 1000).toLocaleString() }),
          el("td", { text: target }), el("td", { text: job.actor_id }),
          el("td", { text: job.state === "uncertain" ? "Outcome unknown" : job.state }),
          el("td", { text: job.outcome || "—" }),
          el("td", {}, actionCanCancel(job) ? el("button", { class: "btn small", text: actionCancelLabel(job), onclick: async () => {
            if (actionCancelNeedsConfirm(job) && !confirm("Clear this stuck log-channel request? This does not change Discord. A new save can then run.")) return;
            await api(`/api/guilds/${guildId}/actions/${encodeURIComponent(job.id)}`, "DELETE");
            await run();
          } }) : null)));
      }
    } catch (e) {
      if (current !== request) return;
      body.replaceChildren(el("tr", {}, el("td", { colspan: 6, text: "Could not load actions: " + e.message })));
    }
  };
  run();
  return el("div", {}, el("h2", { text: "Bot actions" }),
    el("p", { class: "muted", text: "Latest 50 requests. Queued work requires the bot to be online. Unknown outcomes are not retried automatically; verify the result in Discord. Cancel unstarted work, including claimed requests the bot has not dispatched. Force-cancel a stuck log-channel request that is running or unknown so a new save can run; this does not undo Discord." }),
    el("button", { class: "btn standalone-action", text: "Refresh", onclick: run }), table);
}

function memberTime(value) {
  if (value === null || value === undefined || value === "") return "Unknown";
  const date = new Date(typeof value === "string" && !/^\d+$/.test(value) ? value : Number(value));
  return Number.isNaN(date.getTime()) ? "Unknown" : date.toLocaleString(undefined, { timeZoneName: "short" });
}

function tabMembers() {
  const guildId = state.guildId;
  const enrollmentId = state.config?.enrollment_id;
  const alive = () => state.guildId === guildId && state.config?.enrollment_id === enrollmentId && state.tab === "members";
  const search = el("input", { type: "search", placeholder: "User ID, mention, display name or username", "aria-label": "Search members" });
  const statusFilter = el("select", { "aria-label": "Member status" },
    ...[["current", "Current members"], ["banned", "Currently banned"], ["previously_banned", "Previously banned"], ["all", "All retained"]]
      .map(([value, label]) => el("option", { value, text: label })));
  const joinedFrom = el("input", { type: "date", "aria-label": "Joined from date, UTC" });
  const joinedTo = el("input", { type: "date", "aria-label": "Joined through date, UTC" });
  const sort = el("select", { "aria-label": "Join date order" },
    el("option", { value: "joined_newest", text: "Newest join first" }),
    el("option", { value: "joined_oldest", text: "Oldest join first" }));
  const syncCard = el("section", { class: "card", "aria-label": "Member import status" }, el("p", { text: "Loading import status…" }));
  const body = el("tbody", {});
  const detail = el("section", { class: "card", "aria-label": "Member history" });
  const table = el("table", {}, el("thead", {}, el("tr", {},
    ...["Member", "User ID", "Joined", "Status", "History"].map((label) => el("th", { text: label })))), body);
  const first = el("button", { class: "btn", text: "First page", disabled: true });
  const next = el("button", { class: "btn", text: "Next page", disabled: true });
  const refresh = el("button", { class: "btn", text: "Refresh" });
  let listRequest = 0, statusRequest = 0, detailRequest = 0;
  let listAbort = null, statusAbort = null, detailAbort = null;
  let timer = null, countdownTimer = null, debounce = null, disposed = false, after = "", nextAfter = null;
  let latestStatus = null, latestStatusRevision = -1, statusRetryNeeded = false;
  let latestStatusReceivedAt = 0;
  let pendingSyncAction = null, syncActionOutcome = null;
  const aliveHere = () => !disposed && alive();
  const clearTimer = () => { if (timer !== null) clearTimeout(timer); timer = null; };
  const clearCountdown = () => { if (countdownTimer !== null) clearTimeout(countdownTimer); countdownTimer = null; };
  const actionApplied = (sync, pending) => {
    if (!sync) return false;
    if (pending.state !== "succeeded") return false;
    if (Number(sync.progress_revision ?? 0) <= pending.revision) return false;
    if (pending.operation === "pause") return sync.status === "paused";
    if (pending.operation === "reconcile") return sync.run_kind === "reconcile" && sync.run_id !== pending.runId;
    return ["queued", "running", "waiting", "complete"].includes(sync.status);
  };
  const schedulePoll = () => {
    clearTimer();
    if (!aliveHere() || document.visibilityState !== "visible") return;
    const active = statusRetryNeeded || pendingSyncAction || (latestStatus && (
      ["queued", "running", "waiting"].includes(latestStatus.status) ||
      ["queued", "running", "waiting"].includes(latestStatus.audit_recovery_status)));
    if (active) timer = setTimeout(loadStatus, 10000);
  };
  const asDate = (value) => memberTime(value);
  const renderStatus = (sync) => {
    clearCountdown();
    const parts = [];
    const title = sync.run_kind === "reconcile" ? "Reconcile from Discord" : "Initial member import";
    parts.push(el("h3", { text: title }));
    const stage = [sync.phase || "pending", sync.status || "needs attention"].join(" · ");
    parts.push(el("p", { role: "status", text: `${stage}${sync.status_reason ? ` — ${sync.status_reason}` : ""}` }));
    if (!sync.run_id && sync.status === "queued") {
      parts.push(el("p", { class: "muted", text: "The bot will create the first import job when it is online. Import controls become available after that job exists." }));
    } else if (!sync.run_id && sync.status === "needs_attention") {
      parts.push(el("p", { class: "alert alert-warning", text: "Import metadata is unavailable. Controls are disabled until the saved job is restored or repaired." }));
    }
    const phaseCounts = { members: sync.member_processed, bans: sync.ban_processed, verify: sync.verify_processed, audit: sync.audit_processed };
    const processed = Number(sync.processed ?? sync.committed_count ?? sync.committed ?? 0);
    const phaseProcessed = phaseCounts[sync.phase] == null ? null : Number(phaseCounts[sync.phase]);
    const total = sync.total_estimate == null ? null : Number(sync.total_estimate);
    const percent = sync.status === "complete" ? 100 : sync.percent == null ? null : Math.min(99, Math.max(0, Number(sync.percent)));
    const totalKind = sync.total_is_estimated === false ? "counted" : "estimated";
    const bar = el("progress", { max: "100", value: sync.status === "complete" ? "100" : percent, "aria-label": "Import progress", "aria-valuetext": sync.status === "complete" ? "Complete" : percent === null ? `${phaseProcessed == null ? "Unknown number of" : phaseProcessed} records committed in current phase; total unknown` : `${Math.round(percent)} percent of ${totalKind} member stage` });
    if (sync.status !== "complete" && percent === null) bar.removeAttribute("value");
    parts.push(bar);
    const memberPhase = sync.phase === "members";
    let totalDescription = "current phase total unknown";
    if (total !== null && sync.total_is_estimated === false) totalDescription = `${total.toLocaleString()} members counted`;
    else if (total !== null && memberPhase) totalDescription = `approximately ${total.toLocaleString()} members estimated`;
    const progressText = sync.status === "complete" ? " · 100% complete" : percent === null ? "" : ` · ${Math.round(percent)}% of ${totalKind} member phase`;
    parts.push(el("p", { text: `${Number.isFinite(processed) ? processed.toLocaleString() : "0"} records committed in this run · ${totalDescription}${progressText}` }));
    const phases = ["members", "bans", "verify", "audit", "local_history"];
    const phaseLabels = { members: "Members", bans: "Current bans", verify: "Candidate checks", audit: "Available audit history", local_history: "Local case history" };
    const currentPhase = sync.phase === "complete" && sync.status !== "complete" && sync.local_history_complete === false
      ? phases.indexOf("local_history") : phases.indexOf(String(sync.phase || ""));
    parts.push(el("p", { class: "muted", text: phases.map((phase, index) => {
      const state = sync.status === "complete" || (currentPhase >= 0 && index < currentPhase) ? "done" :
        index === currentPhase ? sync.status : "pending";
      const count = phaseCounts[phase] == null ? " · committed count unknown" : ` · ${Number(phaseCounts[phase]).toLocaleString()} committed`;
      return `${phaseLabels[phase]}: ${state}${count}`;
    }).join(" · ") }));
    const serverReadAt = Number(sync.as_of ?? sync.as_of_ms);
    const statusNow = Number.isFinite(serverReadAt) && serverReadAt > 0
      ? serverReadAt + Math.max(0, Date.now() - latestStatusReceivedAt) : Date.now();
    const elapsed = sync.elapsed_seconds ?? (sync.started_at ? Math.max(0, (statusNow - Number(sync.started_at)) / 1000) : null);
    const workerUnconfirmed = ["worker_unconfirmed", "worker_offline"].includes(sync.estimate_reason);
    const forecastUnknown = workerUnconfirmed || ["paused", "needs_attention"].includes(sync.status) ||
      (sync.status === "waiting" && sync.estimate_reason !== "rate_limited") ||
      sync.estimate_reason === "discord_unavailable";
    const phaseFinish = Number(sync.phase_finish_at);
    const forecastAvailable = !forecastUnknown && sync.phase_remaining_seconds != null &&
      sync.phase_finish_at != null && Number.isFinite(phaseFinish) && phaseFinish > statusNow;
    const remaining = workerUnconfirmed ? "Unknown until worker activity is confirmed" :
      !forecastAvailable
        ? "Unknown" : `about ${Math.ceil((phaseFinish - statusNow) / 60000)} minutes for this phase`;
    const overallFinish = Number(sync.overall_finish_at);
    const finish = forecastAvailable ? asDate(sync.overall_finish_at != null && Number.isFinite(overallFinish) && overallFinish > statusNow
      ? sync.overall_finish_at : sync.phase_finish_at) : "Unknown";
    parts.push(el("p", { class: "muted", text: `Elapsed: ${elapsed == null ? "Unknown" : `${Math.round(Number(elapsed) / 60)} minutes`} · Remaining: ${remaining} · Expected finish: ${finish}` }));
    if (sync.phase === "members" && sync.phase_finish_at != null && sync.estimate_reason?.startsWith("preliminary member-stage estimate")) {
      parts.push(el("p", { class: "muted", text: "Preliminary member-stage estimate; later stages have unknown totals." }));
    }
    if (sync.estimate_reason === "worker_unconfirmed") parts.push(el("p", { class: "muted", text: "Worker activity is not recently confirmed. The bot may be between paced pages; timing will resume when activity is confirmed." }));
    if (sync.estimate_reason === "worker_offline") parts.push(el("p", { class: "muted", text: "The bot worker is offline. Timing will resume when it is online." }));
    if (sync.next_attempt_at) {
      const nextAttempt = Number(sync.next_attempt_at);
      const rateLimited = sync.status === "waiting" && (sync.status_reason === "rate_limited" || sync.estimate_reason === "rate_limited");
      let retryText = `Next attempt: ${asDate(sync.next_attempt_at)}`;
      if (rateLimited && Number.isFinite(nextAttempt)) {
        const seconds = Math.max(0, Math.ceil((nextAttempt - statusNow) / 1000));
        retryText = `${seconds ? `Rate limit: retry in ${Math.floor(seconds / 60)}m ${seconds % 60}s` : "Rate limit: retry due now"} · ${retryText}`;
        if (seconds && aliveHere() && document.visibilityState === "visible" && !statusRetryNeeded && latestStatus === sync) {
          countdownTimer = setTimeout(() => { countdownTimer = null; if (aliveHere() && latestStatus === sync) renderStatus(sync); }, 1000);
        }
      }
      parts.push(el("p", { class: "muted", text: retryText }));
    }
    if (sync.initial_completed_at_ms || sync.initial_completed_at) parts.push(el("p", { text: `Initial import completed: ${asDate(sync.initial_completed_at_ms ?? sync.initial_completed_at)}` }));
    if (sync.last_success_at) {
      const counts = [["processed", "processed"], ["inserted", "inserted"], ["updated", "updated"], ["unchanged", "unchanged"]]
        .filter(([field]) => sync[`last_success_${field}`] != null)
        .map(([field, label]) => `${Number(sync[`last_success_${field}`]).toLocaleString()} ${label}`);
      parts.push(el("p", { class: "muted", text: `Last successful run: ${asDate(sync.last_success_at)}${counts.length ? ` · ${counts.join(" · ")}` : ""}` }));
    }
    if (sync.reconciliation_needed) parts.push(el("p", { class: "alert alert-warning", text: "Coverage may be stale after an outage or restore. A full current-member and current-ban check needs an administrator to select Reconcile from Discord." }));
    if (sync.audit_recovery_status && sync.audit_recovery_status !== "complete") {
      const phase = sync.audit_recovery_phase === "ban" ? "Ban actions" : sync.audit_recovery_phase === "unban" ? "Unban actions" : "Ban and unban actions";
      parts.push(el("p", { class: "muted", text: `Bounded audit recovery: ${phase} · ${sync.audit_recovery_status} · ${Number(sync.audit_recovery_processed || 0).toLocaleString()} entries checked${sync.audit_recovery_reason ? ` · ${sync.audit_recovery_reason}` : ""}` }));
      if (sync.audit_recovery_next_attempt_at) parts.push(el("p", { class: "muted", text: `Audit recovery next attempt: ${asDate(sync.audit_recovery_next_attempt_at)}` }));
      if (sync.audit_recovery_last_progress_at) parts.push(el("p", { class: "muted", text: `Audit recovery last progressed: ${asDate(sync.audit_recovery_last_progress_at)}` }));
    }
    if (Number(sync.unseen_candidate_count || 0) > 0) parts.push(el("p", { class: "muted", text: `${Number(sync.unseen_candidate_count).toLocaleString()} absence candidates still need bounded Discord checks.` }));
    if (sync.status !== "complete") parts.push(el("p", { class: "muted", text: "Results are partial while import is incomplete. Older join or ban dates may be unavailable." }));
    parts.push(el("p", { class: "muted", text: `Last committed progress: ${asDate(sync.last_progress_at)}` }));
    if (sync.as_of_ms || sync.as_of) parts.push(el("p", { class: "muted", text: `Status read at ${asDate(sync.as_of_ms ?? sync.as_of)}` }));
    if (pendingSyncAction) parts.push(el("p", { role: "status", text: `${pendingSyncAction.operation[0].toUpperCase()}${pendingSyncAction.operation.slice(1)} requested · bot action ${pendingSyncAction.state}. Waiting for the import status to reflect it.` }));
    if (syncActionOutcome) parts.push(el("p", { class: "err", role: "status", text: syncActionOutcome }));
    const controls = el("div", { class: "row" });
    if (!isPreview()) {
      for (const [operation, label] of [["pause", "Pause"], ["resume", "Resume"], ["retry", "Retry"], ["reconcile", "Reconcile from Discord"]]) {
        const disabled = !sync.run_id || !!pendingSyncAction || (operation === "pause" && !["queued", "running", "waiting"].includes(sync.status)) ||
          (operation === "resume" && sync.status !== "paused") ||
          (operation === "retry" && !["needs_attention", "waiting"].includes(sync.status)) ||
          (operation === "reconcile" && (sync.status !== "complete" || !sync.initial_completed_at));
        controls.append(el("button", { class: "btn small", text: label, disabled, onclick: async () => {
          if (!latestStatus?.run_id) return;
          if (operation === "reconcile" && !confirm("Request a full current-member and current-ban reconciliation from Discord? This can take a while.")) return;
          const beforeRevision = latestStatusRevision;
          const beforeRunId = latestStatus?.run_id;
          const job = await submitBotAction(`/api/guilds/${guildId}/member-sync/actions`, "POST", { operation });
          if (!aliveHere()) return;
          syncActionOutcome = null;
          if (["pending", "claimed", "running", "succeeded"].includes(job.state)) {
            pendingSyncAction = { id: job.id, operation, state: job.state, revision: beforeRevision, runId: beforeRunId };
            if (actionApplied(latestStatus, pendingSyncAction)) pendingSyncAction = null;
          } else {
            syncActionOutcome = `${label} action ${job.state}. Check Bot actions for details.`;
          }
          renderStatus(latestStatus);
          schedulePoll();
          loadStatus();
        } }));
      }
    }
    parts.push(controls);
    syncCard.replaceChildren(...parts);
  };
  const loadStatus = async () => {
    const request = ++statusRequest;
    statusAbort?.abort();
    statusAbort = new AbortController();
    try {
      const sync = await apiRead(`/api/guilds/${guildId}/member-sync`, statusAbort.signal);
      if (!aliveHere() || request !== statusRequest) return;
      const revision = Number(sync.progress_revision ?? 0);
      if (latestStatus && revision < latestStatusRevision && pendingSyncAction) {
        syncActionOutcome = "Import status changed after a restore. Check Bot actions before repeating the requested control.";
        pendingSyncAction = null;
      }
      statusRetryNeeded = false;
      if (pendingSyncAction && actionApplied(sync, pendingSyncAction)) pendingSyncAction = null;
      latestStatus = sync;
      latestStatusRevision = revision;
      latestStatusReceivedAt = Date.now();
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
            if (actionApplied(sync, pendingSyncAction)) pendingSyncAction = null;
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
      if (latestStatus) renderStatus(latestStatus);
      else syncCard.replaceChildren();
      syncCard.append(el("p", { class: "err", role: "status", text: latestStatus ? "Could not refresh import status. Showing the last saved status; retrying shortly while visible." : "Could not load import status. Retrying shortly while visible." }));
      schedulePoll();
    }
  };
  const loadMembers = async () => {
    const request = ++listRequest;
    listAbort?.abort();
    listAbort = new AbortController();
    body.replaceChildren(el("tr", {}, el("td", { colspan: 5, text: "Loading members…" })));
    try {
      const q = new URLSearchParams({ limit: "50", status: statusFilter.value, sort: sort.value });
      const rawSearch = String(search.value || "").trim();
      const mention = rawSearch.match(/^<@!?(\d+)>$/);
      if (rawSearch) q.set("q", mention ? mention[1] : rawSearch);
      if (joinedFrom.value) q.set("joined_from", joinedFrom.value);
      if (joinedTo.value) q.set("joined_to", joinedTo.value);
      if (after) q.set("after", after);
      const page = await apiRead(`/api/guilds/${guildId}/members?${q}`, listAbort.signal);
      if (!aliveHere() || request !== listRequest) return;
      nextAfter = page.next_cursor || null;
      first.disabled = !after;
      next.disabled = !nextAfter;
      const rows = (page.members || []).map((member) => el("tr", {},
        el("td", {}, el("strong", { text: member.display_name || member.username || member.user_id }),
          member.username ? el("small", { class: "muted", text: ` @${member.username}` }) : null),
        el("td", {}, el("code", { text: member.user_id }), " ", el("button", { class: "btn small", text: "Copy", "aria-label": `Copy user ID ${member.user_id}`, onclick: () => navigator.clipboard.writeText(member.user_id) })),
        el("td", { text: asDate(member.joined_at_ms) }),
        el("td", {}, member.is_banned ? "Currently banned" : member.is_current ? "Current" : member.ever_banned ? "Previously banned" : "Departed",
          member.is_banned && member.current_ban_reason ? el("small", { class: "muted", text: ` · Current reason: ${member.current_ban_reason}` }) : null),
        el("td", {}, el("button", { class: "btn small", text: "View history", onclick: () => openMember(member.user_id, member.display_name) }))));
      body.replaceChildren(...(rows.length ? rows : [el("tr", {}, el("td", { colspan: 5, class: "muted", text: latestStatus?.status === "complete" ? "No members match these filters." : "No saved members match yet. Import may still be incomplete." }))]));
    } catch (error) {
      if (!aliveHere() || request !== listRequest || error.message === "Failed to fetch") return;
      body.replaceChildren(el("tr", {}, el("td", { colspan: 5, class: "err", text: "Could not load members. Refresh to retry." })));
    }
  };
  const openMember = async (userId, displayName) => {
    const request = ++detailRequest;
    detailAbort?.abort();
    detailAbort = new AbortController();
    let cursor = null;
    detail.replaceChildren(el("p", { text: "Loading member history…" }));
    const appendPage = async () => {
      const q = new URLSearchParams({ limit: "50" });
      if (cursor) q.set("after", cursor);
      const page = await apiRead(`/api/guilds/${guildId}/members/${userId}/history?${q}`, detailAbort.signal);
      if (!aliveHere() || request !== detailRequest) return;
      if (!cursor) detail.replaceChildren(el("h3", { text: displayName ? `History for ${displayName} (${userId})` : `History for ${userId}` }));
      for (const event of page.entries || []) {
        const sourceLabel = ({
          discord_audit: "Discord audit action",
          discord_audit_unresolved: "Confirmed Discord audit action; local case association unresolved",
          application_audit_linked: "Bot action linked to Discord audit",
          gateway_observation_unlinked: "Gateway observation; action time unconfirmed",
          state_reconciliation: "Verified from current Discord state; transition time unknown",
          application: "Bot action",
        })[event.source_kind] || "";
        const eventLabel = event.source_kind === "gateway_observation_unlinked"
          ? `${event.kind || "Ban state"} observation`
          : event.source_kind === "discord_audit_unresolved"
            ? `${event.kind === "ban" ? "Ban" : event.kind === "unban" ? "Unban" : "Discord"} audit entry`
            : event.kind || "Observation";
        const visibleLabel = event.kind === "member_state_observation" ? "Membership absence" :
          event.kind === "membership_state_observation" ? "Membership state check" :
          event.kind === "ban_present_state_observation" ? "Ban present (time unknown)" :
          event.kind === "ban_absent_state_observation" || event.kind === "ban_state_observation" ? "Ban absent (time unknown)" : eventLabel;
        detail.append(el("p", {},
          el("strong", { text: `${visibleLabel} · ${asDate(event.occurred_at_ms)}` }),
          event.case_id ? el("button", { class: "btn small", text: `Case ${event.case_id}`, onclick: () => { pendingCaseId = event.case_id; navigate("cases"); } }) : null,
          el("span", { class: "muted", text: `${sourceLabel ? ` ${sourceLabel}` : ""}${event.source_kind === "discord_audit_unresolved" && event.observation_id ? ` · Audit entry ${event.observation_id}` : ""}${event.observed_at_ms && event.source_kind === "state_reconciliation" ? ` · Checked ${asDate(event.observed_at_ms)}` : ""}${event.outcome ? ` · ${event.outcome}` : ""}${event.moderator_id ? ` · moderator ${event.moderator_id}` : ""}${event.reason ? ` · ${event.reason}` : ""}` })));
      }
      cursor = page.next_cursor || null;
      if (cursor) detail.append(el("button", { class: "btn small", text: "More history", onclick: (event) => { event.currentTarget.remove(); appendPage().catch(reportApiError); } }));
      else if (!(page.entries || []).length) detail.append(el("p", { class: "muted", text: "No known events. Older history may be unavailable." }));
    };
    try { await appendPage(); }
    catch (error) { if (aliveHere() && request === detailRequest && error.message !== "Failed to fetch") detail.replaceChildren(el("p", { class: "err", text: "Could not load history." })); }
  };
  const invalidateResults = () => {
    ++listRequest;
    listAbort?.abort();
    ++detailRequest;
    detailAbort?.abort();
    after = "";
    nextAfter = null;
    first.disabled = true;
    next.disabled = true;
    body.replaceChildren(el("tr", {}, el("td", { colspan: 5, text: "Loading members…" })));
    detail.replaceChildren();
  };
  const queueFilteredList = () => {
    if (!aliveHere()) return;
    invalidateResults();
    clearTimeout(debounce);
    debounce = setTimeout(() => { debounce = null; if (aliveHere()) loadMembers(); }, 300);
  };
  const loadChangedFilters = () => {
    if (!aliveHere()) return;
    clearTimeout(debounce);
    debounce = null;
    invalidateResults();
    loadMembers();
  };
  search.addEventListener("input", queueFilteredList);
  search.addEventListener("change", loadChangedFilters);
  for (const control of [joinedFrom, joinedTo]) {
    control.addEventListener("input", queueFilteredList);
    control.addEventListener("change", loadChangedFilters);
  }
  for (const control of [statusFilter, sort]) {
    control.addEventListener("input", queueFilteredList);
    control.addEventListener("change", loadChangedFilters);
  }
  first.addEventListener("click", () => { after = ""; loadMembers(); });
  next.addEventListener("click", () => { if (nextAfter) { after = nextAfter; loadMembers(); } });
  refresh.addEventListener("click", () => { loadStatus(); loadMembers(); });
  const visibility = () => { if (document.visibilityState === "visible" && aliveHere()) loadStatus(); else { clearTimer(); clearCountdown(); } };
  document.addEventListener("visibilitychange", visibility);
  disposeMembersView = () => { disposed = true; clearTimer(); clearCountdown(); clearTimeout(debounce); listAbort?.abort(); statusAbort?.abort(); detailAbort?.abort(); document.removeEventListener("visibilitychange", visibility); };
  loadStatus();
  loadMembers();
  return el("div", {}, el("h2", { text: "Members" }),
    el("p", { class: "muted", text: "Search saved member identities and known join dates. Import and historical coverage are shown below." }),
    syncCard,
    el("section", { class: "card" },
      el("div", { class: "row" }, search, statusFilter, joinedFrom, joinedTo, sort),
      el("div", { class: "row" }, refresh, first, next), table), detail);
}

function tabCases() {
  const guildId = state.guildId;
  const body = el("tbody", {});
  const detail = el("div", { class: "card", style: "margin-top:16px" });
  const table = el("table", {}, el("thead", {}, el("tr", {},
    ...["Case", "Action", "Outcome", "Subject", "When"].map((text) => el("th", { text })))), body);
  let request = 0;
  let detailRequest = 0;
  let after = "";
  let removed = false;
  let nextAfter = "";
  const pageSize = 50;
  const next = el("button", { class: "btn", text: "Next page", disabled: true, onclick: () => {
    if (!nextAfter) return;
    after = nextAfter;
    run();
  } });
  const first = el("button", { class: "btn", text: "First page", onclick: () => { after = ""; run(); } });
  const recovery = el("button", { class: "btn", text: "Removed records / retry cleanup", style: "display:none", onclick: () => {
    removed = !removed;
    after = "";
    recovery.textContent = removed ? "Back to cases" : "Removed records / retry cleanup";
    run();
  } });
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
      if (currentDetail !== detailRequest || state.guildId !== guildId) return;
      const c = payload.case || {};
      let noteIntent = null;
      let notePending = false;
      const note = el("textarea", { rows: "3", placeholder: "Add a note. This does not change Discord." });
      detail.replaceChildren(
        el("h3", { text: "Case " + id }),
        el("p", { text: `${c.action || "unknown"} · ${c.outcome || "unknown"} · subject ${c.subject_id || ""}` }),
        el("p", { class: "muted", text: c.reason || "No reason recorded." }),
        el("p", { class: "muted", text: "Removing a case deletes retained private records. It does not reverse a Discord ban, kick, timeout, or warning." }),
        payload.can_delete ? el("button", { class: "btn small standalone-action", text: "Remove case record", onclick: async () => {
          if (!confirm("Remove this case record? This does not undo the Discord action.")) return;
          const result = await api(`/api/guilds/${guildId}/cases/${id}`, "DELETE");
          toast(result?.cleanup_pending
            ? "Case record removed. Cleanup is pending. Use Removed records / retry cleanup."
            : "Case record removed", "success");
          renderTab();
        } }) : null,
        el("h3", { text: "History" }),
        ...(history.history || []).map((entry) => el("p", { class: "muted", text: JSON.stringify(entry.entry.body) })),
        el("div", { class: "field" }, el("label", { text: "Note" }), note,
          el("button", { class: "btn small", type: "button", text: "Add note", onclick: async () => {
            if (notePending) return;
            const text = note.value.trim();
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
            } finally { notePending = false; }
          } })),
        el("h3", { text: "Private evidence" }),
        ...(Array.isArray(c.evidence?.text) ? c.evidence.text : []).map((item) => el("div", {},
          el("p", { class: "muted", text: `Message ${item.message_id} · channel ${item.channel_id} · author ${item.author_id}` }),
          el("pre", { text: item.text }))),
        ...(evidence.evidence || []).map((item) => el("p", {},
          el("button", { class: "btn small", type: "button", text: `Download ${item.id}`, onclick: async () => {
            await downloadPrivate(`/api/guilds/${guildId}/cases/${id}/evidence/${item.id}`, item.id);
          } }))),
      );
    } catch (e) {
      if (currentDetail !== detailRequest || state.guildId !== guildId) return;
      detail.replaceChildren(el("p", { class: "err", text: e.message || String(e) }));
    }
  };
  const run = async () => {
    const current = ++request;
    ++detailRequest;
    detail.replaceChildren();
    next.disabled = true;
    body.replaceChildren(el("tr", {}, el("td", { colspan: 5, text: "Loading…" })));
    try {
      const query = new URLSearchParams({ limit: String(pageSize), removed: String(removed) });
      if (after) query.set("after", after);
      const payload = await apiRead(`/api/guilds/${guildId}/cases?${query}`);
      if (current !== request || state.guildId !== guildId) return;
      recovery.style.display = payload.can_delete ? "" : "none";
      const ids = payload.cases || [];
      nextAfter = ids.length === pageSize ? ids[ids.length - 1] : "";
      next.disabled = !nextAfter;
      first.disabled = !after;
      if (!ids.length) {
        body.replaceChildren(el("tr", {}, el("td", { colspan: 5, class: "muted", text: removed ? "No more removed records." : "No selected cases on this page." })));
        return;
      }
      const rows = [];
      for (const id of ids) {
        if (removed) {
          rows.push(el("tr", {}, el("td", { text: id }), el("td", { colspan: 4 },
            el("button", { class: "btn small", text: "Retry cleanup", onclick: async () => {
                const result = await api(`/api/guilds/${guildId}/cases/${id}`, "DELETE");
                toast(result.cleanup_pending ? "Cleanup is pending. Retry when storage is available." : "Live record cleanup complete. This does not delete backup copies.", "success");
            } }))));
          continue;
        }
        const payload = await apiRead(`/api/guilds/${guildId}/cases/${id}`);
        if (current !== request || state.guildId !== guildId) return;
        const c = payload.case || {};
        rows.push(el("tr", {},
          el("td", {}, el("button", { class: "btn small", text: id.slice(0, 8), onclick: () => openCase(id) })),
          el("td", { text: c.action || "" }),
          el("td", { text: c.outcome || "" }),
          el("td", { text: String(c.subject_id || "") }),
          el("td", { class: "muted", text: c.occurred_at_ms ? new Date(Number(c.occurred_at_ms)).toLocaleString() : "" }),
        ));
      }
      body.replaceChildren(...rows);
    } catch (e) {
      if (current !== request || state.guildId !== guildId) return;
      body.replaceChildren(el("tr", {}, el("td", { colspan: 5, class: "err", text: "Could not load cases: " + (e.message || String(e)) })));
    }
  };
  run();
  if (pendingCaseId) {
    const id = pendingCaseId;
    pendingCaseId = null;
    openCase(id);
  }
  return el("div", {},
    el("h2", { text: "Cases" }),
    el("p", { class: "muted", text: "Selected moderation facts for this bot membership. Ordinary messages are not archived. Removing a record does not reverse Discord." }),
    el("p", { class: "muted", text: "Use removed records to retry cleanup. Their private content stays deleted, and retrying does not undo Discord actions." }),
    el("div", { class: "row", style: "margin-bottom:12px" },
      el("button", { class: "btn", text: "Refresh", onclick: run }), first, next, recovery), table, detail);
}

function tabSettings() {
  const cfg = state.config;
  const ov = cfg.settings_override || {};
  const prefix = el("input", { type: "text", value: cfg.settings.prefix });
  const botName = el("input", { type: "text", value: cfg.settings.bot_name });

  let sourceEnrollment = null;
  const srcSel = el("select", { class: "searchable", onchange: async () => {
    sourceEnrollment = null;
    const guildId = srcSel.value;
    if (!guildId) return;
    const source = await apiRead(`/api/guilds/${guildId}/config`);
    if (srcSel.value === guildId && source.enrollment_id) {
      sourceEnrollment = { guildId, id: source.enrollment_id };
    }
  } },
    el("option", { value: "", text: "Select a source server" }),
    ...state.guilds.filter((g) => g.id !== state.guildId).map((g) => el("option", { value: g.id, text: g.name })),
  );
  const sectionSel = el("select", {},
    ["all", "rules", "badwords", "commands", "templates", "settings"]
      .map((s) => el("option", { value: s, text: s === "all" ? "everything" : s })),
  );
  const globalPrefix = el("input", { type: "text", value: cfg.global_settings.prefix });

  /** Clear one override field via API null (inherit parent). */
  async function inheritField(field) {
    await reloadAfter(
      () => api(`/api/guilds/${state.guildId}/settings`, "PUT", { [field]: null }),
      () => toast(`Using global ${field.replaceAll("_", " ")}`, "success"),
    );
  }

  function settingsField(labelText, input, overrideKey, hint, allowInheritance = true) {
    const hasOv = ov[overrideKey] != null;
    return el("div", { class: "field" },
      el("label", { text: labelText }),
      input,
      hint ? el("p", { class: "muted", style: "margin:4px 0 0;font-size:12px", text: hint }) : null,
      hasOv && allowInheritance
        ? el("button", {
            class: "btn small",
            type: "button",
            text: "Use global",
            savedInputs: [input],
            style: "margin-top:6px",
            onclick: () => inheritField(overrideKey),
          })
        : null,
    );
  }

  const cards = [
    el("div", { class: "card" },
      el("h3", { text: "This server" }),
      cfg.nickname_reconcile?.state === "terminal"
        ? el("p", { class: "error", text: `Discord nickname needs operator action: ${cfg.nickname_reconcile.last_error || cfg.nickname_reconcile.last_error_code || "permanent Discord rejection"}` })
        : cfg.nickname_reconcile?.state === "dormant"
          ? el("p", { class: "muted", text: "Discord nickname work is paused. After confirming the bot is in this server, use Re-sync Discord nickname to submit a fresh request. Work from an earlier membership is never replayed." })
          : null,
      el("div", { class: "row form-row" },
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
      el("div", { class: "row" },
        el("button", { class: "btn primary", text: "Save", onclick: async () => {
          // Tri-state body: omit unchanged, null clears override, value sets.
          // Only send fields the operator actually changed — always sending
          // bot_name re-applied a stale input when global nick changed elsewhere.
          // bot_name empty is special (AGENTS): persist "" to clear Discord nick — not inherit.
          const body = {};
          const rawPrefix = String(prefix.value ?? "").trim();
          if (rawPrefix === "") {
            if (ov.prefix != null) body.prefix = null;
            else {
              toast("Prefix cannot be empty (Use global to inherit)", "error");
              return;
            }
          } else if (rawPrefix !== cfg.settings.prefix) {
            body.prefix = rawPrefix;
          }
          if (botName.value !== cfg.settings.bot_name) {
            body.bot_name = botName.value;
          }
          if (body.prefix === undefined && body.bot_name === undefined) {
            toast("No settings changes", "success");
            return;
          }
          await reloadAfter(
            () => api(`/api/guilds/${state.guildId}/settings`, "PUT", body),
            (res) => toast(res.nickname_queued ? "Settings saved — Discord nickname queued" : "Settings saved", "success"),
          );
        } }),
        el("button", { class: "btn", text: "Re-sync Discord nickname", savedInputs: [], onclick: async () => {
          await reloadAfter(
            () => api(`/api/guilds/${state.guildId}/settings`, "PUT", { sync_nickname: true }),
            (res) => toast(res.nickname_queued ? "Discord nickname reconciliation queued" : "No nickname work queued", "success"),
          );
        } })),
    ),
  ];

  const roleInput = el("input", { type: "text", placeholder: "role id, <@&id> mention, comma-separated" });
  const roleRevision = { value: "" };
  let roleWriteGeneration = 0;
  const initialRoleGeneration = roleWriteGeneration;
  apiRead(`/api/guilds/${state.guildId}/case-delete-roles`).then((roles) => {
    if (initialRoleGeneration !== roleWriteGeneration) return;
    const loadedValue = (roles.roles || []).join(", ");
    if (roleInput._initialValue === undefined || roleInput.value === roleInput._initialValue) {
      roleInput.value = loadedValue;
    }
    roleInput._initialValue = loadedValue;
    roleRevision.value = roles.revision || "";
  }).catch((e) => { if (!isUnauthenticated(e)) reportApiError(e); });
  cards.push(el("div", { class: "card" },
    el("h3", { text: "Case removal" }),
    el("p", { class: "muted", text: "Server owners and Administrators can remove case records. Stored role IDs are remembered for a later member-role check; Discord OAuth currently cannot apply them. Removal never undoes a Discord action." }),
    el("div", { class: "field" }, el("label", { text: "Delegated case-removal role IDs" }), roleInput),
    el("div", { class: "card-actions" },
      el("button", { class: "btn", text: "Save case-removal roles", onclick: async () => {
      const saved = await api(`/api/guilds/${state.guildId}/case-delete-roles`, "PUT", { roles: splitIds(roleInput.value), revision: roleRevision.value });
      roleWriteGeneration++;
      roleRevision.value = saved.revision;
      roleInput._initialValue = roleInput.value;
      dirty = currentDraftInputs().length > 0;
      updateWorkspaceStatus();
      toast("Case-removal roles saved", "success");
    } }),
    ),
  ));

  if (isBotOwner()) {
    cards.push(el("div", { class: "card" },
      el("h3", { text: "Global defaults (applies to all servers)" }),
      el("p", { class: "muted", style: "margin-bottom:8px" },
        "Global settings apply to every server until that server overrides a field. Rules/templates/commands work the same way — choose 'Global' in any editor, or set the global prefix here."),
      el("div", { class: "row form-actions" },
        el("div", { class: "field" }, el("label", { text: "Global prefix" }), globalPrefix),
        el("button", { class: "btn", text: "Save global prefix", onclick: async () => {
          const raw = String(globalPrefix.value ?? "").trim();
          if (raw === "") {
            toast("Global prefix cannot be empty", "error");
            return;
          }
          await reloadAfter(
            () => api(`/api/guilds/0/settings`, "PUT", { prefix: raw }),
            () => toast("Global prefix saved", "success"),
          );
        } }),
      ),
    ));
  }

  cards.push(el("div", { class: "card" },
      el("h3", { text: "Copy configuration from another server" }),
      el("p", { class: "muted", style: "margin-bottom:8px" },
        "Replace rules, badwords, commands, templates, settings, or everything with another server's configuration. Log channels are server-specific and are not copied. Source must be a server you manage."),
      el("div", { class: "row" },
        srcSel, sectionSel,
        el("button", { class: "btn primary", text: "Copy", onclick: async () => {
          if (!sourceEnrollment || sourceEnrollment.guildId !== srcSel.value) return toast("Select a source server to load its current configuration", "error");
          if (!confirm(`Copy ${sectionSel.value} from ${srcSel.selectedOptions[0].textContent}?`)) return;
          await reloadAfter(
            () => api(`/api/guilds/${state.guildId}/copy`, "POST", {
              from_guild_id: String(srcSel.value), from_enrollment_id: sourceEnrollment.id, sections: [sectionSel.value],
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
              toast(msg, (res.skipped_badwords > 0 || res.skipped_logchannels) ? "info" : "success");
            },
          );
        } }),
      ),
    ));

  return el("div", {},
    el("h2", { text: "Settings" }),
    ...cards,
  );
}

// ---------------------------------------------------------------------------
// overlay + boot
// ---------------------------------------------------------------------------

function overlay() {
  const modal = el("div", { class: "modal-overlay", role: "dialog", "aria-modal": "true", "aria-label": "Configuration editor", tabindex: "-1", onclick: (e) => { if (e.target === modal && canLeave()) closeOverlay(); } });
  modal._returnFocus = document.activeElement;
  modal.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && canLeave()) closeOverlay();
    if (event.key === "Tab") {
      const controls = [...modal.querySelectorAll("button:not([disabled]):not([hidden]), input:not([disabled]):not(.search-select-native), select:not([disabled]):not(.search-select-native), textarea:not([disabled])")];
      const first = controls[0], last = controls.at(-1);
      if (event.shiftKey && (document.activeElement === first || document.activeElement === modal)) { event.preventDefault(); last?.focus(); }
      else if (!event.shiftKey && (document.activeElement === last || document.activeElement === modal)) { event.preventDefault(); first?.focus(); }
    }
  });
  $("#content").append(modal);
  modal.focus();
  return modal;
}
function closeOverlay() {
  const m = $(".modal-overlay");
  if (m) { unmountVendorWidgets(m); m.remove(); m._returnFocus?.focus(); dirty = false; updateWorkspaceStatus(); }
}

function initTheme() {
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  let preference = "system";
  try { preference = window.localStorage.getItem("jmodguard.theme") || "system"; } catch (_) { /* Keep an in-memory preference when storage is blocked. */ }
  if (!["light", "dark", "system"].includes(preference)) preference = "system";
  const apply = () => {
    const theme = preference === "system" ? (media.matches ? "dark" : "light") : preference;
    document.documentElement.dataset.theme = theme;
    document.querySelector('meta[name="theme-color"]').content = theme === "dark" ? "#101e33" : "#e0eeff";
    document.querySelectorAll(".theme-select").forEach((select) => { select.value = preference; });
  };
  document.querySelectorAll(".theme-select").forEach((select) => select.addEventListener("change", () => {
    preference = select.value;
    try { window.localStorage.setItem("jmodguard.theme", preference); } catch (_) { /* Appearance still changes for this session. */ }
    apply();
  }));
  media.addEventListener("change", apply);
  apply();
}

function bootUi() {
  capturePreviewToken();
  applyVendorCss();
  initTheme();
  bindTabButtons();
  $("#content").addEventListener("input", (event) => {
    if (event.target.matches("input, textarea, select") && !event.target.classList.contains("search-select-input") && !["events", "members", "bans"].includes(state.tab)) markDirty();
  });
  window.addEventListener("beforeunload", (event) => {
    if (dirty || busy) { event.preventDefault(); event.returnValue = ""; }
  });
  const loginLink = $("#login-link");
  if (loginLink) loginLink.href = authUrl("/login");
  const previewReturnLink = $("#preview-return-link");
  if (previewReturnLink) previewReturnLink.addEventListener("click", returnFromPreview);
  const logoutBtn = $("#logout-btn");
  if (logoutBtn) logoutBtn.addEventListener("click", async () => {
    if (!canLeave()) return;
    dirty = false;
    if (isPreview()) {
      try { await api("/api/preview", "DELETE"); } catch (_) {}
      clearPreviewToken();
      window.location = "/";
      return;
    }
    clearPreviewToken();
    try {
      const res = await fetch(authUrl("/logout"), { method: "POST", credentials: "include" });
      const data = await res.json();
      window.location = data.return_to || "/";
    } catch (_) {
      // The session delete may have succeeded even if navigation failed.
      window.location = "/";
    }
  });
  init().catch((e) => { toast("Init failed: " + e.message, "error"); });
}

if (typeof document !== "undefined") applyVendorCss();
if (typeof document !== "undefined" && document.getElementById?.("app")) {
  bootUi();
}
