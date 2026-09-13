/* jModGuard WebUI — vanilla JS single-page app */
"use strict";

const $ = (sel) => document.querySelector(sel);
const el = (tag, attrs, ...children) => {
  const n = document.createElement(tag);
  if (attrs) Object.entries(attrs).forEach(([k, v]) => {
    if (k === "class") n.className = v;
    else if (k === "text") n.textContent = v;
    else if (k === "savedInputs") n._savedInputs = v;
    else if (k.startsWith("on")) n.addEventListener(k.slice(2), (event) => handleEvent(n, v, event));
    else if (k === "checked" || k === "selected" || k === "hidden" || k === "disabled") n[k] = Boolean(v);
    else if (v !== null && v !== undefined) n.setAttribute(k, v);
  });
  children.flat(Infinity).forEach((c) => {
    if (c !== null && c !== undefined) n.append(c.nodeType ? c : document.createTextNode(c));
  });
  return n;
};

let state = { me: null, guilds: [], guildId: null, config: null, tab: "dashboard", loading: false, error: null };
let configRequest = 0;
let busy = false;
let dirty = false;
let pendingDrafts = null;
const tabs = ["dashboard", "rules", "badwords", "commands", "templates", "logchannels", "events", "bans", "actions", "cases", "settings"];

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
function captureDrafts(node) {
  const form = node.closest?.(".card, .modal-box");
  if (!form) return [];
  return [...$("#content").querySelectorAll("input, select, textarea")]
    .filter((input) => !(node._savedInputs ? node._savedInputs.includes(input) : form.contains(input)) &&
      input._initialValue !== undefined && input.value !== input._initialValue)
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
  } catch (e) { toast(e.message || String(e), "error"); }
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
  return err.status === 401 || msg.includes("not authenticated") || msg.includes("401");
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

function pageEnrollment(path, method = "GET") {
  // Bind edits and scoped reads to the membership this page loaded. Never
  // fetch a new token or retry against a replacement installation.
  const edit = path.match(/^\/api\/guilds\/(\d+)\/(settings|rules|badwords|commands|templates|copy|logchannels|unban|actions|events|bans|stats|cases|case-delete-roles)(?:\/|\?|$)/);
  if (edit && (method !== "GET" || ["rules", "badwords", "commands", "templates", "logchannels", "actions", "events", "bans", "stats", "cases", "case-delete-roles"].includes(edit[2])) && edit[1] !== "0" && edit[1] === state.guildId && state.config?.enrollment_id) {
    return state.config.enrollment_id;
  }
  return null;
}

async function api(path, method = "GET", body = null, signal) {
  // credentials: include so cross-origin CDN SPA (JMODGUARD_API_BASE) sends the session cookie.
  const opts = { method, headers: {}, credentials: "include", cache: "no-store" };
  const enrollment = pageEnrollment(path, method);
  if (enrollment) opts.headers["x-jmodguard-enrollment"] = enrollment;
  const controller = signal ? null : new AbortController();
  const timeout = controller ? setTimeout(() => controller.abort(), 30000) : null;
  opts.signal = signal || controller.signal;
  try {
    if (body !== null) { opts.headers["content-type"] = "application/json"; opts.body = JSON.stringify(body); }
    const res = await fetch(apiUrl(path), opts);
    const text = await res.text();
    let data = null;
    try { data = text ? JSON.parse(text) : null; } catch (_) { data = text; }
    if (!res.ok) {
      const msg = (data && data.error) ? data.error : `HTTP ${res.status}`;
      const error = apiError(msg, res.status, parseRetryAfterMs(res.headers.get("retry-after")));
      error.code = data && data.code;
      throw error;
    }
    return data;
  } catch (e) {
    if (e && (e.name === "AbortError" || e.name === "TimeoutError")) {
      throw apiError(signal ? "Failed to fetch" : "Request timed out. Refresh to verify saved state before trying again.");
    }
    throw e;
  } finally { if (timeout !== null) clearTimeout(timeout); }
}

async function downloadPrivate(path, filename) {
  const opts = { method: "GET", headers: {}, credentials: "include", cache: "no-store" };
  const enrollment = pageEnrollment(path, "GET");
  if (enrollment) opts.headers["x-jmodguard-enrollment"] = enrollment;
  const res = await fetch(apiUrl(path), opts);
  if (!res.ok) {
    let message = `HTTP ${res.status}`;
    try {
      const data = JSON.parse(await res.text());
      if (data && data.error) message = data.error;
    } catch (_) {}
    throw apiError(message, res.status);
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
      const me = await api("/api/me", "GET", null, controller.signal);
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
      if (!canLeave()) { sel.value = state.guildId || ""; return; }
      pendingDrafts = null;
      state.guildId = sel.value;
      state.config = null;
      dirty = false;
      persistWorkspace();
      if (!await loadConfig()) return;
      renderTab();
    });
    if (!await loadConfig()) return;
    renderTab();
  } catch (e) {
    if (isUnauthenticated(e)) {
      showView("login-view");
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
    const config = await api(`/api/guilds/${guildId}/config`);
    if (request !== configRequest || guildId !== state.guildId) return;
    if (state.config && state.config.enrollment_id !== config.enrollment_id) {
      pendingDrafts = null;
      dirty = false;
      toast("Bot membership changed. Configuration reloaded; review it before making changes.", "info");
    }
    state.config = config;
    return true;
  } catch (e) {
    if (request !== configRequest || guildId !== state.guildId) return;
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

// ---------------------------------------------------------------------------
// tabs
// ---------------------------------------------------------------------------

function renderTab() {
  const map = {
    dashboard: tabDashboard, rules: tabRules, badwords: tabBadwords, commands: tabCommands,
    templates: tabTemplates, logchannels: tabLogChannels,
    events: tabEvents, bans: tabBans, actions: tabActions, cases: tabCases, settings: tabSettings,
  };
  document.querySelectorAll("#tabs button").forEach((b) => {
    b.classList.toggle("active", b.dataset.tab === state.tab);
    if (b.dataset.tab === state.tab) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  });
  const fn = map[state.tab] || tabDashboard;
  const content = $("#content");
  content.innerHTML = "";
  dirty = false;
  const guild = selectedGuild();
  const name = $("#workspace-name");
  if (name) name.textContent = guild?.name || "Your servers";
  if (!state.guildId) {
    content.append(el("div", { class: "empty-state" }, el("h2", { text: "No servers to manage yet" }), el("p", { text: "Sign in with an account that has Manage Server permission in a server using jModGuard." })));
  } else if (!state.config) {
    content.append(el("div", { class: "empty-state", role: "status" },
      el("h2", { text: state.loading ? "Loading your workspace…" : "Configuration unavailable" }),
      el("p", { text: state.error || "Fetching saved settings for this server." }),
      !state.loading ? el("button", { class: "btn primary", text: "Retry", onclick: async () => { if (!await loadConfig()) return; renderTab(); } }) : null));
  } else {
    content.append(fn());
    content.querySelectorAll("table").forEach((table) => {
      const wrap = el("div", { class: "table-scroll", tabindex: "0", role: "region", "aria-label": "Scrollable data table" });
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
  updateWorkspaceStatus();
}

function bindTabButtons() {
  document.querySelectorAll("#tabs button").forEach((b) => b.addEventListener("click", () => {
    navigate(b.dataset.tab);
  }));
}

function tabDashboard() {
  const g = selectedGuild();
  const cfg = state.config;
  const rulesOn = cfg.rules.filter((r) => r.effective.enabled).length;
  const rulesDry = cfg.rules.filter((r) => r.effective.dry_run).length;
  const commandsOn = cfg.commands.filter((c) => c.effective.enabled).length;
  const chans = cfg.log_channels.filter((l) => l.effective && String(l.effective) !== "0").length;
  const moderation = [['events', 'Moderation records'], ['warns', 'Warns'], ['bans', 'Bans'], ['cases', 'Cases']]
    .map(([key, label]) => ({ key, value: el('div', { class: 'value', text: 'Loading…' }), label }));
  const guildId = state.guildId;
  api(`/api/guilds/${guildId}/stats`).then(counts => {
    moderation.forEach(({ key, value }) => { value.textContent = counts[key] == null ? 'Unavailable' : String(counts[key]); });
  }).catch(() => {
    moderation.forEach(({ value }) => { value.textContent = 'Unavailable'; });
  });
  return el("div", {},
    el("section", { class: "hero" },
      el("span", { class: "eyebrow", text: "COMMUNITY AT A GLANCE" }),
      el("h2", { text: g ? `${g.name}, under your care.` : "Your moderation workspace" }),
      el("p", { text: "A clear view of your server’s protection. Fine-tune the rules, review activity, and make room for a better community." }),
      el("div", { class: "row" },
        el("button", { class: "btn primary", text: "Review protection →", onclick: () => navigate("rules") }),
        el("button", { class: "btn", text: "View audit log", onclick: () => navigate("events") }))),
    el("div", { class: "stat-grid" },
      stat("Prefix", cfg.settings.prefix),
      stat("Bot name", cfg.settings.bot_name || "default"),
      stat("Rules enabled", `${rulesOn} / ${cfg.rules.length}`),
      stat("Rules in dry-run", rulesDry),
      stat("Commands enabled", `${commandsOn} / ${cfg.commands.length}`),
      stat("Log channels", chans),
      stat("Badword patterns", cfg.badwords.length),
      stat("Routine chat collection", "Off"),
      ...moderation.map(({ value, label }) => el('div', { class: 'stat' }, value, el('div', { class: 'label', text: label }))),
    ),
    el('p', { class: 'muted', text: 'Moderation records exclude routine history and older plan summaries whose action type was not recorded. Those summaries remain in the Audit Log.' }),
    el("div", { class: "card", style: "margin-top:16px" },
      el("h3", { text: "Getting started" }),
      el("p", { class: "muted", text: isBotOwner()
        ? `Use the tabs to configure automod rules, badwords, commands, and log channels. Use each editor’s Save action to apply changes; the Discord bot usually picks them up within a few seconds (config poll). As bot owner you can also edit Global defaults that apply to every server.`
        : `Use the tabs to configure automod rules, badwords, commands, and log channels for this server. Use each editor’s Save action to apply changes; the Discord bot usually picks them up within a few seconds (config poll). Global defaults are owner-only.` }),
    ),
  );
}

function stat(label, value) {
  return el("div", { class: "stat" }, el("div", { class: "value", text: value }), el("div", { class: "label", text: label }));
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
      "Exact image and repeated-text rules have a separate matcher with test or delete-and-ban mode. " +
      "Dry-run logs what would have happened without acting — use it while tuning."),
    ...rules.map((r) => ruleCard(r)),
  );
}

function ruleCard(r) {
  const rule = r.effective;
  const spam = spamRule(r.rule_id);
  const supported = !spam || spamConfigured(r.rule_id, rule);
  const badge = !rule.enabled || !supported ? "off" : rule.dry_run ? "dry" : "on";
  const label = !rule.enabled ? "disabled" : !supported ? "inactive — convert this rule" : spam && rule.dry_run ? "log-only test" : spam ? "delete and ban" : rule.dry_run ? "dry-run" : "enabled";
  return el("div", { class: "card" },
    el("div", { class: "row", style: "justify-content:space-between" },
      el("div", {},
        el("strong", { text: `${r.name} ` }),
        el("span", { class: `tag ${badge}`, text: label }),
        r.guild_override ? el("span", { class: "tag", text: "server override" }) : null,
        r.global ? el("span", { class: "tag", text: "global" }) : null,
      ),
      el("button", { class: "btn small", onclick: () => editRule(r) }, "Edit"),
    ),
    el("p", { class: "muted", style: "margin-top:6px", text: r.description }),
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
  const enforce = el("input", { type: "checkbox", checked: spamConfigured(r.rule_id, rule) && rule.dry_run === false });
  const modal = overlay();
  const paramsText = el("textarea", { class: "mono" });
  paramsText.value = JSON.stringify(rule.params ?? {}, null, 2);
  const actionsRows = el("div", {});
  const whitelistText = el("input", { type: "text", value: (rule.whitelist_roles || []).join(",") });
  const channelsText = el("input", { type: "text", value: (rule.whitelist_channels || []).join(",") });
  const usersText = el("input", { type: "text", value: (rule.whitelist_users || []).join(",") });
  const scopeSel = scopeSelect();

  const renderActions = () => {
    actionsRows.innerHTML = "";
    rule.actions.forEach((a, i) => {
      const at = el("input", { type: "number", min: 1, value: a.at, style: "width:70px" });
      const action = el("select", {},
        ["log", "delete", "warn", "timeout", "kick", "ban"].map((x) => el("option", { value: x, text: x, selected: a.action === x })),
      );
      const dur = el("input", { type: "number", min: 0, value: a.duration_minutes ?? 0, style: "width:80px", title: "minutes (timeout)" });
      const dd = el("input", { type: "number", min: 0, max: 7, value: a.delete_days ?? 0, style: "width:70px", title: "days of messages deleted (ban)" });
      const reason = el("input", { type: "text", value: a.reason || "", style: "flex:1", placeholder: "reason template" });
      actionsRows.append(el("div", { class: "row", style: "margin-bottom:6px" },
        el("span", { class: "muted", text: "at " }), at, action, dur, dd, reason,
        el("button", { class: "btn small danger", onclick: () => { rule.actions.splice(i, 1); markDirty(); renderActions(); } }, "✕"),
      ));
      at.onchange = () => (rule.actions[i].at = Math.max(1, parseInt(at.value) || 1));
      action.onchange = () => { rule.actions[i].action = action.value; renderActions(); };
      dur.onchange = () => (rule.actions[i].duration_minutes = parseInt(dur.value) || 0);
      dd.onchange = () => { rule.actions[i].delete_days = Math.min(7, Math.max(0, parseInt(dd.value) || 0)); };
      reason.onchange = () => (rule.actions[i].reason = reason.value);
    });
  };
  if (!spam) renderActions();

  const box = el("div", { class: "modal-box" },
    el("h3", { text: `Edit rule: ${r.name}` }),
    el("div", { class: "field" },
      el("label", { text: "Enabled" }),
      el("input", { type: "checkbox", checked: rule.enabled, onchange: (e) => (rule.enabled = e.target.checked) }),
      spam ? el("label", {}, " Delete matched messages and ban", enforce) :
        el("label", {}, " Dry-run (log only)", el("input", { type: "checkbox", checked: rule.dry_run, onchange: (e) => (rule.dry_run = e.target.checked) })),
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
      el("label", { text: "Action escalation" }), actionsRows,
      el("button", { class: "btn small", onclick: () => { rule.actions.push({ at: rule.actions.length + 1, action: "warn", duration_minutes: 0, delete_days: 0, reason: "" }); markDirty(); renderActions(); } }, "+ add action"),
    ),
    el("div", { class: "row" },
      el("div", { class: "field" }, el("label", { text: "Whitelist roles (ids, comma)" }), whitelistText),
      el("div", { class: "field" }, el("label", { text: "Whitelist channels (ids)" }), channelsText),
      el("div", { class: "field" }, el("label", { text: "Whitelist users (ids)" }), usersText),
    ),
    el("div", { class: "row", style: "margin-top:10px" },
      el("div", { class: "field" }, el("label", { text: "Applies to" }), scopeSel),
      el("button", { class: "btn primary", text: "Save", onclick: async () => {
        try {
          if (spam) {
            const punish = enforce.checked;
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
        await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", { scope, data: rule });
        if (scope === "global") {
          try {
            await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" });
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
        if (!await loadConfig()) return;
        toast("Rule saved", "success");
        closeOverlay();
        renderTab();
      } }),
      el("button", { class: "btn danger", text: "Reset to global", onclick: async () => {
        await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" });
        if (!await loadConfig()) return;
        toast("Rule reset", "success");
        closeOverlay();
        renderTab();
      } }),
      el("button", { class: "btn", text: "Cancel", onclick: () => { if (canLeave()) closeOverlay(); } }),
    ),
  );
  modal.append(box);
  return modal;
}

function splitIds(s) {
  // Keep Discord snowflakes as digit strings — never Number() (precision loss above 2^53−1).
  return String(s ?? "")
    .split(",")
    .map((x) => x.trim())
    .filter((x) => /^\d+$/.test(x));
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
          await api(`/api/guilds/${state.guildId}/badwords`, "POST", { scope: scope.value, pattern: pattern.value.trim(), note: note.value || null });
          pattern.value = ""; note.value = "";
          if (!await loadConfig()) return;
          toast("Badword added", "success");
          renderTab();
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
              await api(`/api/guilds/${state.guildId}/badwords/${w.id}`, "DELETE");
              if (!await loadConfig()) return;
              toast("Removed", "success");
              renderTab();
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
    el("div", { class: "row", style: "justify-content:space-between" },
      el("h2", { text: "Commands" }),
      el("button", { class: "btn good", text: "+ New custom command", onclick: () => editCommand(null) }),
    ),
    el("p", { class: "muted", style: "margin:6px 0 14px" },
      "Every command lives in config: rename, reword responses, change permissions or cooldowns. " +
      "Custom commands run multi-step action plans (reply, dm, log, roles, timeout, kick, ban…). " +
      "Enabled commands are synced to Discord as slash commands."),
    el("table", {},
      el("thead", {}, el("tr", {},
        el("th", { text: "Name" }), el("th", { text: "Category" }), el("th", { text: "Description" }),
        el("th", { text: "Permission" }), el("th", { text: "State" }), el("th", {})),
      ),
      el("tbody", {}, ...cmds.map((c) => {
        const d = c.effective;
        return el("tr", {},
          el("td", {}, el("strong", { text: d.name }),
            c.guild_override ? el("span", { class: "tag", text: "override" }) : null,
            c.scope === "global" ? el("span", { class: "tag", text: "global" }) : null),
          el("td", { text: d.category }),
          el("td", { class: "muted", text: d.description }),
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
    stepsBox.innerHTML = "";
    const steps = data.behavior.params.steps;
    steps.forEach((s, i) => {
      const type = el("select", {},
        ["reply", "dm", "log", "add_role", "remove_role", "timeout", "kick", "ban", "delete_command_message", "react", "wait"].map((t) =>
          el("option", { value: t, text: t, selected: s.type === t })),
      );
      const field = el("input", { type: "text", value: s.template ?? s.role_id ?? s.minutes ?? s.delete_days ?? s.emoji ?? s.secs ?? "", placeholder: typePlaceholder(s.type), style: "flex:1" });
      type.onchange = () => {
        const t = type.value;
        const n = { type: t };
        if (t === "reply" || t === "dm" || t === "log") n.template = field.value;
        if (t === "add_role" || t === "remove_role") n.role_id = String(field.value || "").trim();
        if (t === "timeout") n.minutes = Number(field.value) || 1;
        if (t === "ban") n.delete_days = Math.min(7, Math.max(0, Number(field.value) || 0));
        if (t === "react") n.emoji = field.value;
        if (t === "wait") n.secs = Math.max(1, Number(field.value) || 1);
        if (t === "log") n.event = "plan_executed";
        steps[i] = n;
        syncPlanJson();
        field.placeholder = typePlaceholder(t);
        field.value = field.placeholder === "…" ? "" : String(n.template ?? n.role_id ?? n.minutes ?? n.delete_days ?? n.emoji ?? n.secs ?? "");
      };
      field.onchange = () => {
        const t = steps[i].type;
        if (t === "reply" || t === "dm" || t === "log") steps[i].template = field.value;
        if (t === "add_role" || t === "remove_role") steps[i].role_id = String(field.value || "").trim();
        if (t === "timeout") steps[i].minutes = Number(field.value) || 1;
        if (t === "ban") steps[i].delete_days = Math.min(7, Math.max(0, Number(field.value) || 0));
        if (t === "react") steps[i].emoji = field.value;
        if (t === "wait") steps[i].secs = Math.max(1, Number(field.value) || 1);
        syncPlanJson();
      };
      stepsBox.append(el("div", { class: "step-card" },
        el("div", { class: "row" }, type, field,
          el("button", { class: "btn small danger", text: "✕", onclick: () => { steps.splice(i, 1); markDirty(); syncPlanJson(); renderSteps(); } })),
      ));
    });
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
    el("div", { class: "row" },
      el("div", { class: "field", style: "flex:1" }, el("label", { text: "Name" }), nameIn),
      el("div", { class: "field", style: "flex:1" }, el("label", { text: "Aliases" }), aliasesIn),
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
    el("div", { class: "row" },
      el("div", { class: "field" }, el("label", { text: "Cooldown (secs)" }), cdIn),
      el("div", { class: "field" }, el("label", { text: "Role restriction (ids)" }), roleIn),
      el("div", { class: "field" }, el("label", { text: "Channel restriction (ids)" }), chanIn),
      el("div", { class: "field" }, el("label", { text: "Log event" }), logIn),
    ),
    el("div", { class: "row", style: "margin-top:12px" },
      el("div", { class: "field" }, el("label", { text: "Applies to" }), scopeSel),
      el("label", { text: "Enabled" }),
      el("input", { type: "checkbox", checked: data.enabled, onchange: (e) => (data.enabled = e.target.checked) }),
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
            if ((s.type === "add_role" || s.type === "remove_role") && !String(s.role_id || "").trim()) {
              return toast("Role steps need a role id", "error");
            }
          }
        }
        if (isNew) {
          await api(`/api/guilds/${state.guildId}/commands`, "POST", { scope: scopeSel.value, data });
        } else {
          await api(`/api/guilds/${state.guildId}/commands/${encodeURIComponent(c.name)}`, "PUT", { scope: scopeSel.value, data });
        }
        if (!await loadConfig()) return;
        toast("Command saved", "success");
        closeOverlay();
        renderTab();
      } }),
      el("button", { class: "btn danger", text: "Delete", hidden: isNew, onclick: async () => {
        if (!confirm(`Delete command ${c.name}?`)) return;
        // Prefer clearing a guild override (reset to global). Only delete the
        // global row when there is no override and the caller is the bot owner.
        const delScope = c.guild_override
          ? "guild"
          : (c.scope === "global" && isBotOwner()) ? "global" : "guild";
        await api(`/api/guilds/${state.guildId}/commands/${encodeURIComponent(c.name)}`, "DELETE", { scope: delScope });
        if (!await loadConfig()) return;
        toast(c.guild_override ? "Override cleared" : "Deleted", "success");
        closeOverlay();
        renderTab();
      } }),
      el("button", { class: "btn", text: "Cancel", onclick: () => { if (canLeave()) closeOverlay(); } }),
    ),
  ));
}

// ---------------------------------------------------------------------------
// templates
// ---------------------------------------------------------------------------

function tabTemplates() {
  return el("div", {},
    el("h2", { text: "Event templates" }),
    el("p", { class: "muted", style: "margin-bottom:12px" },
      "Templates render into the configured log channels. Basic message reports supply {message_id} and {message_url}; updates also supply {after} and {attachments}. Earlier text and deleted content are not retained. Other event variables include {user} {mention} {user_id} {guild} {channel} {actor} {reason} {rule} {action} {content} {count} {account_age} {duration} {time}. Saved templates keep their wording after upgrades; add the new fields where needed."),
    ...state.config.templates.map((t) => templateCard(t)),
  );
}

function templateCard(t) {
  const text = el("textarea", { class: "mono" });
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
  return el("div", { class: "card" },
    el("div", { class: "row", style: "justify-content:space-between" },
      el("strong", { text: t.event }),
      el("div", { class: "row" },
        scopeSel,
        el("button", { class: "btn primary small", text: "Save", onclick: async () => {
          await api(`/api/guilds/${state.guildId}/templates/${t.event}`, "PUT", { scope: scopeSel.value, template: text.value });
          if (!await loadConfig()) return;
          toast("Template saved", "success");
          renderTab();
        } }),
        el("button", { class: "btn small", text: "Reset to global", onclick: async () => {
          await api(`/api/guilds/${state.guildId}/templates/${t.event}`, "DELETE", { scope: "guild" });
          if (!await loadConfig()) return;
          toast("Reset", "success");
          renderTab();
        } }),
      ),
    ),
    text,
    el("div", { class: "muted", style: "margin:6px 0" }, el("strong", { text: "Preview: " }), preview),
  );
}

// ---------------------------------------------------------------------------
// log channels
// ---------------------------------------------------------------------------

function tabLogChannels() {
  const guildId = state.guildId;
  return el("div", {},
    el("h2", { text: "Log channels" }),
    el("button", { class: "btn", text: "Refresh configuration", onclick: async () => {
      if (!canLeave()) return;
      if (await loadConfig()) renderTab();
    } }),
    el("p", { class: "muted", style: "margin-bottom:12px" },
      "Each event type routes to a channel in this server. Enter a channel ID or mention (<#id>). Changes are queued for bot validation; check Bot actions for results, then refresh this page."),
    ...state.config.log_channels.map((lc) => {
      const input = el("input", { type: "text", value: (lc.effective && String(lc.effective) !== "0") ? String(lc.effective) : "", placeholder: "channel id" });
      const scopeSel = scopeSelect("This server", "Global", false);
      return el("div", { class: "card row", style: "margin-bottom:8px" },
        el("label", { style: "width:180px", text: lc.event }),
        input, scopeSel,
        el("button", { class: "btn small primary", text: "Set", onclick: async () => {
          const v = input.value.trim().replace(/[<#>]/g, "");
          if (!v) return toast("Enter a channel id", "error");
          await submitBotAction(`/api/guilds/${guildId}/logchannels/${lc.event}`, "PUT", { scope: scopeSel.value, channel_id: v });
        } }),
        el("button", { class: "btn small", text: "Clear", onclick: async () => {
          await submitBotAction(`/api/guilds/${guildId}/logchannels/${lc.event}`, "DELETE", { scope: scopeSel.value });
        } }),
      );
    }),
  );
}

// ---------------------------------------------------------------------------
// events / bans
// ---------------------------------------------------------------------------

function tabEvents() {
  const typeSel = el("select", {}, el("option", { value: "", text: "all types" }),
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
      const user = userIn.value.trim();
      const rows = await api(`/api/guilds/${guildId}/events?limit=100` +
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
  run().catch((e) => toast(e.message, "error"));
  return el("div", {},
    el("h2", { text: "Audit log" }),
    el("div", { class: "row", style: "margin-bottom:12px" }, typeSel, userIn, el("button", { class: "btn", text: "Refresh", onclick: run })),
    table,
  );
}

function tabBans() {
  const table = el("table", {}, el("thead", {}, el("tr", {}, ...["User ID", "Status", "Reason", "Time", "Actions"].map((text) => el("th", { text })))), el("tbody", {}));
  const guildId = state.guildId;
  let request = 0;
  const tbody = () => table.querySelector("tbody");
  const run = async () => {
    const current = ++request;
    tbody().replaceChildren(el("tr", {}, el("td", { colspan: 5, text: "Loading…" })));
    try {
      const rows = await api(`/api/guilds/${guildId}/bans?limit=200`);
      if (current !== request) return;
      tbody().innerHTML = "";
      if (!rows.length) tbody().append(el("tr", {}, el("td", { colspan: 5, class: "muted", text: "No matching records." })));
      rows.forEach((r) => tbody().append(el("tr", {},
        el("td", { class: "mono", text: r.user_id }),
        el("td", { text: r.active ? "banned" : "unbanned" }),
        el("td", { text: r.reason || "" }),
        el("td", { class: "muted", text: new Date(r.created_at).toLocaleString() }),
        el("td", {}, r.active ? el("button", { class: "btn small", text: "Unban", onclick: async () => {
          if (!confirm(`Unban ${r.user_id}?`)) return;
          try {
            await submitBotAction(`/api/guilds/${guildId}/unban`, "POST", { user_id: String(r.user_id), reason: "unbanned via WebUI" });
          } catch (e) {
            toast(e.message || String(e), "error");
          }
        } }) : null),
      )));
    } catch (e) {
      if (current !== request) return;
      tbody().replaceChildren(el("tr", {}, el("td", { colspan: 5, class: "err", text: "Could not load records: " + (e.message || String(e)) })));
    }
  };
  run().catch((e) => toast(e.message, "error"));
  return el("div", {},
    el("h2", { text: "Bans" }),
    el("p", { class: "muted", style: "margin-bottom:12px" }, "Bans performed by the bot (automod + commands). Request an unban here or unban in Discord. Check Bot actions for queued results."),
    table,
  );
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

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
      const jobs = await api(`/api/guilds/${guildId}/actions`);
      if (current !== request) return;
      body.replaceChildren();
      if (!jobs.length) body.append(el("tr", {}, el("td", { colspan: 6, text: "No bot actions yet." })));
      for (const job of jobs) {
        const target = job.action.kind === "unban" ? `Unban ${job.action.user_id}` :
          `${job.action.event}: ${job.action.channel_id || "clear log channel"}`;
        body.append(el("tr", {},
          el("td", { text: new Date(job.created_at * 1000).toLocaleString() }),
          el("td", { text: target }), el("td", { text: job.actor_id }),
          el("td", { text: job.state === "uncertain" ? "Outcome unknown" : job.state }),
          el("td", { text: job.outcome || "—" }),
          el("td", {}, job.state === "pending" ? el("button", { class: "btn small", text: "Cancel", onclick: async () => {
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
    el("p", { class: "muted", text: "Latest 50 requests. Queued work requires the bot to be online. Unknown outcomes are not retried automatically; verify the result in Discord. You can cancel work that has not yet been claimed." }),
    el("button", { class: "btn", text: "Refresh", onclick: run }), table);
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
      const payload = await api(`/api/guilds/${guildId}/cases/${id}`);
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
      const evidence = await api(`/api/guilds/${guildId}/cases/${id}/evidence`);
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
        payload.can_delete ? el("button", { class: "btn small", text: "Remove case record", onclick: async () => {
          if (!confirm("Remove this case record? This does not undo the Discord action.")) return;
          try {
            const result = await api(`/api/guilds/${guildId}/cases/${id}`, "DELETE");
            toast(result?.cleanup_pending
              ? "Case record removed. Cleanup is pending. Use Removed records / retry cleanup."
              : "Case record removed", "success");
            renderTab();
          } catch (e) { toast(e.message || String(e), "error"); }
        } }) : null,
        el("h3", { text: "History" }),
        ...(history.history || []).map((entry) => el("p", { class: "muted", text: JSON.stringify(entry.entry.body) })),
        el("div", { class: "field" }, el("label", { text: "Note" }), note,
          el("button", { class: "btn small", type: "button", text: "Add note", onclick: async () => {
            if (notePending) return;
            try {
              const text = note.value.trim();
              if (!noteIntent || noteIntent.text !== text) {
                noteIntent = { entry_id: crypto.randomUUID().replaceAll("-", ""), text };
              }
              notePending = true;
              await api(`/api/guilds/${guildId}/cases/${id}/history`, "POST", noteIntent);
              noteIntent = null;
              toast("Note added", "success");
              await openCase(id);
            } catch (e) { toast(e.message || String(e), "error"); }
            finally { notePending = false; }
          } })),
        el("h3", { text: "Private evidence" }),
        ...(Array.isArray(c.evidence?.text) ? c.evidence.text : []).map((item) => el("div", {},
          el("p", { class: "muted", text: `Message ${item.message_id} · channel ${item.channel_id} · author ${item.author_id}` }),
          el("pre", { text: item.text }))),
        ...(evidence.evidence || []).map((item) => el("p", {},
          el("button", { class: "btn small", type: "button", text: `Download ${item.id}`, onclick: async () => {
            try { await downloadPrivate(`/api/guilds/${guildId}/cases/${id}/evidence/${item.id}`, item.id); }
            catch (e) { toast(e.message || String(e), "error"); }
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
      const payload = await api(`/api/guilds/${guildId}/cases?${query}`);
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
              try {
                const result = await api(`/api/guilds/${guildId}/cases/${id}`, "DELETE");
                toast(result.cleanup_pending ? "Cleanup is pending. Retry when storage is available." : "Live record cleanup complete. This does not delete backup copies.", "success");
              } catch (e) { toast(e.message || String(e), "error"); }
            } }))));
          continue;
        }
        const payload = await api(`/api/guilds/${guildId}/cases/${id}`);
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
  return el("div", {},
    el("h2", { text: "Cases" }),
    el("p", { class: "muted", text: "Selected moderation facts for this bot membership. Ordinary messages are not archived. Removing a record does not reverse Discord." }),
    el("p", { class: "muted", text: "Use removed records to retry cleanup. Their private content stays deleted, and retrying does not undo Discord actions." }),
    el("button", { class: "btn", text: "Refresh", onclick: run }), first, next, recovery, table, detail);
}

function tabSettings() {
  const cfg = state.config;
  const ov = cfg.settings_override || {};
  const prefix = el("input", { type: "text", value: cfg.settings.prefix });
  const botName = el("input", { type: "text", value: cfg.settings.bot_name });

  let sourceEnrollment = null;
  const srcSel = el("select", { onchange: async () => {
    sourceEnrollment = null;
    const guildId = srcSel.value;
    if (!guildId) return;
    try {
      const source = await api(`/api/guilds/${guildId}/config`);
      if (srcSel.value === guildId && source.enrollment_id) {
        sourceEnrollment = { guildId, id: source.enrollment_id };
      }
    } catch (e) { toast("Could not load source server: " + e.message, "error"); }
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
    try {
      await api(`/api/guilds/${state.guildId}/settings`, "PUT", { [field]: null });
      if (!await loadConfig()) return;
      toast(`Using global ${field.replaceAll("_", " ")}`, "success");
      renderTab();
    } catch (e) {
      toast(e.message || String(e), "error");
    }
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
      el("div", { class: "row" },
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
        try {
          const res = await api(`/api/guilds/${state.guildId}/settings`, "PUT", body);
          if (!await loadConfig()) return;
          toast(res.nickname_queued ? "Settings saved — Discord nickname queued" : "Settings saved", "success");
          renderTab();
        } catch (e) {
          toast(e.message || String(e), "error");
        }
      } }),
      el("button", { class: "btn", text: "Re-sync Discord nickname", savedInputs: [], onclick: async () => {
        try {
          const res = await api(`/api/guilds/${state.guildId}/settings`, "PUT", {
            sync_nickname: true,
          });
          if (!await loadConfig()) return;
          toast(res.nickname_queued ? "Discord nickname reconciliation queued" : "No nickname work queued", "success");
          renderTab();
        } catch (e) {
          toast(e.message || String(e), "error");
        }
      } }),
    ),
  ];

  const roleInput = el("input", { type: "text", placeholder: "Role IDs, comma-separated" });
  const roleRevision = { value: "" };
  api(`/api/guilds/${state.guildId}/case-delete-roles`).then((roles) => {
    roleInput.value = (roles.roles || []).join(", ");
    roleRevision.value = roles.revision || "";
  }).catch(() => {});
  cards.push(el("div", { class: "card" },
    el("h3", { text: "Case removal" }),
    el("p", { class: "muted", text: "Server owners and Administrators can remove case records. Stored role IDs are remembered for a later member-role check; Discord OAuth currently cannot apply them. Removal never undoes a Discord action." }),
    el("div", { class: "field" }, el("label", { text: "Delegated case-removal role IDs" }), roleInput),
    el("button", { class: "btn", text: "Save case-removal roles", onclick: async () => {
      try {
        const roles = roleInput.value.split(",").map((s) => s.trim()).filter(Boolean);
        const saved = await api(`/api/guilds/${state.guildId}/case-delete-roles`, "PUT", { roles, revision: roleRevision.value });
        roleRevision.value = saved.revision;
        toast("Case-removal roles saved", "success");
      } catch (e) { toast(e.message || String(e), "error"); }
    } }),
  ));

  if (isBotOwner()) {
    cards.push(el("div", { class: "card" },
      el("h3", { text: "Global defaults (applies to all servers)" }),
      el("p", { class: "muted", style: "margin-bottom:8px" },
        "Global settings apply to every server until that server overrides a field. Rules/templates/commands work the same way — choose 'Global' in any editor, or set the global prefix here."),
      el("div", { class: "row" },
        el("div", { class: "field" }, el("label", { text: "Global prefix" }), globalPrefix),
        el("button", { class: "btn", text: "Save global prefix", onclick: async () => {
          const raw = String(globalPrefix.value ?? "").trim();
          if (raw === "") {
            toast("Global prefix cannot be empty", "error");
            return;
          }
          try {
            await api(`/api/guilds/0/settings`, "PUT", { prefix: raw });
            if (!await loadConfig()) return;
            toast("Global prefix saved", "success");
            renderTab();
          } catch (e) {
            toast(e.message || String(e), "error");
          }
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
          try {
            let res = await api(`/api/guilds/${state.guildId}/copy`, "POST", {
              from_guild_id: String(srcSel.value), from_enrollment_id: sourceEnrollment.id, sections: [sectionSel.value],
            });
            if (!await loadConfig()) return;
            let msg = `Copied: ${(res.copied || []).join(", ") || "(nothing)"}`;
            if (res.skipped_badwords > 0) {
              msg += ` (${res.skipped_badwords} invalid badword(s) skipped — not enforced)`;
            }
            if (res.skipped_logchannels) {
              msg += " (log channels skipped — channel IDs are server-specific; set them here)";
            }
            if (res.nickname_queued) msg += " (Discord nickname queued)";
            toast(msg, (res.skipped_badwords > 0 || res.skipped_logchannels) ? "info" : "success");
            renderTab();
          } catch (e) {
            toast(e.message || String(e), "error");
          }
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
      const controls = [...modal.querySelectorAll("button:not([disabled]):not([hidden]), input:not([disabled]), select:not([disabled]), textarea:not([disabled])")];
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
  if (m) { m.remove(); m._returnFocus?.focus(); dirty = false; updateWorkspaceStatus(); }
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
  initTheme();
  bindTabButtons();
  $("#content").addEventListener("input", (event) => {
    if (event.target.matches("input, textarea, select") && !["events", "bans"].includes(state.tab)) markDirty();
  });
  window.addEventListener("beforeunload", (event) => {
    if (dirty || busy) { event.preventDefault(); event.returnValue = ""; }
  });
  const loginLink = $("#login-link");
  if (loginLink) loginLink.href = authUrl("/login");
  const logoutBtn = $("#logout-btn");
  if (logoutBtn) logoutBtn.addEventListener("click", async () => {
    if (!canLeave()) return;
    dirty = false;
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

if (typeof document !== "undefined" && document.getElementById?.("app")) {
  bootUi();
}
