/* jModGuard WebUI — vanilla JS single-page app */
"use strict";

const $ = (sel) => document.querySelector(sel);
const el = (tag, attrs, ...children) => {
  const n = document.createElement(tag);
  if (attrs) Object.entries(attrs).forEach(([k, v]) => {
    if (k === "class") n.className = v;
    else if (k === "text") n.textContent = v;
    else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
    else if (k === "checked" || k === "selected") n[k] = Boolean(v);
    else if (v !== null && v !== undefined) n.setAttribute(k, v);
  });
  children.forEach((c) => {
    if (c !== null && c !== undefined) n.append(c.nodeType ? c : document.createTextNode(c));
  });
  return n;
};

let state = { me: null, guilds: [], guildId: null, config: null, tab: "dashboard" };

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

/** Pre-axum API bind returns 503 JSON `{error:"not ready"}` until the router is live. */
function isApiWarmup(err) {
  const msg = String(err && err.message);
  return err.status === 503 || msg === "not ready";
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

async function api(path, method = "GET", body = null) {
  // credentials: include so cross-origin CDN SPA (JMODGUARD_API_BASE) sends the session cookie.
  const opts = { method, headers: {}, credentials: "include" };
  if (body !== null) { opts.headers["content-type"] = "application/json"; opts.body = JSON.stringify(body); }
  const res = await fetch(apiUrl(path), opts);
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch (_) { data = text; }
  if (!res.ok) {
    const msg = (data && data.error) ? data.error : `HTTP ${res.status}`;
    throw apiError(msg, res.status, parseRetryAfterMs(res.headers.get("retry-after")));
  }
  return data;
}

/** CDN Storage can serve this page while the API is still opening the database. */
async function waitForApiMe() {
  const deadline = Date.now() + 6 * 60 * 1000;
  let delay = 500;
  let sawWarmup = false;
  while (true) {
    try {
      return await api("/api/me");
    } catch (e) {
      if (!isApiWarmup(e) || Date.now() >= deadline) throw e;
      if (!sawWarmup) {
        sawWarmup = true;
        showView("startup-view");
      }
      await sleep(Math.max(e.retryAfterMs || 0, delay));
      delay = Math.min(Math.floor(delay * 1.5), 2000);
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
    if (me.guilds.length === 0) {
      sel.append(el("option", { value: "", text: "No manageable servers — invite the bot" }));
      return;
    }
    if (me.guilds.length > 1) {
      sel.append(el("option", { value: "", text: "Select a server…" }));
    }
    sel.value = state.guildId || "";
    if (!state.guildId) { sel.value = me.guilds[0].id; }
    state.guildId = sel.value;
    sel.addEventListener("change", async () => {
      state.guildId = sel.value;
      if (state.guildId) { await loadConfig(); renderTab(); }
    });
    await loadConfig();
    renderTab();
  } catch (e) {
    if (isUnauthenticated(e)) {
      showView("login-view");
    } else if (isApiWarmup(e)) {
      showView("startup-view");
      toast("API is still starting. Reload this page in a minute.", "error");
    } else {
      toast("Failed to load: " + e.message, "error");
    }
  }
}

async function loadConfig() {
  state.config = await api(`/api/guilds/${state.guildId}/config`);
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
    templates: tabTemplates, logchannels: tabLogChannels, members: tabMembers,
    events: tabEvents, bans: tabBans, settings: tabSettings,
  };
  document.querySelectorAll("#tabs button").forEach((b) => b.classList.toggle("active", b.dataset.tab === state.tab));
  const fn = map[state.tab] || tabDashboard;
  const content = $("#content");
  content.innerHTML = "";
  content.append(fn());
}

document.querySelectorAll("#tabs button").forEach((b) => b.addEventListener("click", () => {
  state.tab = b.dataset.tab;
  renderTab();
}));

function tabDashboard() {
  const g = selectedGuild();
  const cfg = state.config;
  const rulesOn = cfg.rules.filter((r) => r.effective.enabled).length;
  const rulesDry = cfg.rules.filter((r) => r.effective.dry_run).length;
  const commandsOn = cfg.commands.filter((c) => c.effective.enabled).length;
  const chans = cfg.log_channels.filter((l) => l.effective && String(l.effective) !== "0").length;
  return el("div", {},
    el("h2", { text: `Dashboard — ${g ? g.name : ""}` }),
    el("div", { class: "stat-grid" },
      stat("Prefix", `\`${cfg.settings.prefix}\``),
      stat("Bot name", cfg.settings.bot_name || "default"),
      stat("Rules enabled", `${rulesOn} / ${cfg.rules.length}`),
      stat("Rules in dry-run", rulesDry),
      stat("Commands enabled", `${commandsOn} / ${cfg.commands.length}`),
      stat("Log channels", chans),
      stat("Retention", Number(cfg.settings.retention_days) === 0 ? "forever" : `${cfg.settings.retention_days}d`),
    ),
    el("div", { class: "card", style: "margin-top:16px" },
      el("h3", { text: "Getting started" }),
      el("p", { class: "muted", text: isBotOwner()
        ? `Use the tabs to configure automod rules, badwords, commands, and log channels. Changes are saved immediately; the Discord bot usually picks them up within a few seconds (config poll). As bot owner you can also edit Global defaults that apply to every server.`
        : `Use the tabs to configure automod rules, badwords, commands, and log channels for this server. Changes are saved immediately; the Discord bot usually picks them up within a few seconds (config poll). Global defaults are owner-only.` }),
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
      "Dry-run logs what would have happened without acting — use it while tuning."),
    ...rules.map((r) => ruleCard(r)),
  );
}

function ruleCard(r) {
  const rule = r.effective;
  const badge = !rule.enabled ? "off" : rule.dry_run ? "dry" : "on";
  const label = !rule.enabled ? "disabled" : rule.dry_run ? "dry-run" : "enabled";
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

function editRule(r) {
  const rule = JSON.parse(JSON.stringify(r.effective));
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
        el("button", { class: "btn small danger", onclick: () => { rule.actions.splice(i, 1); renderActions(); } }, "✕"),
      ));
      at.onchange = () => (rule.actions[i].at = Math.max(1, parseInt(at.value) || 1));
      action.onchange = () => { rule.actions[i].action = action.value; renderActions(); };
      dur.onchange = () => (rule.actions[i].duration_minutes = parseInt(dur.value) || 0);
      dd.onchange = () => { rule.actions[i].delete_days = Math.min(7, Math.max(0, parseInt(dd.value) || 0)); };
      reason.onchange = () => (rule.actions[i].reason = reason.value);
    });
  };
  renderActions();

  const box = el("div", { class: "modal-box" },
    el("h3", { text: `Edit rule: ${r.name}` }),
    el("div", { class: "field" },
      el("label", { text: "Enabled" }),
      el("input", { type: "checkbox", checked: rule.enabled, onchange: (e) => (rule.enabled = e.target.checked) }),
      el("label", { text: " Dry-run (log only)", style: "margin-left:14px" }),
      el("input", { type: "checkbox", checked: rule.dry_run, onchange: (e) => (rule.dry_run = e.target.checked) }),
    ),
    el("div", { class: "field" },
      el("label", { text: "Parameters (JSON)" }), paramsText,
    ),
    el("div", { class: "field" },
      el("label", { text: "Action escalation" }), actionsRows,
      el("button", { class: "btn small", onclick: () => { rule.actions.push({ at: rule.actions.length + 1, action: "warn", duration_minutes: 0, delete_days: 0, reason: "" }); renderActions(); } }, "+ add action"),
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
          rule.params = JSON.parse(paramsText.value || "{}");
        } catch (e) { toast("Invalid params JSON: " + e.message, "error"); return; }
        rule.whitelist_roles = splitIds(whitelistText.value);
        rule.whitelist_channels = splitIds(channelsText.value);
        rule.whitelist_users = splitIds(usersText.value);
        const scope = scopeSel.value;
        await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", { scope, data: rule });
        if (scope === "global") {
          await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" }).catch(() => {});
        }
        await loadConfig();
        toast("Rule saved", "success");
        closeOverlay();
        renderTab();
      } }),
      el("button", { class: "btn danger", text: "Reset to global", onclick: async () => {
        await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" });
        await loadConfig();
        toast("Rule reset", "success");
        closeOverlay();
        renderTab();
      } }),
      el("button", { class: "btn", text: "Cancel", onclick: closeOverlay }),
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
          await loadConfig();
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
              await loadConfig();
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
          el("button", { class: "btn small danger", text: "✕", onclick: () => { steps.splice(i, 1); syncPlanJson(); renderSteps(); } })),
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
        await loadConfig();
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
        await loadConfig();
        toast(c.guild_override ? "Override cleared" : "Deleted", "success");
        closeOverlay();
        renderTab();
      } }),
      el("button", { class: "btn", text: "Cancel", onclick: closeOverlay }),
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
      "Templates render into the configured log channels. Variables: {user} {mention} {user_id} {guild} {channel} {actor} {reason} {rule} {action} {content} {message_link} {count} {age} {joined} {duration} {time}."),
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
          await loadConfig();
          toast("Template saved", "success");
          renderTab();
        } }),
        el("button", { class: "btn small", text: "Reset to global", onclick: async () => {
          await api(`/api/guilds/${state.guildId}/templates/${t.event}`, "DELETE", { scope: "guild" });
          await loadConfig();
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
  return el("div", {},
    el("h2", { text: "Log channels" }),
    el("p", { class: "muted", style: "margin-bottom:12px" },
      "Each event type routes to a channel in this server. Enter a channel ID or mention (<#id>)."),
    ...state.config.log_channels.map((lc) => {
      const input = el("input", { type: "text", value: (lc.effective && String(lc.effective) !== "0") ? String(lc.effective) : "", placeholder: "channel id" });
      const scopeSel = scopeSelect("This server", "Global", false);
      return el("div", { class: "row", style: "margin-bottom:8px" },
        el("label", { style: "width:180px", text: lc.event }),
        input, scopeSel,
        el("button", { class: "btn small primary", text: "Set", onclick: async () => {
          const v = input.value.trim().replace(/[<#>]/g, "");
          if (!v) return toast("Enter a channel id", "error");
          await api(`/api/guilds/${state.guildId}/logchannels/${lc.event}`, "PUT", { scope: scopeSel.value, channel_id: v });
          await loadConfig();
          toast("Log channel set", "success");
          renderTab();
        } }),
        el("button", { class: "btn small", text: "Clear", onclick: async () => {
          await api(`/api/guilds/${state.guildId}/logchannels/${lc.event}`, "DELETE", { scope: scopeSel.value });
          await loadConfig();
          toast("Cleared", "success");
          renderTab();
        } }),
      );
    }),
  );
}

// ---------------------------------------------------------------------------
// members / events / bans
// ---------------------------------------------------------------------------

function tabMembers() {
  const q = el("input", { type: "text", placeholder: "search name or id" });
  const table = el("table", {}, el("tbody", {}));
  const tbody = () => table.querySelector("tbody");
  const run = async () => {
    const rows = await api(`/api/guilds/${state.guildId}/members?q=${encodeURIComponent(q.value || "")}&limit=100`);
    tbody().innerHTML = "";
    rows.forEach((r) => tbody().append(el("tr", {},
      el("td", { text: r.username || String(r.user_id) }),
      el("td", { class: "mono", text: r.user_id }),
      el("td", { text: r.currently_in ? "in server" : "left" }),
      el("td", { text: `joined ${r.join_count}x` }),
      el("td", { text: `${r.message_count} msgs` }),
      el("td", { class: "muted", text: r.first_joined_at ? new Date(r.first_joined_at).toLocaleString() : "-" }),
      el("td", { class: "muted", text: r.last_left_at ? new Date(r.last_left_at).toLocaleString() : "-" }),
    )));
  };
  q.addEventListener("keydown", (e) => { if (e.key === "Enter") run(); });
  run().catch((e) => toast(e.message, "error"));
  return el("div", {},
    el("h2", { text: "Members" }),
    el("div", { class: "row", style: "margin-bottom:12px" }, q, el("button", { class: "btn", text: "Search", onclick: run })),
    table,
  );
}

function tabEvents() {
  const typeSel = el("select", {}, el("option", { value: "", text: "all types" }),
    ...state.config.event_types.map((t) => el("option", { value: t, text: t })));
  const userIn = el("input", { type: "text", placeholder: "user id" });
  const table = el("table", {}, el("tbody", {}));
  const tbody = () => table.querySelector("tbody");
  const run = async () => {
    const user = userIn.value.trim();
    const rows = await api(`/api/guilds/${state.guildId}/events?limit=100` +
      (typeSel.value ? `&event=${encodeURIComponent(typeSel.value)}` : "") +
      (user ? `&user=${encodeURIComponent(user)}` : ""));
    tbody().innerHTML = "";
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
  const table = el("table", {}, el("tbody", {}));
  const tbody = () => table.querySelector("tbody");
  const run = async () => {
    const rows = await api(`/api/guilds/${state.guildId}/bans?limit=200`);
    tbody().innerHTML = "";
    rows.forEach((r) => tbody().append(el("tr", {},
      el("td", { class: "mono", text: r.user_id }),
      el("td", { text: r.active ? "banned" : "unbanned" }),
      el("td", { text: r.reason || "" }),
      el("td", { class: "muted", text: new Date(r.created_at).toLocaleString() }),
      el("td", {}, r.active ? el("button", { class: "btn small", text: "Unban", onclick: async () => {
        if (!confirm(`Unban ${r.user_id}?`)) return;
        try {
          await api(`/api/guilds/${state.guildId}/unban`, "POST", { user_id: String(r.user_id), reason: "unbanned via WebUI" });
          toast("Unbanned", "success");
          run();
        } catch (e) {
          toast(e.message || String(e), "error");
        }
      } }) : null),
    )));
  };
  run().catch((e) => toast(e.message, "error"));
  return el("div", {},
    el("h2", { text: "Bans" }),
    el("p", { class: "muted", style: "margin-bottom:12px" }, "Bans performed by the bot (automod + commands). Unban via the API here or in Discord."),
    table,
  );
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

function tabSettings() {
  const cfg = state.config;
  const ov = cfg.settings_override || {};
  const prefix = el("input", { type: "text", value: cfg.settings.prefix });
  const botName = el("input", { type: "text", value: cfg.settings.bot_name });
  const retention = el("input", { type: "number", min: 0, value: cfg.settings.retention_days });

  const srcSel = el("select", {},
    ...state.guilds.filter((g) => g.id !== state.guildId).map((g) => el("option", { value: g.id, text: g.name })),
  );
  if (state.guilds.filter((g) => g.id !== state.guildId).length === 0) {
    srcSel.append(el("option", { value: "", text: "No other servers" }));
  }
  const sectionSel = el("select", {},
    ["all", "rules", "badwords", "commands", "templates", "settings"]
      .map((s) => el("option", { value: s, text: s === "all" ? "everything" : s })),
  );
  const globalPrefix = el("input", { type: "text", value: cfg.global_settings.prefix });

  /** Clear one override field via API null (inherit parent). */
  async function inheritField(field) {
    try {
      await api(`/api/guilds/${state.guildId}/settings`, "PUT", { [field]: null });
      await loadConfig();
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
          ? el("p", { class: "muted", text: "Discord nickname work is dormant while the bot is absent from this server; it will reactivate on rejoin." })
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
        settingsField(
          "Message retention (days)",
          retention,
          "retention_days",
          "0 = keep forever. Empty + Save clears a server override (inherit global).",
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
        const rawDays = String(retention.value ?? "").trim();
        if (rawDays === "") {
          // Empty means "clear override / inherit" — never "forever" (that is 0).
          // Without an override, blank is invalid (same fail-closed rule as prefix).
          if (ov.retention_days != null) body.retention_days = null;
          else {
            toast("Enter retention days (0 = forever). Empty only clears a server override.", "error");
            return;
          }
        } else {
          const days = Number(rawDays);
          if (!Number.isFinite(days) || days < 0) {
            toast("Retention must be a non-negative number (0 = keep forever)", "error");
            return;
          }
          if (days !== cfg.settings.retention_days) body.retention_days = days;
        }
        if (body.prefix === undefined && body.bot_name === undefined && body.retention_days === undefined) {
          toast("No settings changes", "success");
          return;
        }
        try {
          const res = await api(`/api/guilds/${state.guildId}/settings`, "PUT", body);
          await loadConfig();
          toast(res.nickname_queued ? "Settings saved — Discord nickname queued" : "Settings saved", "success");
          renderTab();
        } catch (e) {
          toast(e.message || String(e), "error");
        }
      } }),
      el("button", { class: "btn", text: "Re-sync Discord nickname", onclick: async () => {
        try {
          const res = await api(`/api/guilds/${state.guildId}/settings`, "PUT", {
            sync_nickname: true,
          });
          await loadConfig();
          toast(res.nickname_queued ? "Discord nickname reconciliation queued" : "No nickname work queued", "success");
          renderTab();
        } catch (e) {
          toast(e.message || String(e), "error");
        }
      } }),
    ),
  ];

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
            await loadConfig();
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
          if (!srcSel.value) return toast("Select a source server", "error");
          if (!confirm(`Copy ${sectionSel.value} from ${srcSel.selectedOptions[0].textContent}?`)) return;
          try {
            let res = await api(`/api/guilds/${state.guildId}/copy`, "POST", {
              from_guild_id: String(srcSel.value), sections: [sectionSel.value],
            });
            await loadConfig();
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
  const modal = el("div", { class: "modal-overlay", onclick: (e) => { if (e.target === modal) closeOverlay(); } });
  $("#content").append(modal);
  return modal;
}
function closeOverlay() {
  const m = $(".modal-overlay");
  if (m) m.remove();
}

const loginLink = $("#login-link");
if (loginLink) loginLink.href = authUrl("/login");
$("#logout-btn").addEventListener("click", async () => {
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
