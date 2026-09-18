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
  const tabs = { status: tabStatus, installation: tabInstallation, notifications: tabNotifications, updates: tabUpdates, backups: tabBackups, moves: tabMoves };
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
  const bot = el("input", { type: "url", value: s.bot_health_url?.value || "", placeholder: "http://127.0.0.1:18080/health" });
  const apiUrl = el("input", { type: "url", value: s.api_health_url?.value || "", placeholder: "http://127.0.0.1:8080/health" });
  const backupEnabled = el("input", { type: "checkbox" });
  backupEnabled.checked = !!s.backup_enabled?.value;
  const interval = num("backup_interval_hours", 6);
  const retain = num("backup_retain_scheduled", 24);
  const daily = num("backup_retain_daily", 7);
  const checkpoints = num("backup_retain_checkpoints", 3);
  const maxBytes = num("backup_max_archive_bytes", 268435456);
  const profile = el("select", {},
    el("option", { value: "local", text: "Local" }),
    el("option", { value: "bunny", text: "Bunny" }),
    el("option", { value: "aws", text: "AWS (EC2 + SQLite or libSQL)" }),
  );
  const origin = el("input", { type: "url", value: "http://127.0.0.1:8081", placeholder: "https://admin.example" });
  const coreUrl = el("input", { type: "url", placeholder: "libsql://CORE.lite.bunnydb.net" });
  const guildUrl = el("input", { type: "url", placeholder: "libsql://GUILD.lite.bunnydb.net" });
  const checklist = el("pre", { class: "muted", text: "Loading setup checklist…" });
  Promise.all([api("/api/install"), api("/api/install/plan")]).then(([report, plan]) => {
    if (plan?.public_origin) origin.value = plan.public_origin;
    checklist.textContent = JSON.stringify({
      operations_available: report.operations_available,
      backup_available: report.backup_available,
      capture_available: report.capture_available,
      moves_available: report.moves_available,
      discovery_available: report.discovery_available,
      update_available: report.update_available,
      private_ops_required: report.private_ops_required,
      license: report.license,
      owner_setup: report.owner_setup,
      deferred_live_checks: report.deferred_live_checks,
      docs: report.docs,
    }, null, 2);
    state.installPlan = plan;
  }).catch((e) => { checklist.textContent = e.message; toast(e.message, "error"); });
  return el("div", {},
    el("h2", { text: "Installation settings" }),
    el("p", { class: "muted", text: "Validate a public Bunny, local, or AWS topology from this source. A private ops repository is not required. Live provider checks stay deferred." }),
    el("div", { class: "card" },
      el("h3", { text: "Public setup checklist" }),
      field("Topology", profile),
      field("Public origin", origin),
      field("Core libSQL URL (Bunny/AWS remote)", coreUrl),
      field("Guild libSQL URL (Bunny/AWS remote)", guildUrl),
      checklist,
      el("button", { class: "btn", text: "Validate install plan", onclick: async () => {
        try {
          const base = state.installPlan || {};
          const bunny = profile.value === "bunny";
          const local = profile.value === "local";
          const coreUrlValue = local ? "" : coreUrl.value.trim();
          const guildUrlValue = local ? "" : guildUrl.value.trim();
          const validation = await api("/api/install/validate", "POST", {
            ...base,
            profile: profile.value,
            public_origin: origin.value,
            core_database: local ? "sqlite" : ((bunny || coreUrlValue) ? "libsql" : "sqlite"),
            guild_database: local ? "sqlite" : ((bunny || guildUrlValue) ? "libsql" : "sqlite"),
            core_url: coreUrlValue,
            guild_url: guildUrlValue,
            api_embedded_ui: profile.value !== "bunny",
            frontend_storage: bunny,
            private_ops_required: false,
            license: "LicenseRef-PolyForm-Noncommercial-1.0.0",
          });
          toast("Install plan validated (not applied)", "success");
          let current = {};
          try { current = JSON.parse(checklist.textContent) || {}; } catch (_) {}
          checklist.textContent = JSON.stringify({ ...current, validation }, null, 2);
        } catch (e) { toast(e.message, "error"); }
      } }),
    ),
    el("p", { class: "muted", text: "Applied settings below stay on this conductor. Bot process limits stay in bot configuration until a later credential-delivery feature." }),
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
      el("h3", { text: "Backup policy (applied)" }),
      field("Enable scheduled backups", backupEnabled),
      field("Schedule interval (hours)", interval),
      field("Retain scheduled points", retain),
      field("Retain daily points", daily),
      field("Retain release checkpoints", checkpoints),
      field("Archive byte budget", maxBytes),
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
            backup_enabled: backupEnabled.checked,
            backup_interval_hours: Number(interval.value),
            backup_retain_scheduled: Number(retain.value),
            backup_retain_daily: Number(daily.value),
            backup_retain_checkpoints: Number(checkpoints.value),
            backup_max_archive_bytes: Number(maxBytes.value),
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
  const allowed = ["health", "jobs", "login", "settings", "backup", "updates"];
  const selectedCats = new Set(s.notify_categories?.value || []);
  const categoryBoxes = allowed.map((name) => {
    const box = el("input", { type: "checkbox" });
    box.checked = selectedCats.has(name);
    return { name, box };
  });
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
      el("h3", { text: "Categories" }),
      ...categoryBoxes.map(({ name, box }) => field(name, box)),
      el("button", { class: "btn primary", text: "Save notification settings", onclick: async () => {
        try {
          const body = {
            revision: state.settings.revision,
            notify_enabled: enabled.checked,
            notify_mentions: mentions.checked,
            notify_verbosity: verbosity.value,
            notify_categories: categoryBoxes.filter((item) => item.box.checked).map((item) => item.name),
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

function tabUpdates() {
  const s = state.settings?.settings || {};
  const selected = new Set(s.discovery_channels?.value || ["stable"]);
  const stable = el("input", { type: "checkbox" });
  stable.checked = selected.has("stable");
  const nightly = el("input", { type: "checkbox" });
  nightly.checked = selected.has("nightly");
  const interval = el("input", { type: "number", value: String(s.discovery_interval_secs?.value ?? 3600) });
  const preop = el("select", {},
    el("option", { value: "fresh", text: "Fresh checkpoint" }),
    el("option", { value: "recent", text: "Reuse a complete archive from the last 6 hours" }),
    el("option", { value: "skip", text: "Skip checkpoint (no pre-update archive)" }),
  );
  const preopHint = el("p", { class: "muted", text: "" });
  const confirm = el("input", { type: "checkbox" });
  const catalog = el("pre", { class: "muted", text: "Loading release catalog…" });
  const list = el("div", {});
  const jobsBox = el("div", {}, el("p", { class: "muted", text: "Loading update jobs…" }));
  const recoverBox = el("pre", { class: "muted", text: "Loading recover inventory…" });
  const renderCatalog = (data) => {
    catalog.textContent = JSON.stringify({
      available: data.available,
      discovery_available: data.available,
      update_available: data.update_available,
      installed: data.installed,
      available_count: data.available_count,
      apply: data.apply,
      poll_error: data.poll_error || null,
      recover: data.recover || null,
    }, null, 2);
    recoverBox.textContent = JSON.stringify(data.recover || { status: "unknown" }, null, 2);
    list.replaceChildren();
    if (!data.available) {
      list.append(el("p", { class: "muted", text: "Discovery needs JMODGUARD_CONDUCTOR_RELEASE_FEED and JMODGUARD_CONDUCTOR_RELEASE_KEYS. Browsing a build never deploys it automatically." }));
      return;
    }
    const rows = data.releases || [];
    if (!rows.length) {
      list.append(el("p", { class: "muted", text: "No complete signed releases in the selected channels." }));
      return;
    }
    rows.forEach((row) => {
      const actions = [];
      if (data.update_available && row.compatible) {
        actions.push(el("button", { class: "btn", text: "Preview impact", onclick: async () => {
          try {
            toast(JSON.stringify(await api(`/api/updates/plan?release_id=${encodeURIComponent(row.release_id)}&preop=${encodeURIComponent(preop.value)}`)), "info");
          } catch (e) { toast(e.message, "error"); }
        } }));
        actions.push(el("button", { class: "btn primary", text: "Install this release", onclick: async () => {
          try {
            if (!confirm.checked) throw new Error("confirm the exclusive bot drain, local API restart, and conductor recover-update path");
            const queued = await api("/api/updates", "POST", {
              release_id: row.release_id,
              preop: preop.value,
              confirm_interruption: true,
            });
            toast(`Update queued (${queued.id})`, "success");
            render();
          } catch (e) { toast(e.message, "error"); }
        } }));
      }
      list.append(el("div", { class: "card" },
        el("strong", { text: `${row.release_id} · ${row.channel}` }),
        el("p", { class: "muted", text: row.compatible ? (data.update_available ? "Signed release passes current core/guild, settings-schema, and backup-format checks." : "Compatible, but install needs JMODGUARD_CONDUCTOR_UPDATE_ROOT.") : (row.blocked_reason || "Incompatible") }),
        el("p", { class: "muted", text: `commit ${row.source_commit} · ${row.published_at}` }),
        el("p", { class: "muted", text: (row.components || []).join(", ") }),
        el("p", { class: "muted", text: row.notes || "No notes." }),
        ...actions,
      ));
    });
    jobsBox.replaceChildren();
    const jobs = data.jobs || [];
    if (!jobs.length) {
      jobsBox.append(el("p", { class: "muted", text: "No update jobs yet." }));
    } else {
      jobs.forEach((job) => {
        const payload = (() => { try { return JSON.parse(job.payload_json || "{}"); } catch { return {}; } })();
        const buttons = [];
        if (job.status === "failed") {
          buttons.push(el("button", { class: "btn", text: "Retry failed update", onclick: async () => {
            try {
              await api(`/api/jobs/${job.id}/retry`, "POST", {});
              toast("Update job queued for retry", "success");
              render();
            } catch (e) { toast(e.message, "error"); }
          } }));
        }
        if (job.status === "succeeded" && !payload.rollback) {
          buttons.push(el("button", { class: "btn", text: "Rollback code", onclick: async () => {
            try {
              await api(`/api/updates/${job.id}/rollback`, "POST", { confirm_rollback: true });
              toast("Code rollback queued; the previous database is not restored", "success");
              render();
            } catch (e) { toast(e.message, "error"); }
          } }));
        }
        jobsBox.append(el("div", { class: "card" },
          el("strong", { text: `${job.kind} · ${job.status}` }),
          el("p", { class: "muted", text: job.last_error || `phase ${payload.phase || "n/a"} · ${payload.release_id || ""}` }),
          el("p", { class: "muted", text: `id: ${job.id}` }),
          ...buttons,
        ));
      });
    }
  };
  const configurePreop = (captureAvailable) => {
    if (captureAvailable) {
      preopHint.textContent = "";
      preop.querySelector('option[value="fresh"]').disabled = false;
      preop.querySelector('option[value="recent"]').disabled = false;
      return;
    }
    preop.value = "skip";
    preopHint.textContent = "Capture is not configured; install requires preop=skip unless capture stores are configured.";
    preop.querySelector('option[value="fresh"]').disabled = true;
    preop.querySelector('option[value="recent"]').disabled = true;
  };
  Promise.all([api("/api/updates"), api("/api/status")])
    .then(([data, status]) => {
      configurePreop(!!status.capture_available);
      renderCatalog(data);
    })
    .catch((e) => { catalog.textContent = e.message; });
  const refreshBtn = el("button", { class: "btn", text: "Refresh catalog", onclick: async () => {
    try {
      const [data, status] = await Promise.all([
        api("/api/updates?refresh=true"),
        api("/api/status"),
      ]);
      configurePreop(!!status.capture_available);
      renderCatalog(data);
    } catch (e) { toast(e.message, "error"); }
  } });
  return el("div", {},
    el("h2", { text: "Updates" }),
    el("p", { class: "muted", text: "Browse signed Stable and Nightly publications, then install a selected compatible build. There is no silent automatic update. Compatible rollback switches retained binaries and does not restore the old database. If conductor replacement fails, run `jmodguard-conductor recover-update` against the update root; that command does not use the new conductor or private ops." }),
    el("div", { class: "card" },
      el("h3", { text: "Discovery" }),
      field("Stable channel", stable),
      field("Nightly channel", nightly),
      field("Poll interval (seconds, background worker)", interval),
      el("button", { class: "btn primary", text: "Save discovery settings", onclick: async () => {
        try {
          const channels = [];
          if (stable.checked) channels.push("stable");
          if (nightly.checked) channels.push("nightly");
          if (!channels.length) {
            toast("Select at least one discovery channel (Stable or Nightly)", "error");
            return;
          }
          state.settings = await api("/api/settings", "PATCH", {
            revision: state.settings.revision,
            discovery_channels: channels,
            discovery_interval_secs: Number(interval.value),
          });
          toast("Discovery settings saved", "success");
          render();
        } catch (e) { toast(e.message, "error"); }
      } }),
      refreshBtn,
    ),
    el("div", { class: "card" },
      el("h3", { text: "Install" }),
      field("Pre-update checkpoint", preop),
      preopHint,
      el("label", {}, confirm, el("span", { text: " I confirm the exclusive bot drain, local API restart, and that conductor recovery uses recover-update rather than the replacement process." })),
    ),
    el("div", { class: "card" }, el("h3", { text: "Installed and availability" }), catalog),
    el("div", { class: "card" }, el("h3", { text: "Complete publications" }), list),
    el("div", { class: "card" }, el("h3", { text: "Update jobs" }), jobsBox),
    el("div", { class: "card" }, el("h3", { text: "Recover inventory" }), recoverBox),
  );
}

function tabBackups() {
  const password = el("input", { type: "password", placeholder: "Portable export/restore password" });
  const ack = el("input", { type: "checkbox" });
  const list = el("pre", { class: "muted", text: "Loading backups…" });
  const upload = el("input", { type: "file" });
  const archiveSelect = el("select", {}, el("option", { value: "", text: "Loading archives…" }));
  const failures = el("div", {}, el("p", { class: "muted", text: "Loading failed jobs…" }));
  const captureNote = el("p", { class: "muted", text: "" });
  const selectedArchive = async (statuses) => {
    if (!archiveSelect.value) throw new Error("select an archive");
    const data = await api("/api/backups");
    const row = (data.archives || []).find((item) => item.id === archiveSelect.value);
    if (!row || !statuses.includes(row.status)) throw new Error(`selected archive must be ${statuses.join(" or ")}`);
    return row;
  };
  api("/api/backups").then((data) => {
    const archives = data.archives || [];
    archiveSelect.replaceChildren(...archives.map((row) => {
      const when = row.captured_ms || row.created_ms;
      const date = when ? new Date(when).toISOString() : "unknown date";
      return el("option", { value: row.id, text: `${row.status} · ${row.kind} · ${date} · ${row.id}` });
    }));
    const preferred = archives.find((row) => row.status === "complete") || archives[0];
    archiveSelect.value = preferred?.id || "";

    failures.replaceChildren();
    const failedJobs = data.failed_jobs || [];
    if (!failedJobs.length) {
      failures.append(el("p", { class: "muted", text: "No failed backup jobs." }));
    } else {
      failedJobs.forEach((job) => failures.append(el("div", { class: "card" },
        el("strong", { text: `${job.kind} failed` }),
        el("p", { class: "muted", text: job.last_error || "No error detail" }),
        el("button", { class: "btn", text: job.kind === "backup_restore" ? "Submit restore again" : "Retry failed job", onclick: async () => {
          try {
            if (job.kind === "backup_restore") {
              const payload = JSON.parse(job.payload_json || "{}");
              if (!payload.id) throw new Error("failed restore has no archive id");
              if (payload.password_required && !password.value) throw new Error("enter the portable restore password");
              await api(`/api/backups/${payload.id}/restore`, "POST", {
                password: password.value || null,
                acknowledge_historical: ack.checked,
              });
              toast("Restore submitted again", "success");
            } else {
              await api(`/api/jobs/${job.id}/retry`, "POST", {});
              toast("Backup job queued for retry", "success");
            }
            render();
          } catch (e) { toast(e.message, "error"); }
        } }),
      )));
    }
    list.textContent = JSON.stringify({
      last_complete_ms: data.last_complete_ms,
      overdue: data.overdue,
      capture_available: data.capture_available,
      archives,
    }, null, 2);
    captureNote.textContent = data.capture_available === false
      ? "Capture is unavailable until a core capture store is configured. Upload, download and restore still use the backup zone."
      : "";
  }).catch((e) => { list.textContent = e.message; });
  return el("div", {},
    el("section", { class: "hero" },
      el("span", { class: "eyebrow", text: "BACKUPS" }),
      el("h2", { text: "Backup and recovery" }),
      el("p", { text: "Scheduled captures retry and alert; they never pause writes. Maintenance capture and restore are explicit. Live Bunny durability is not claimed by local tests." }),
    ),
    el("div", { class: "card" },
      el("h3", { text: "Create and download" }),
      el("p", { class: "muted", text: "A portable download is wrapped with the password you type here. The password is not stored. Destroying the old environment requires this file plus Discord/provider credentials supplied on the new install." }),
      captureNote,
      el("button", { class: "btn primary", text: "Create backup now", onclick: async () => {
        try {
          await api("/api/backups", "POST", { kind: "manual" });
          toast("Backup queued", "success");
          render();
        } catch (e) { toast(e.message, "error"); }
      } }),
      field("Archive", archiveSelect),
      field("Portable password", password),
      el("button", { class: "btn", text: "Download selected complete backup", onclick: async () => {
        try {
          const complete = await selectedArchive(["complete"]);
          const res = await fetch(`/api/backups/${complete.id}/download`, { credentials: "include" });
          if (!res.ok) throw new Error(`HTTP ${res.status}`);
          const blob = await res.blob();
          const url = URL.createObjectURL(blob);
          const a = document.createElement("a");
          a.href = url;
          a.download = `jmodguard-${complete.id}.jmb`;
          a.click();
          URL.revokeObjectURL(url);
        } catch (e) { toast(e.message, "error"); }
      } }),
      el("button", { class: "btn", text: "Download selected complete backup (portable)", onclick: async () => {
        try {
          const complete = await selectedArchive(["complete"]);
          const res = await fetch(`/api/backups/${complete.id}/portable`, {
            method: "POST",
            credentials: "include",
            headers: { "content-type": "application/json" },
            body: JSON.stringify({ password: password.value }),
          });
          if (!res.ok) throw new Error(`HTTP ${res.status}`);
          const blob = await res.blob();
          const url = URL.createObjectURL(blob);
          const a = document.createElement("a");
          a.href = url;
          a.download = `jmodguard-${complete.id}-portable.jmb`;
          a.click();
          URL.revokeObjectURL(url);
        } catch (e) { toast(e.message, "error"); }
      } }),
    ),
    el("div", { class: "card" },
      el("h3", { text: "Upload and restore" }),
      el("p", { class: "muted", text: "Uploads are quarantined after a format-prefix check and authenticated during restore. Select the exact archive above before restoring. Restoring after total loss can return later-deleted records; that requires the archive date/scope warning and this acknowledgment. Known remaining removals stay enforced. Pending Discord jobs are not replayed." }),
      field("Archive file", upload),
      field("I acknowledge this historical snapshot may restore later-deleted data", ack),
      el("button", { class: "btn", text: "Upload archive", onclick: async () => {
        try {
          const file = upload.files && upload.files[0];
          if (!file) throw new Error("choose a file");
          const res = await fetch("/api/backups/upload", {
            method: "POST",
            credentials: "include",
            body: file,
          });
          const data = await res.json().catch(() => ({}));
          if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
          toast(`Quarantined ${data.id}`, "success");
          render();
        } catch (e) { toast(e.message, "error"); }
      } }),
      el("button", { class: "btn primary", text: "Restore selected archive", onclick: async () => {
        try {
          const row = await selectedArchive(["complete", "quarantined"]);
          await api(`/api/backups/${row.id}/restore`, "POST", {
            password: password.value || null,
            acknowledge_historical: ack.checked,
          });
          toast("Restore queued into the configured restore target", "success");
        } catch (e) { toast(e.message, "error"); }
      } }),
    ),
    el("div", { class: "card" }, el("h3", { text: "Failed backup jobs" }), failures),
    el("div", { class: "card" }, el("h3", { text: "Catalog" }), list),
  );
}

function tabMoves() {
  const guild = el("input", { placeholder: "Guild snowflake" });
  const dest = el("input", { placeholder: "Destination store id" });
  const confirm = el("input", { type: "checkbox" });
  const preop = el("select", {},
    el("option", { value: "fresh", text: "Fresh checkpoint" }),
    el("option", { value: "recent", text: "Reuse a complete archive from the last 6 hours" }),
    el("option", { value: "skip", text: "Skip checkpoint (no pre-move archive)" }),
  );
  const planBox = el("pre", { class: "muted", text: "Choose a guild and destination, then preview." });
  const jobList = el("div", {});
  const failedMoves = el("div", {}, el("p", { class: "muted", text: "Loading failed move jobs…" }));
  const catalog = el("pre", { class: "muted", text: "Loading…" });
  api("/api/moves").then((data) => {
    catalog.textContent = JSON.stringify(data.catalog || data, null, 2);
    jobList.replaceChildren();
    const jobs = data.jobs || [];
    if (!jobs.length) {
      jobList.append(el("p", { class: "muted", text: "No guild-move jobs yet." }));
    } else {
      jobs.forEach((job) => jobList.append(el("div", { class: "card" },
        el("strong", { text: `${job.kind} · ${job.status}` }),
        el("p", { class: "muted", text: job.last_error || "No error detail" }),
        el("p", { class: "muted", text: `id: ${job.id}` }),
      )));
    }
    failedMoves.replaceChildren();
    const failed = data.failed_jobs || jobs.filter((job) => job.status === "failed");
    if (!failed.length) {
      failedMoves.append(el("p", { class: "muted", text: "No failed move jobs." }));
    } else {
      failed.forEach((job) => failedMoves.append(el("div", { class: "card" },
        el("strong", { text: "guild_move failed" }),
        el("p", { class: "muted", text: job.last_error || "No error detail" }),
        el("p", { class: "muted", text: `id: ${job.id}` }),
        el("button", { class: "btn", text: "Retry failed move", onclick: async () => {
          try {
            await api(`/api/jobs/${job.id}/retry`, "POST", {});
            toast("Move job queued for retry", "success");
            render();
          } catch (e) { toast(e.message, "error"); }
        } }),
      )));
    }
    if (!data.available) {
      planBox.textContent = "Moves need a core capture store plus shared and dedicated guild-store paths in JMODGUARD_CONDUCTOR_GUILD_STORES. Bot/API must already have those destination credentials.";
    }
  }).catch((e) => {
    catalog.textContent = e.message;
    jobList.append(el("p", { class: "muted", text: e.message }));
    failedMoves.replaceChildren(el("p", { class: "muted", text: e.message }));
  });
  return el("div", {},
    el("h2", { text: "Guild database moves" }),
    el("p", { class: "muted", text: "Move one enrolled guild between an already-configured shared store and its dedicated store. Neighboring guilds on the shared database are left in place. Destination credentials stay in bot/API process configuration; adding a new store requires restarting those services first. If a move fails after the write-fence is installed, use Retry failed move below—do not start a new move for the same guild." }),
    el("div", { class: "card" },
      el("h3", { text: "Plan and start" }),
      field("Guild id", guild),
      field("Destination store id", dest),
      field("Pre-move checkpoint", preop),
      el("label", {}, confirm, el("span", { text: " I confirm this guild will be interrupted, Discord actions will not be replayed, and bot/API already have the destination store." })),
      el("div", { class: "row" },
        el("button", { class: "btn", text: "Preview impact", onclick: async () => {
          try {
            const data = await api(`/api/moves/plan?guild_id=${encodeURIComponent(guild.value.trim())}&destination_store_id=${encodeURIComponent(dest.value.trim())}`);
            planBox.textContent = JSON.stringify(data, null, 2);
          } catch (e) { toast(e.message, "error"); }
        } }),
        el("button", { class: "btn primary", text: "Start move", onclick: async () => {
          try {
            if (!confirm.checked) throw new Error("confirm the shared impact before starting");
            const data = await api("/api/moves", "POST", {
              guild_id: guild.value.trim(),
              destination_store_id: dest.value.trim(),
              preop: preop.value,
              confirm_shared_impact: true,
            });
            toast(`Move queued ${data.id}`, "success");
            render();
          } catch (e) { toast(e.message, "error"); }
        } }),
      ),
      planBox,
    ),
    el("div", { class: "card" }, el("h3", { text: "Failed move jobs" }), failedMoves),
    el("div", { class: "card" }, el("h3", { text: "Move jobs" }), jobList),
    el("div", { class: "card" }, el("h3", { text: "Catalog" }), catalog),
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
