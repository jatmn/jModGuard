const $ = (sel) => document.querySelector(sel);
const state = { tab: "status", me: null, settings: null, leaseTimer: null };

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  Object.entries(attrs || {}).forEach(([key, value]) => {
    if (value == null || value === false) return;
    if (key === "text") node.textContent = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2).toLowerCase(), value);
    else if (key === "class") node.className = value;
    else node.setAttribute(key, value === true ? "" : String(value));
  });
  children.flat().forEach((child) => { if (child) node.append(child); });
  return node;
}

function toast(msg, kind = "info") {
  const t = $("#toast");
  t.textContent = msg;
  t.className = "show " + kind;
  clearTimeout(t._timer);
  t._timer = setTimeout(() => { t.className = ""; }, kind === "error" ? 5000 : 2600);
}

async function api(path, method = "GET", body = null) {
  const opts = { method, headers: {}, credentials: "include", cache: "no-store" };
  if (body !== null) { opts.headers["content-type"] = "application/json"; opts.body = JSON.stringify(body); }
  const res = await fetch(path, opts);
  const text = await res.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch (_) { data = text; }
  if (!res.ok) throw new Error((data && data.error) || `HTTP ${res.status}`);
  return data;
}

function showView(id) {
  ["login-view", "main-view"].forEach((name) => $(`#${name}`)?.classList.toggle("hidden", name !== id));
}

function stopLease() {
  if (state.leaseTimer) { clearInterval(state.leaseTimer); state.leaseTimer = null; }
}

function startLease() {
  stopLease();
  const ping = () => api("/api/telemetry/lease", "POST", {}).catch(() => {});
  ping();
  state.leaseTimer = setInterval(ping, 10000);
}

function render() {
  document.querySelectorAll("#tabs button").forEach((b) => {
    b.classList.toggle("active", b.dataset.tab === state.tab);
  });
  const content = $("#content");
  content.replaceChildren();
  const tabs = { status: tabStatus, installation: tabInstallation, notifications: tabNotifications, updates: tabLater, backups: tabLater };
  content.append((tabs[state.tab] || tabStatus)());
  if (state.tab === "status" && !document.hidden) startLease();
  else stopLease();
}

function tabStatus() {
  const root = el("div", {},
    el("section", { class: "hero" },
      el("span", { class: "eyebrow", text: "SYSTEM STATUS" }),
      el("h2", { text: "Service health" }),
      el("p", { text: "Live sampling runs while this page is visible, then returns to economical polling." }),
    ),
    el("div", { class: "stat-grid", id: "status-stats" }, el("div", { class: "stat" }, el("div", { class: "value", text: "…" }), el("div", { class: "label", text: "Loading" }))),
    el("div", { class: "card" }, el("h3", { text: "Recent samples" }), el("pre", { class: "muted", id: "status-samples", text: "Loading…" })),
  );
  Promise.all([api("/api/status"), api("/api/telemetry")]).then(([status, telemetry]) => {
    const stats = $("#status-stats");
    if (!stats) return;
    stats.replaceChildren(
      el("div", { class: "stat" }, el("div", { class: "value", text: String(status.conductor?.uptime_secs ?? "—") }), el("div", { class: "label", text: "Conductor uptime (s)" })),
      el("div", { class: "stat" }, el("div", { class: "value", text: status.bot?.ok ? "ok" : (status.bot?.configured ? "down" : "unset") }), el("div", { class: "label", text: "Bot health" })),
      el("div", { class: "stat" }, el("div", { class: "value", text: status.api?.ok ? "ok" : (status.api?.configured ? "down" : "unset") }), el("div", { class: "label", text: "API health" })),
      el("div", { class: "stat" }, el("div", { class: "value", text: status.conductor?.rss_bytes != null ? `${Math.round(status.conductor.rss_bytes / 1048576)} MiB` : "—" }), el("div", { class: "label", text: "Observed RSS" })),
    );
    const samples = $("#status-samples");
    if (samples) samples.textContent = JSON.stringify((telemetry.samples || []).slice(0, 8), null, 2);
  }).catch((e) => toast(e.message, "error"));
  return root;
}

function field(label, input) {
  return el("div", { class: "field" }, el("label", { text: label }), input);
}

function tabInstallation() {
  const s = state.settings?.settings || {};
  const num = (key, fallback) => el("input", { type: "number", value: String(s[key]?.value ?? fallback) });
  const idle = num("session_idle_secs", 3600);
  const absolute = num("session_absolute_secs", 86400);
  const bg = num("telemetry_background_secs", 60);
  const live = num("telemetry_live_secs", 5);
  const history = num("telemetry_history_hours", 48);
  const rows = num("telemetry_max_rows", 4096);
  const lease = num("lease_secs", 30);
  const bot = el("input", { type: "url", value: s.bot_health_url?.value || "", placeholder: "http://127.0.0.1:8080/health" });
  const apiUrl = el("input", { type: "url", value: s.api_health_url?.value || "", placeholder: "http://127.0.0.1:8080/health" });
  return el("div", {},
    el("h2", { text: "Installation settings" }),
    el("p", { class: "muted", text: "These values apply in the conductor immediately. Bot process limits stay in bot configuration until a later credential-delivery feature." }),
    el("div", { class: "card" },
      el("h3", { text: "Sessions and monitoring" }),
      field("Session idle (seconds, applied)", idle),
      field("Session absolute (seconds, applied)", absolute),
      field("Background sample interval (seconds, applied)", bg),
      field("Live sample interval (seconds, applied)", live),
      field("History window (hours, applied)", history),
      field("History row budget (applied)", rows),
      field("Dashboard lease (seconds, applied)", lease),
      field("Bot /health URL", bot),
      field("API /health URL", apiUrl),
      el("button", { class: "btn primary", text: "Save applied settings", onclick: async () => {
        try {
          const saved = await api("/api/settings", "PATCH", {
            revision: state.settings.revision,
            session_idle_secs: Number(idle.value),
            session_absolute_secs: Number(absolute.value),
            telemetry_background_secs: Number(bg.value),
            telemetry_live_secs: Number(live.value),
            telemetry_history_hours: Number(history.value),
            telemetry_max_rows: Number(rows.value),
            lease_secs: Number(lease.value),
            bot_health_url: bot.value,
            api_health_url: apiUrl.value,
          });
          state.settings = saved;
          toast("Settings applied", "success");
          render();
        } catch (e) { toast(e.message, "error"); }
      } }),
    ),
    el("div", { class: "card" },
      el("h3", { text: "Process-owned (not applied here)" }),
      el("p", { class: "muted", text: s.process_owned?.image_spam || "Bot image-spam limits stay in the bot process." }),
      el("p", { class: "muted", text: s.process_owned?.log_discord_concurrency || "" }),
      el("p", { class: "muted", text: s.process_owned?.enrollment_repair || "" }),
    ),
  );
}

function tabNotifications() {
  const s = state.settings?.settings || {};
  const enabled = el("input", { type: "checkbox" });
  enabled.checked = !!s.notify_enabled?.value;
  const webhook = el("input", { type: "url", placeholder: "Incoming webhook URL (write-only)" });
  const mentions = el("input", { type: "checkbox" });
  mentions.checked = !!s.notify_mentions?.value;
  const verbosity = el("select", {},
    el("option", { value: "errors", text: "Errors only" }),
    el("option", { value: "all", text: "All selected events" }),
  );
  verbosity.value = s.notify_verbosity?.value || "errors";
  const health = el("pre", { class: "muted", text: "Loading delivery health…" });
  api("/api/notifications/health").then((data) => {
    health.textContent = JSON.stringify(data.receipts || [], null, 2);
  }).catch((e) => { health.textContent = e.message; });
  return el("div", {},
    el("h2", { text: "Owner notifications" }),
    el("p", { class: "muted", text: "Independent of guild log channels. The webhook secret is never returned after save. Discovery here cannot enable auto-updates." }),
    el("div", { class: "card" },
      el("h3", { text: "Discord incoming webhook" }),
      el("p", { class: "muted", text: s.notify_webhook_configured?.value ? "A webhook is configured." : "No webhook configured." }),
      field("Enable delivery", enabled),
      field("Webhook URL", webhook),
      field("Allow mention parsing", mentions),
      field("Verbosity", verbosity),
      el("button", { class: "btn primary", text: "Save notification settings", onclick: async () => {
        try {
          const body = {
            revision: state.settings.revision,
            notify_enabled: enabled.checked,
            notify_mentions: mentions.checked,
            notify_verbosity: verbosity.value,
          };
          if (webhook.value.trim()) body.notify_webhook_url = webhook.value.trim();
          state.settings = await api("/api/settings", "PATCH", body);
          toast("Notification settings saved", "success");
          render();
        } catch (e) { toast(e.message, "error"); }
      } }),
      el("button", { class: "btn", text: "Send test notification", onclick: async () => {
        try {
          await api("/api/notifications/test", "POST", {});
          toast("Test notification queued", "success");
        } catch (e) { toast(e.message, "error"); }
      } }),
    ),
    el("div", { class: "card" }, el("h3", { text: "Delivery health" }), health),
  );
}

function tabLater() {
  const title = state.tab === "updates" ? "Updates" : "Backups";
  return el("div", {},
    el("h2", { text: title }),
    el("div", { class: "card unavailable" },
      el("h3", { text: "Not available yet" }),
      el("p", { text: "This control is intentionally inactive. A later change will add a working action here; this page does not expose a non-functional button." }),
    ),
  );
}

function initTheme() {
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  let preference = "system";
  try { preference = window.localStorage.getItem("jmodguard.theme") || "system"; } catch (_) {}
  const apply = () => {
    const theme = preference === "system" ? (media.matches ? "dark" : "light") : preference;
    document.documentElement.dataset.theme = theme;
  };
  document.querySelectorAll(".theme-select").forEach((select) => {
    select.value = preference;
    select.addEventListener("change", () => {
      preference = select.value;
      try { window.localStorage.setItem("jmodguard.theme", preference); } catch (_) {}
      apply();
    });
  });
  media.addEventListener("change", apply);
  apply();
}

async function boot() {
  initTheme();
  document.querySelectorAll("#tabs button").forEach((b) => b.addEventListener("click", () => { state.tab = b.dataset.tab; render(); }));
  $("#logout-btn")?.addEventListener("click", async () => {
    stopLease();
    try { await api("/logout", "POST", {}); } catch (_) {}
    window.location = "/";
  });
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stopLease();
    else if (state.tab === "status") startLease();
  });
  window.addEventListener("pagehide", stopLease);
  try {
    state.me = await api("/api/me");
    state.settings = await api("/api/settings");
    $("#user-chip").textContent = `operator ${state.me.user.id}`;
    showView("main-view");
    render();
  } catch (_) {
    showView("login-view");
  }
}

if (document.getElementById("app")) boot();
