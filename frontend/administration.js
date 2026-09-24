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
    signIn.classList.remove("hidden");
    updateView.classList.add("hidden");
    usageView.classList.add("hidden");
    throw new Error("Sign in again to use Administration.");
  }
  if (!response.ok) throw new Error(value.error || `Request failed (${response.status})`);
  return value;
}

function age(timestamp) {
  if (!timestamp) return "unknown";
  const seconds = Math.max(0, Math.floor(Date.now() / 1000) - timestamp);
  if (seconds < 3600) return `${Math.floor(seconds / 60)} minutes ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} hours ago`;
  return `${Math.floor(seconds / 86400)} days ago`;
}

function line(parent, label, value) {
  const item = node("p");
  const strong = node("strong", `${label}: `);
  item.append(strong, node("span", String(value)));
  parent.append(item);
}

function action(label, callback, disabled) {
  const button = node("button", label, "btn btn-sm");
  button.type = "button";
  button.disabled = disabled;
  button.addEventListener("click", async () => {
    button.disabled = true;
    try { await callback(); await refresh(); }
    catch (error) { statusNode.textContent = error.message; }
    finally { button.disabled = disabled; }
  });
  return button;
}

function releaseText(release) {
  return release ? `${release.release_id} · ${release.source_sha} · ${release.published_at}` : "No checked candidate";
}

function render() {
  cards.replaceChildren();
  const loaded = window.JMODGUARD_WEBUI_SHA || "unknown (local or legacy asset)";
  const installedWeb = snapshot.components.find((item) => item.component === "webui")?.observed?.source_sha || "unknown or changed outside this updater";
  document.getElementById("browser-version").textContent = `This tab loaded WebUI SHA ${loaded}. Storage-observed SHA ${installedWeb}.`;
  document.getElementById("automatic-state").textContent = snapshot.unattended_enabled
    ? "Automatic installation policies are available."
    : "Automatic installation is disabled for this deployment. Hourly checks and Check now still work.";
  for (const record of snapshot.components) {
    const card = node("section", null, "card bg-base-100 shadow-xl");
    const content = node("div", null, "card-body");
    content.append(node("h2", record.component.toUpperCase(), "card-title"));
    const fresh = record.observed_at && Date.now() / 1000 - record.observed_at < 7200 && record.state !== "drift";
    const observed = fresh ? record.observed : null;
    line(content, "Base image build SHA", observed?.base_image_sha || (record.component === "api" ? snapshot.base_image_sha : "unknown"));
    line(content, record.component === "webui" ? "Storage-observed SHA" : "Running executable SHA", observed?.source_sha || "unknown");
    line(content, "Verified running file SHA-256", observed?.executable_sha256 || "unknown");
    line(content, "Running since", age(observed?.started_at));
    line(content, "Managed release installed", age(record.installed_at));
    const checked = record.checked_at && Date.now() / 1000 - record.checked_at < 7200 && !record.check_error;
    const candidateCurrent = checked && record.candidate?.channel === record.channel;
    const webuiRepacked = record.component === "webui" && [record.installed, record.previous, record.desired].some((release) => release?.source_sha === record.candidate?.source_sha && release.digest !== record.candidate?.digest);
    line(content, "Available release", candidateCurrent ? releaseText(record.candidate) : "Check now to confirm this channel");
    if (webuiRepacked) line(content, "Release warning", "This WebUI source SHA has a different image digest; file bytes may differ, so publish a new source commit");
    const compatibility = snapshot.compatibility?.[record.component];
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
    line(content, "Availability", availability);
    line(content, "Update state", record.check_error ? `Check failed: ${record.check_error}` : record.last_error ? record.state === "drift" ? `Storage drift: ${record.last_error}` : `Install failed: ${record.last_error}` : record.state);
    line(content, "Launcher release", record.installed_launcher ? releaseText(record.installed_launcher) : "baked into base image");
    line(content, "Launcher SHA-256", record.launcher_sha256 || "unknown");
    const row = node("div", null, "card-actions");
    row.append(action("Check now", () => request("/api/updates/check", { component: record.component }), false));
    const canApply = snapshot.apply_enabled && candidateCurrent && !webuiRepacked && compatibility?.installable && (record.component === "webui" ? snapshot.webui_storage_configured : Boolean(observed?.self_update_enabled));
    if (record.candidate) {
      const applicationCurrent = record.component !== "webui" && record.installed?.digest === record.candidate.digest;
      row.append(action(record.component === "webui" ? "Update WebUI" : "Update application", () => request("/api/updates/install", { component: record.component, digest: record.candidate.digest }), !canApply || applicationCurrent));
      if (record.component !== "webui") {
        const launcherCurrent = record.installed_launcher?.digest === record.candidate.digest;
        row.append(action("Update launcher", () => request("/api/updates/install", { component: record.component, digest: record.candidate.digest, launcher: true }), !canApply || launcherCurrent));
      }
    }
    content.append(row);
    const channel = node("select");
    channel.setAttribute("aria-label", `${record.component} release channel`);
    for (const value of ["latest", "stable", "nightly"]) {
      const option = node("option", value === "latest" ? "Latest published build" : value === "stable" ? "Stable releases" : "Nightly releases");
      option.value = value;
      channel.append(option);
    }
    channel.value = record.channel;
    channel.addEventListener("change", async () => {
      try { await request("/api/updates/policy", { component: record.component, channel: channel.value, cadence: record.cadence }); await refresh(); }
      catch (error) { statusNode.textContent = error.message; channel.value = record.channel; }
    });
    content.append(channel);
    const cadence = node("select");
    cadence.setAttribute("aria-label", `${record.component} automatic installation cadence`);
    for (const value of ["off", "daily", "weekly", "monthly"]) {
      const option = node("option", value === "off" ? "Automatic install off" : value);
      option.value = value;
      option.disabled = value !== "off" && (!snapshot.unattended_enabled || record.channel === "latest");
      cadence.append(option);
    }
    cadence.value = record.cadence;
    cadence.addEventListener("change", async () => {
      try { await request("/api/updates/policy", { component: record.component, channel: record.channel, cadence: cadence.value }); await refresh(); }
      catch (error) { statusNode.textContent = error.message; cadence.value = record.cadence; }
    });
    content.append(cadence);
    card.append(content);
    cards.append(card);
  }
}

async function refresh() {
  if (activeView !== "updates" || document.visibilityState === "hidden") return;
  try {
    const me = await request("/api/me", {});
    if (activeView !== "updates" || document.visibilityState === "hidden") return;
    if (!me.is_bot_owner) {
      updateView.classList.add("hidden");
      statusNode.textContent = "Only the configured bot owner can manage updates.";
      return;
    }
    snapshot = await request("/api/updates", {});
    if (activeView !== "updates") return;
    signIn.classList.add("hidden");
    updateView.classList.remove("hidden");
    statusNode.textContent = "Release state is current as of the last successful check.";
    render();
  } catch (error) {
    statusNode.textContent = error.message;
  }
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
    if (activeView !== "usage") return;
    signIn.classList.add("hidden");
    usageView.classList.remove("hidden");
    usageCards.replaceChildren(...snapshot.databases.map(renderUsage));
    usageStatus.textContent = `Usage response ${age(snapshot.generated_at)}. Catalog ${snapshot.catalog_status || "unknown"}. Unknown measurements are not treated as free capacity.`;
  } catch (error) { usageStatus.textContent = error.message; }
}

function refreshUsage() {
  if (!usageRefreshPromise) {
    usageRefreshPromise = doRefreshUsage().finally(() => { usageRefreshPromise = null; });
  }
  return usageRefreshPromise;
}

function selectView(view) {
  activeView = view;
  if (view === "usage") usageCards.replaceChildren();
  document.getElementById("admin-title").textContent = view === "usage" ? "Database Usage" : "Updates";
  document.getElementById("show-updates").setAttribute("aria-current", view === "updates" ? "page" : "false");
  document.getElementById("show-database-usage").setAttribute("aria-current", view === "usage" ? "page" : "false");
  updateView.classList.toggle("hidden", view !== "updates");
  usageView.classList.toggle("hidden", view !== "usage");
  return view === "usage" ? refreshUsage() : refresh();
}

document.getElementById("show-updates").addEventListener("click", () => selectView("updates"));
document.getElementById("show-database-usage").addEventListener("click", () => selectView("usage"));
document.getElementById("refresh-usage").addEventListener("click", () => refreshUsage());
document.addEventListener?.("visibilitychange", () => {
  if (document.visibilityState !== "hidden") {
    if (activeView === "usage") refreshUsage(); else refresh();
  }
});

refresh();
setInterval(() => {
  if (document.visibilityState === "hidden") return;
  if (activeView === "usage") refreshUsage(); else refresh();
}, 60000);
