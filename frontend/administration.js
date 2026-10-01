/* Owner-only Administration page. The API enforces ownership and Origin for
 * every mutation; hidden controls do not provide authorization. */
const cards = document.getElementById("update-cards");
const statusNode = document.getElementById("update-status");
const updateView = document.getElementById("updates");
const signIn = document.getElementById("sign-in");
const usageView = document.getElementById("database-usage");
const usageCards = document.getElementById("usage-cards");
const usageStatus = document.getElementById("usage-status");
let activeView = "updates";
let snapshot = null;
let usageRefreshPromise = null;
const detailRequests = new Map();
let refreshing = null;
let busy = false;
let stale = true;
let authorized = false;
let accessDenied = false;
let refreshTimer;
let actionMessage = "";
const componentViews = new Map();
let browserView;
const refreshButton = document.getElementById("refresh-status");

function initTheme() {
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const select = document.getElementById("appearance");
  let preference = "system";
  try { preference = window.localStorage.getItem("jmodguard.theme") || "system"; } catch (_) { /* Storage is optional. */ }
  if (!["light", "dark", "system"].includes(preference)) preference = "system";
  const apply = () => {
    const dark = preference === "dark" || (preference === "system" && media.matches);
    document.documentElement.dataset.theme = dark ? "dark" : "light";
    document.querySelector('meta[name="theme-color"]').content = dark ? "#101e33" : "#e0eeff";
    select.value = preference;
  };
  select.addEventListener("change", () => {
    preference = select.value;
    try { window.localStorage.setItem("jmodguard.theme", preference); } catch (_) { /* Storage is optional. */ }
    apply();
  });
  media.addEventListener("change", apply);
  apply();
}

function applicationPending(record) {
  // A launcher failure changes the shared state but leaves a service request active.
  // Failed WebUI publications intentionally retain desired for recovery, not retry.
  return record.state === "queued" || Boolean(record.component !== "webui" &&
    record.desired && record.desired.digest !== record.installed?.digest);
}

function pending(record) {
  return applicationPending(record) || Boolean(record.desired_launcher &&
    record.desired_launcher.digest !== record.installed_launcher?.digest);
}

function notice(message, tone = "neutral") {
  statusNode.textContent = message;
  statusNode.className = `administration-notice administration-${tone}`;
}

function scheduleRefresh() {
  clearTimeout(refreshTimer);
  if (accessDenied || document.visibilityState === "hidden") return;
  if (activeView === "usage") {
    refreshTimer = setTimeout(() => refreshUsage(), 60000);
    return;
  }
  refreshTimer = setTimeout(() => refresh(), snapshot?.components.some(pending) ? 2000 : 30000);
}

function apiBase() {
  return (window.JMODGUARD_API_BASE || "").replace(/\/$/, "");
}

const base = apiBase();
document.getElementById("sign-in-link").href = base
  ? `${base}/login?return_to=${encodeURIComponent(window.location.origin)}`
  : "/login?return_to=%2Fadministration%2Findex.html";

function node(tag, text, className) {
  const element = document.createElement(tag);
  if (text != null) element.textContent = text;
  if (className) element.className = className;
  return element;
}

async function request(path, body) {
  const response = await fetch(apiBase() + path, {
    method: body === undefined ? "GET" : "POST",
    credentials: "include", cache: "no-store",
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  if (response.status === 401 || (path === "/api/me" && response.status === 400) ||
      (response.status === 400 && ["not authenticated", "session expired"].includes(value.error))) {
    authorized = false;
    accessDenied = true;
    stale = true;
    clearTimeout(refreshTimer);
    signIn.classList.remove("hidden");
    updateView.classList.add("hidden");
    usageView.classList.add("hidden");
    throw new Error("Sign in again to manage updates.");
  }
  if (response.status === 403) {
    authorized = false;
    accessDenied = true;
    stale = true;
    clearTimeout(refreshTimer);
    updateView.classList.add("hidden");
    usageView.classList.add("hidden");
  }
  if (!response.ok) throw new Error(value.error || `Request failed (${response.status})`);
  return value;
}

function age(timestamp) {
  if (!timestamp) return "unknown";
  const seconds = Math.max(0, Math.floor(Date.now() / 1000) - timestamp);
  if (seconds < 60) return "just now";
  if (seconds < 3600) return `${Math.floor(seconds / 60)} minutes ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} hours ago`;
  return `${Math.floor(seconds / 86400)} days ago`;
}

function uptime(timestamp) {
  if (!timestamp) return "unknown";
  const elapsed = Math.max(0, Math.floor(Date.now() / 1000) - timestamp);
  const days = Math.floor(elapsed / 86400);
  const hours = Math.floor(elapsed % 86400 / 3600);
  const minutes = Math.floor(elapsed % 3600 / 60);
  const seconds = elapsed % 60;
  return [days ? `${days}d` : "", days || hours ? `${hours}h` : "",
    days || hours || minutes ? `${minutes}m` : "", `${seconds}s`].filter(Boolean).join(" ");
}

function line(parent, label, value) {
  const item = node("p", null, "administration-detail");
  const strong = node("strong", `${label}: `);
  item.append(strong, node("span", String(value)));
  parent.append(item);
}

function restorePolicySelections() {
  for (const view of componentViews.values()) {
    view.channelValue = undefined;
    view.cadenceValue = undefined;
  }
}

async function mutate(record, label, callback) {
  if (busy || stale || !authorized) return;
  busy = true;
  clearTimeout(refreshTimer);
  // Finish any older read before submitting; it must never overwrite the result.
  if (refreshing) await refreshing;
  if (!authorized || stale) { busy = false; restorePolicySelections(); render(); scheduleRefresh(); return; }
  actionMessage = `${record.component.toUpperCase()}: ${label}…`;
  notice(actionMessage);
  render();
  try {
    const result = await callback();
    if (result.components) {
      snapshot = result;
      stale = false;
    } else {
      await refresh();
    }
    // The queued acknowledgement is not progress. Say we are waiting only when
    // the snapshot just read still has that component's work pending.
    const current = snapshot?.components?.find((item) => item.component === record.component);
    const name = record.component.toUpperCase();
    actionMessage = result.queued
      ? current && pending(current)
        ? `${name}: update request accepted. Waiting for the component to report its result.`
        : `${name}: update request accepted.`
      : `${name}: ${label} completed.`;
    if (!stale) notice(actionMessage, "good");
  } catch (error) {
    const message = `${record.component.toUpperCase()}: ${error.message}`;
    // A failed check may have persisted an error; show that state too.
    if (authorized) await refresh();
    actionMessage = message;
    notice(message, "bad");
  } finally {
    busy = false;
    restorePolicySelections();
    render();
    scheduleRefresh();
  }
}

function action(label, callback, view) {
  const button = node("button", label, "btn btn-sm");
  button.type = "button";
  button.addEventListener("click", () => {
    // Read the record when the request is built, after mutate finishes any in-flight refresh.
    return mutate(view.record, label, () => callback(view.record));
  });
  return button;
}

// Keep controls mounted during status reads so native selects and keyboard focus survive.
function createComponentView(record) {
  const view = { record };
  view.card = node("section", null, "card bg-base-100 administration-card");
  const content = node("div", null, "card-body");
  view.heading = node("div", null, "administration-card-heading");
  view.info = node("div", null, "administration-info");
  const details = node("details", null, "administration-technical");
  details.append(node("summary", "Version & technical details"));
  view.technical = node("div");
  details.append(view.technical);
  view.help = node("p", null, "muted administration-help");
  const row = node("div", null, "card-actions administration-actions");
  view.check = action("Check now", (item) => request("/api/updates/check", { component: item.component }), view);
  view.install = action(record.component === "webui" ? "Update WebUI" : "Update",
    (item) => request("/api/updates/install", { component: item.component, digest: item.candidate.digest }), view);
  row.append(view.check, view.install);
  if (record.component !== "webui") {
    view.launcher = action("Update launcher", (item) => request("/api/updates/install", { component: item.component, digest: item.candidate.digest, launcher: true }), view);
    const advanced = node("details", null, "administration-technical");
    advanced.append(node("summary", "Advanced launcher controls"),
      node("p", "The launcher starts and recovers this service. Normal Update includes any required launcher update first. Use this control only for an optional launcher update; it restarts the service."), view.launcher);
    row.append(advanced);
  }
  view.policy = node("fieldset", null, "administration-policy");
  view.policy.append(node("legend", "Release policy"));
  view.channel = node("select", null, "select select-bordered select-sm");
  view.channel.setAttribute("aria-label", `${record.component} release channel`);
  for (const value of ["latest", "stable", "nightly"]) {
    const option = node("option", value === "latest" ? "Latest published build" : value === "stable" ? "Stable releases" : "Nightly releases");
    option.value = value;
    view.channel.append(option);
  }
  view.channel.addEventListener("change", () => {
    const value = view.channel.value;
    return mutate(view.record, "Save release channel", () => {
      const item = view.record;
      return request("/api/updates/policy", { component: item.component, channel: value, cadence: value === "latest" ? "off" : item.cadence });
    });
  });
  const channelLabel = node("label", "Release channel", "field");
  channelLabel.append(view.channel);
  view.cadence = node("select", null, "select select-bordered select-sm");
  view.cadence.setAttribute("aria-label", `${record.component} automatic installation cadence`);
  for (const value of ["off", "daily", "weekly", "monthly"]) {
    const option = node("option", value === "off" ? "Automatic install off" : value);
    option.value = value;
    view.cadence.append(option);
  }
  view.cadence.addEventListener("change", () => {
    const value = view.cadence.value;
    return mutate(view.record, "Save automatic installation", () => {
      const item = view.record;
      return request("/api/updates/policy", { component: item.component, channel: item.channel, cadence: value });
    });
  });
  const cadenceLabel = node("label", "Automatic installation", "field");
  cadenceLabel.append(view.cadence);
  view.policy.append(channelLabel, cadenceLabel);
  content.append(view.heading, view.info, details, view.help, row, view.policy);
  view.card.append(content);
  cards.append(view.card);
  return view;
}

function badge(text, tone, icon) {
  const item = node("span", null, `badge administration-badge administration-${tone}`);
  const symbol = node("span", icon);
  symbol.setAttribute("aria-hidden", "true");
  item.append(symbol, node("span", text));
  return item;
}

function releaseText(release) {
  return release ? `${release.release_id} · ${release.source_sha} · ${release.published_at}` : "No checked candidate";
}

function render() {
  refreshButton.disabled = busy;
  if (!snapshot) return;
  const loaded = window.JMODGUARD_WEBUI_SHA || "unknown (local or legacy asset)";
  const installedWeb = snapshot.components.find((item) => item.component === "webui")?.observed?.source_sha || "unknown or changed outside this updater";
  if (!browserView) {
    const browser = document.getElementById("browser-version");
    const details = node("details");
    details.append(node("summary", "Browser version"));
    const technical = node("div");
    details.append(technical);
    const warning = badge("A different WebUI version is installed. Reload this page to load it.", "warn", "↻");
    const reload = node("button", "Reload page", "btn btn-sm");
    reload.type = "button";
    reload.addEventListener("click", () => window.location.reload());
    browser.append(details, warning, reload);
    browserView = { technical, warning, reload };
  }
  browserView.technical.replaceChildren();
  line(browserView.technical, "Loaded WebUI SHA", loaded);
  line(browserView.technical, "Storage-observed SHA", installedWeb);
  const differentWeb = /^[a-f0-9]{40}$/.test(loaded) && /^[a-f0-9]{40}$/.test(installedWeb) && loaded !== installedWeb;
  browserView.warning.hidden = !differentWeb;
  browserView.reload.hidden = !differentWeb;
  browserView.reload.disabled = busy;
  document.getElementById("automatic-state").textContent = snapshot.unattended_enabled
    ? "Automatic installation policies are available."
    : "Automatic installation is disabled for this deployment. Hourly checks and Check now still work.";
  for (const record of snapshot.components) {
    let view = componentViews.get(record.component);
    if (!view) {
      view = createComponentView(record);
      componentViews.set(record.component, view);
    }
    view.record = record;
    const content = view.info;
    const details = view.technical;
    const heading = view.heading;
    content.replaceChildren();
    details.replaceChildren();
    heading.replaceChildren();
    const identity = node("div");
    identity.append(node("h2", record.component.toUpperCase(), "card-title"));
    identity.append(node("p", {api: "Dashboard service", bot: "Discord moderation", webui: "Browser workspace"}[record.component], "muted"));
    heading.append(identity);
    const fresh = record.observed_at && Date.now() / 1000 - record.observed_at < 7200 && record.state !== "drift";
    const observed = fresh ? record.observed : null;
    line(details, "Base image build SHA", observed?.base_image_sha || (record.component === "api" ? snapshot.base_image_sha : "unknown"));
    line(details, record.component === "webui" ? "Storage-observed SHA" : "Running executable SHA", observed?.source_sha || "unknown");
    line(details, "Verified running file SHA-256", observed?.executable_sha256 || "unknown");
    line(content, "Uptime", uptime(observed?.started_at));
    line(details, "Managed release installed", age(record.installed_at));
    const checked = record.checked_at && Date.now() / 1000 - record.checked_at < 7200 && !record.check_error;
    const candidateCurrent = checked && record.candidate?.channel === record.channel;
    const webuiRepacked = record.component === "webui" && [record.installed, record.previous, record.desired].some((release) => release?.source_sha === record.candidate?.source_sha && release.digest !== record.candidate?.digest);
    const releaseLabel = record.candidate?.release_id || "No checked candidate";
    line(content, "Available release", candidateCurrent ? (/^[a-f0-9]{40}$/.test(releaseLabel) ? releaseLabel.slice(0, 12) : releaseLabel) : "Check now to confirm this channel");
    line(details, "Available release", candidateCurrent ? releaseText(record.candidate) : "No current candidate");
    if (webuiRepacked) line(content, "Release warning", "This WebUI source SHA has a different image digest; file bytes may differ, so publish a new source commit");
    const compatibility = snapshot.compatibility?.[record.component];
    const launcher = snapshot.launchers?.[record.component];
    if (record.component !== "webui") {
      line(content, "Launcher requirement", candidateCurrent ? launcher?.reason || "Check with a launcher-aware API to determine this release's requirement." : "Check now to determine the release requirement.");
      if (launcher?.running != null) line(details, "Launcher capability", `${launcher.running} running; ${launcher.minimum} required; ${launcher.available} included in release`);
    }
    if (record.component !== "webui" && snapshot.coordinated_upgrade_path === "/updates/recovery") {
      const target = record.candidate;
      const changesSchema = target && observed && (target.result_schema !== observed.max_schema ||
        target.upgrade?.guild_schema !== observed.guild_schema);
      if (changesSchema) line(content, "Required action", "This release changes the database. Update bot and API together; both services will briefly stop.");
      if (snapshot.service_upgrade) line(content, "Paired upgrade", snapshot.service_upgrade.error || snapshot.service_upgrade.phase);
      const pair = node("a", "Update bot and API together / recovery");
      pair.href = `${apiBase()}/updates/recovery`;
      content.append(pair);
    }

    if (candidateCurrent) line(content, "Compatibility", compatibility?.installable ? "Ready for this deployment" : compatibility?.reason || "Unable to verify");
    line(content, "Last check", record.checked_at ? age(record.checked_at) : "never");
    const bakedSourceCurrent = record.component !== "webui" && !record.installed && observed &&
      observed.source_sha === observed.base_image_sha && observed.source_sha === record.candidate?.source_sha;
    let availability = "unknown";
    if (candidateCurrent && observed && !webuiRepacked) {
      if (record.installed?.digest === record.candidate?.digest && observed.source_sha === record.candidate?.source_sha) {
        availability = "up to date";
      } else if (bakedSourceCurrent) {
        availability = "no source update (image digest unverified)";
      } else if (record.candidate && observed.source_sha !== record.candidate.source_sha) {
        availability = "update available";
      }
    }
    const inProgress = pending(record);
    const problem = record.check_error || record.last_error || record.state === "failed" || record.state === "drift";
    heading.append(badge(stale ? "Status unavailable" : inProgress ? "Update pending" : problem ? "Needs attention" : availability === "up to date" ? "Up to date" : availability === "update available" ? "Update available" : "Check status",
      stale || problem ? "bad" : inProgress || availability === "update available" ? "warn" : availability === "up to date" ? "good" : "neutral",
      stale || problem ? "!" : inProgress ? "↻" : availability === "up to date" ? "✓" : "↑"));
    line(content, "Availability", availability);
    if (record.desired_launcher && record.desired_launcher.digest !== record.installed_launcher?.digest) {
      line(content, "Launcher", "Update pending — waiting for the launcher to report its result");
    }
    line(content, "Update state", record.check_error ? `Check failed: ${record.check_error}` : record.last_error ? record.state === "drift" ? `Storage drift: ${record.last_error}` : `Install failed: ${record.last_error}` : record.state);
    line(details, "Launcher release", record.installed_launcher ? releaseText(record.installed_launcher) : "baked into base image");
    line(details, "Launcher SHA-256", record.launcher_sha256 || "unknown");
    const pairActive = snapshot.service_upgrade && !["complete", "aborted"].includes(snapshot.service_upgrade.phase);
    const canApply = !pairActive && snapshot.apply_enabled && candidateCurrent && !webuiRepacked && compatibility?.installable && (record.component === "webui" ? snapshot.webui_storage_configured : Boolean(observed?.self_update_enabled));
    const candidatePending = applicationPending(record) && record.desired?.digest === record.candidate?.digest;
    const applicationCurrent = record.component !== "webui" && record.installed?.digest === record.candidate?.digest;
    view.check.disabled = busy || stale;
    view.install.hidden = !record.candidate;
    view.install.disabled = busy || stale || !canApply || applicationCurrent || candidatePending;
    if (view.launcher) {
      view.launcher.hidden = !record.candidate;
      view.launcher.disabled = busy || stale || pairActive || !snapshot.apply_enabled || !candidateCurrent || !launcher?.installable || record.desired_launcher?.digest === record.candidate?.digest || record.installed_launcher?.digest === record.candidate?.digest;
    }
    view.help.textContent = !snapshot.apply_enabled ? "On-demand installation is disabled for this deployment."
      : record.component === "webui" ? !snapshot.webui_storage_configured ? "Configure WebUI Storage to enable installation." : ""
      : !observed?.self_update_enabled ? "Self-update is not enabled or the running component could not be verified." : "";
    view.help.hidden = !view.help.textContent;
    view.policy.disabled = busy || stale;
    // Do not reset an in-progress native selection on an unchanged server value.
    if (view.channelValue !== record.channel) view.channel.value = record.channel;
    if (view.cadenceValue !== record.cadence) view.cadence.value = record.cadence;
    view.channelValue = record.channel;
    view.cadenceValue = record.cadence;
    for (const option of view.cadence.children) {
      option.disabled = option.value !== "off" && (!snapshot.unattended_enabled || record.channel === "latest");
    }
  }
}

function refresh() {
  if (activeView !== "updates" || document.visibilityState === "hidden") return;
  if (refreshing) return refreshing;
  clearTimeout(refreshTimer);
  refreshButton.disabled = busy;
  refreshing = (async () => {
    try {
      if (!authorized) {
        const me = await request("/api/me", {});
        if (activeView !== "updates" || document.visibilityState === "hidden") return;
        if (!me.is_bot_owner) {
          accessDenied = true;
          updateView.classList.add("hidden");
          notice("Only the configured bot owner can manage updates.", "bad");
          return;
        }
        authorized = true;
        accessDenied = false;
      }
      snapshot = await request("/api/updates", {});
      if (activeView !== "updates" || document.visibilityState === "hidden") return;
      if (!busy) actionMessage = "";
      stale = false;
      signIn.classList.add("hidden");
      updateView.classList.remove("hidden");
      notice(actionMessage || "Status refreshed. Release availability reflects the last successful release check.");
      render();
    } catch (error) {
      stale = true;
      notice(`Status could not be refreshed: ${error.message}`, "bad");
      render();
    } finally {
      refreshing = null;
      refreshButton.disabled = busy;
      if (!busy) scheduleRefresh();
    }
  })();
  return refreshing;
}


function decimalMB(value) {
  return value == null ? "unavailable" : `${(value / 1000000).toFixed(2)} MB`;
}

function physicalPart(value, total) {
  return value == null || !total ? "unavailable" :
    `${decimalMB(value)} (${(value * 100 / total).toFixed(2)}% of allocated; ${(value * 100 / 1000000000).toFixed(2)}% of 1,000 MB limit)`;
}

function ageMs(value) { return value == null ? "unavailable" : age(Math.floor(value / 1000)); }

function usageState(value) {
  return ({normal: "Measured allocation below pressure threshold; cross-process guard unknown", pressure: "Pressure: cleanup accelerates at 700 MB",
    capture_paused: "Capture pause threshold reached at 800 MB", growth_stopped: "Application growth stop threshold reached at 900 MB",
    application_paused: "API process growth guard paused writes",
    stale: "Stale measurement", unavailable: "Measurement unavailable"})[value] || "Unknown";
}

function renderUsage(database) {
  const card = node("section", null, "card bg-base-100 shadow-xl");
  const content = node("div", null, "card-body");
  const title = database.kind === "core" ? "Core database" :
    database.policy?.kind === "dedicated" ? `Guild database · ${database.policy.guild_id}` : "Shared guild database";
  content.append(node("h3", title, "card-title"));
  line(content, "Database ID", database.id);
  line(content, "Role", database.role || "unknown");
  line(content, "Availability", database.availability || "unknown");
  line(content, "Allocated", decimalMB(database.usage?.allocated_bytes));
  line(content, "Pages", database.usage ? `${database.usage.page_count ?? "unknown"} × ${database.usage.page_size ?? "unknown"} bytes` : "unavailable");
  line(content, "Reusable free pages", database.usage?.freelist_count ?? "unavailable");
  line(content, "Limit used", database.percent_of_limit == null ? "unavailable" : `${database.percent_of_limit.toFixed(2)}% of 1,000 MB`);
  line(content, "Headroom", decimalMB(database.headroom_bytes));
  line(content, "Application thresholds", "Pressure 700 MB · capture pause 800 MB · growth stop 900 MB · hard limit 1,000 MB");
  if (database.kind === "guild") {
    line(content, "Snapshot budget", decimalMB(database.snapshot_budget_bytes));
    line(content, "Snapshot footprint estimate", database.snapshot_usage_estimate_bytes == null ? "unavailable" :
      `${decimalMB(database.snapshot_usage_estimate_bytes)} (${(database.snapshot_usage_estimate_bytes * 100 / database.snapshot_budget_bytes).toFixed(2)}% of 400 MB budget; conservative logical estimate)`);
  }
  line(content, "Operating state", usageState(database.status));
  line(content, "Measured", database.measurement?.sampled_at ? age(database.measurement.sampled_at) : "unavailable");
  line(content, "Last successful sample", database.measurement?.last_success_at ? age(database.measurement.last_success_at) : "unavailable");
  line(content, "Measurement source", database.measurement?.source || "unavailable");
  line(content, "Measurement capability", database.measurement?.capability || "unavailable");
  if (database.measurement?.error_code) line(content, "Probe status", database.measurement.error_code);
  line(content, "Provider reported bytes", decimalMB(database.provider_usage?.bytes));
  line(content, "API growth guard", database.application_admission?.api_process_growth_paused == null ? "unknown" :
    database.application_admission.api_process_growth_paused ? "paused" : "no local pause observed");
  line(content, "API startup", database.application_admission?.api_startup_deferred ? "deferred maintenance" : "normal or unknown");
  line(content, "Bot growth guard", database.application_admission?.bot_cross_process_guard || "unknown");
  line(content, "Capture", database.capture_status || "unavailable");
  if (database.kind === "guild") {
    const capture = database.capture || {};
    line(content, "Capture reason", capture.reason || "unknown");
    line(content, "Bot heartbeat age", capture.heartbeat_age_ms == null ? "unavailable" : `${Math.floor(capture.heartbeat_age_ms / 1000)} seconds`);
    line(content, "Retention maximum", capture.configured_retention_hours == null ? "unavailable" : `${capture.configured_retention_hours} hours`);
    line(content, "Oldest retained observation", ageMs(capture.oldest_available_snapshot_at_ms));
    line(content, "Capture epoch / counters reset", `${capture.epoch ?? "unknown"} / ${ageMs(capture.counters_reset_at_ms)}`);
    line(content, "Retained / lost / expired / evicted", `${capture.retained_count ?? "unknown"} / ${capture.lost_count ?? "unknown"} / ${capture.expired_count ?? "unknown"} / ${capture.evicted_count ?? "unknown"}`);
    line(content, "Verified effective snapshot allowance", decimalMB(database.policy_limits?.effective_snapshot_allowance_bytes));
  }
  line(content, "Cleanup", database.cleanup_status || "unavailable");
  const usage = database.usage;
  if (usage?.breakdown_error_code) line(content, "Breakdown status", usage.breakdown_error_code);
  if (usage?.breakdown_quality === "measured") {
    line(content, "Category basis", "Measured allocated pages; categories are disjoint");
    line(content, "Snapshot table", physicalPart(usage.categories.snapshot_data_bytes, usage.allocated_bytes));
    line(content, "Moderation and evidence tables", physicalPart(usage.categories.moderation_evidence_data_bytes, usage.allocated_bytes));
    line(content, "Configuration tables", physicalPart(usage.categories.configuration_data_bytes, usage.allocated_bytes));
    line(content, "Operational tables", physicalPart(usage.categories.operational_data_bytes, usage.allocated_bytes));
    line(content, "Indexes", physicalPart(usage.categories.index_bytes, usage.allocated_bytes));
    line(content, "Reusable pages", physicalPart(usage.categories.reusable_bytes, usage.allocated_bytes));
    line(content, "System / unattributed", physicalPart(usage.categories.system_unattributed_bytes, usage.allocated_bytes));
    line(content, "Category sample", age(usage.category_sampled_at));
  } else if (usage?.breakdown_quality === "estimated") {
    line(content, "Category basis", "Physical category allocation unavailable; snapshot value is encoded payload estimate");
    line(content, "Snapshot payload estimate", decimalMB(usage.snapshot_logical_bytes));
    line(content, "Reusable pages", decimalMB(usage.reusable_bytes));
    line(content, "Indexes and other allocation", "unavailable");
  } else if (usage) {
    line(content, "Category basis", "Physical category allocation unavailable");
    line(content, "Reusable pages", decimalMB(usage.reusable_bytes));
  }
  if (usage?.snapshot_count != null) line(content, "Retained snapshots", usage.snapshot_count);
  line(content, "External evidence file bytes", decimalMB(database.external_evidence_storage?.bytes));
  if (!usage || usage.breakdown_quality === "unavailable" && usage.snapshot_count == null) {
    line(content, "Breakdown", "unavailable");
  }
  const details = node("button", "Show current breakdown", "btn btn-sm");
  details.type = "button";
  details.addEventListener("click", async () => {
    details.disabled = true;
    try {
      let pending = detailRequests.get(database.id);
      if (!pending) {
        pending = request(`/api/database-usage/${encodeURIComponent(database.id)}`, {});
        detailRequests.set(database.id, pending);
        pending.finally(() => detailRequests.delete(database.id)).catch(() => {});
      }
      const updated = await pending;
      card.replaceWith(renderUsage(updated));
    } catch (error) { usageStatus.textContent = error.message; }
    finally { details.disabled = false; }
  });
  content.append(details);
  card.append(content);
  return card;
}

async function doRefreshUsage() {
  if (activeView !== "usage" || document.visibilityState === "hidden") return;
  try {
    const me = await request("/api/me", {});
    if (activeView !== "usage" || document.visibilityState === "hidden") return;
    if (!me.is_bot_owner || me.preview) {
      usageCards.replaceChildren();
      usageView.classList.add("hidden");
      usageStatus.textContent = "Only the configured bot owner can inspect database usage.";
      return;
    }
    const snapshot = await request("/api/database-usage", {});
    if (activeView !== "usage" || document.visibilityState === "hidden") return;
    signIn.classList.add("hidden");
    usageView.classList.remove("hidden");
    usageCards.replaceChildren(...snapshot.databases.map(renderUsage));
    usageStatus.textContent = `Usage response ${age(snapshot.generated_at)}. Catalog ${snapshot.catalog_status || "unknown"}. Unknown measurements are not treated as free capacity.`;
  } catch (error) {
    usageStatus.textContent = error.message;
    for (const card of usageCards.children) {
      if (card.dataset.usageStale === "1") continue;
      card.dataset.usageStale = "1";
      const note = node("p", "Stale: the latest usage refresh failed. These figures are not a current capacity sample.", "text-warning");
      card.prepend(note);
    }
  }
  finally { scheduleRefresh(); }
}

function refreshUsage() {
  if (!usageRefreshPromise) {
    usageRefreshPromise = doRefreshUsage().finally(() => { usageRefreshPromise = null; });
  }
  return usageRefreshPromise;
}

function selectView(view) {
  activeView = view;
  clearTimeout(refreshTimer);
  if (view === "usage") usageCards.replaceChildren();
  document.getElementById("admin-title").textContent = view === "usage" ? "Database Usage" : "Updates";
  document.getElementById("show-updates").setAttribute("aria-current", view === "updates" ? "page" : "false");
  document.getElementById("show-database-usage").setAttribute("aria-current", view === "usage" ? "page" : "false");
  updateView.classList.toggle("hidden", view !== "updates");
  usageView.classList.toggle("hidden", view !== "usage");
  return view === "usage" ? refreshUsage() : refresh();
}

refreshButton.addEventListener("click", () => {
  if (busy) return;
  actionMessage = "";
  return refresh();
});
document.getElementById("show-updates").addEventListener("click", () => selectView("updates"));
document.getElementById("show-database-usage").addEventListener("click", () => selectView("usage"));
document.getElementById("refresh-usage").addEventListener("click", () => refreshUsage());
document.addEventListener?.("visibilitychange", () => {
  if (document.visibilityState !== "hidden") {
    if (activeView === "usage") refreshUsage(); else refresh();
  }
});
initTheme();
refresh();
