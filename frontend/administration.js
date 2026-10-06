/* Owner Administration. React owns rendering; the API remains authoritative. */
import React, { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";

const h = React.createElement;
const root = createRoot(document.getElementById("content"));
let activeView = "updates";
let snapshot = null;
let usageSnapshot = null;
let usageStale = false;
let usageRevision = 0;
let usageMessage = "Waiting to load usage…";
let status = { message: "Checking your access and release state…", tone: "neutral" };
let refreshing = null;
let usageRefreshPromise = null;
const detailRequests = new Map();
let busy = false;
let stale = true;
let authorized = false;
let accessDenied = false;
let needsSignIn = false;
let rateLimitUntil = 0;
let refreshTimer;
let actionMessage = "";
let policyRevision = 0;

function render() { root.render(h(Administration)); }
function notice(message, tone = "neutral") { status = { message, tone }; render(); }
function restorePolicySelections() { policyRevision++; }
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

function nextRefreshDelay(normal) {
  return Math.max(normal, Math.max(0, rateLimitUntil - Date.now()));
}

function scheduleRefresh() {
  clearTimeout(refreshTimer);
  if (accessDenied || document.visibilityState === "hidden") return;
  if (activeView === "usage") {
    refreshTimer = setTimeout(() => refreshUsage(), nextRefreshDelay(60000));
    return;
  }
  refreshTimer = setTimeout(() => refresh(), nextRefreshDelay(snapshot?.components.some(pending) ? 2000 : 30000));
}

function retryWaitMs(response) {
  const header = response.headers && typeof response.headers.get === "function"
    ? response.headers.get("retry-after") : null;
  const seconds = Number(header);
  if (!Number.isFinite(seconds) || seconds < 1) return 1000;
  return Math.min(seconds, 900) * 1000;
}

function apiBase() {
  return (window.JMODGUARD_API_BASE || "").replace(/\/$/, "");
}

async function request(path, body) {
  const response = await fetch(apiBase() + path, {
    method: body === undefined ? "GET" : "POST",
    credentials: "include", cache: "no-store",
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  if (response.status === 429) {
    rateLimitUntil = Date.now() + retryWaitMs(response);
    throw new Error(value.error || "Discord is temporarily rate limiting requests.");
  }
  if (response.status === 401 || (path === "/api/me" && response.status === 400) ||
      (response.status === 400 && ["not authenticated", "session expired"].includes(value.error))) {
    authorized = false;
    accessDenied = true;
    stale = true;
    clearTimeout(refreshTimer);
    needsSignIn = true;
    render();
    throw new Error("Sign in again to manage updates.");
  }
  if (response.status === 403) {
    authorized = false;
    accessDenied = true;
    stale = true;
    clearTimeout(refreshTimer);
    render();
  }
  if (!response.ok) throw new Error(value.error || `Request failed (${response.status})`);
  rateLimitUntil = 0;
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

function line(items, label, value) {
  items.push(h("p", { key: label, className: "administration-detail" },
    h("strong", null, label + ": "), h("span", null, String(value))));
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

function action(label, component, callback, props = {}) {
  return h("button", { type: "button", className: "btn btn-sm", ...props,
    onClick: () => mutate(snapshot.components.find(item => item.component === component), label,
      () => callback(snapshot.components.find(item => item.component === component))) }, label);
}

function badge(text, tone, icon) {
  return h("span", { className: `badge badge-soft administration-badge badge-${{ good: "success", warn: "warning", bad: "error", neutral: "neutral" }[tone]}` },
    h("span", { "aria-hidden": "true" }, icon), h("span", null, text));
}

function releaseText(release) {
  return release ? `${release.release_id} · ${release.source_sha} · ${release.published_at}` : "No checked candidate";
}

function ComponentCard({ record, revision }) {
  const [channel, setChannel] = useState(record.channel);
  const [cadence, setCadence] = useState(record.cadence);
  useEffect(() => { setChannel(record.channel); setCadence(record.cadence); }, [record.channel, record.cadence, revision]);
  const content = [], details = [], heading = [];
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
  if (record.component !== "webui" && snapshot.coordinated_upgrade_path === "/api/service-upgrade/recovery") {
    const target = record.candidate;
    const changesSchema = target && observed && (target.result_schema !== observed.max_schema ||
    target.upgrade?.guild_schema !== observed.guild_schema);
    if (changesSchema) line(content, "Required action", "This release changes the database. Update bot and API together; both services will briefly stop.");
    if (snapshot.service_upgrade) line(content, "Paired upgrade", snapshot.service_upgrade.error || snapshot.service_upgrade.phase);
    content.push(h("a", { key: "paired-upgrade", href: `${apiBase()}/api/service-upgrade/recovery` }, "Update bot and API together / recovery"));
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
  heading.push(badge(stale ? "Status unavailable" : inProgress ? "Update pending" : problem ? "Needs attention" : availability === "up to date" ? "Up to date" : availability === "update available" ? "Update available" : "Check status",
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
  const help = !snapshot.apply_enabled ? "On-demand installation is disabled for this deployment."
    : record.component === "webui" ? !snapshot.webui_storage_configured ? "Configure WebUI Storage to enable installation." : ""
    : !observed?.self_update_enabled ? "Self-update is not enabled or the running component could not be verified." : "";
  return h("section", { className: "card bg-base-100 administration-card" },
    h("div", { className: "card-body" },
      h("div", { className: "administration-card-heading" },
        h("div", null, h("h2", { className: "card-title" }, record.component.toUpperCase()),
          h("p", { className: "muted" }, { api: "Dashboard service", bot: "Discord moderation", webui: "Browser workspace" }[record.component])), ...heading),
      h("div", { className: "administration-info" }, content),
      h("details", { className: "administration-technical" }, h("summary", null, "Version & technical details"), h("div", null, details)),
      h("p", { className: "muted administration-help", hidden: !help }, help),
      h("div", { className: "card-actions administration-actions" },
        action("Check now", record.component, item => request("/api/updates/check", { component: item.component }), { disabled: busy || stale }),
        action(record.component === "webui" ? "Update WebUI" : "Update", record.component,
          item => request("/api/updates/install", { component: item.component, digest: item.candidate.digest }),
          { hidden: !record.candidate, disabled: busy || stale || !canApply || applicationCurrent || candidatePending }),
        record.component !== "webui" && h("details", { className: "administration-technical" },
          h("summary", null, "Advanced launcher controls"),
          h("p", null, "The launcher starts and recovers this service. Normal Update includes any required launcher update first. Use this control only for an optional launcher update. It restarts the service."),
          action("Update launcher", record.component,
            item => request("/api/updates/install", { component: item.component, digest: item.candidate.digest, launcher: true }),
            { hidden: !record.candidate, disabled: busy || stale || pairActive || !snapshot.apply_enabled || !candidateCurrent || !launcher?.installable || record.desired_launcher?.digest === record.candidate?.digest || record.installed_launcher?.digest === record.candidate?.digest }))),
      h("fieldset", { className: "administration-policy", disabled: busy || stale },
        h("legend", null, "Release policy"),
        h("label", { className: "field" }, "Release channel",
          h("select", { className: "select select-bordered select-sm", "aria-label": `${record.component} release channel`, value: channel,
            onChange: event => {
              const value = event.target.value; setChannel(value);
              return mutate(record, "Save release channel", () => {
                const item = snapshot.components.find(item => item.component === record.component);
                return request("/api/updates/policy", { component: item.component, channel: value, cadence: value === "latest" ? "off" : item.cadence });
              });
            } }, ...["latest", "stable", "nightly"].map(value => h("option", { key: value, value }, value === "latest" ? "Latest published build" : value === "stable" ? "Stable releases" : "Nightly releases")))),
        h("label", { className: "field" }, "Automatic installation",
          h("select", { className: "select select-bordered select-sm", "aria-label": `${record.component} automatic installation cadence`, value: cadence,
            onChange: event => {
              const value = event.target.value; setCadence(value);
              return mutate(record, "Save automatic installation", () => {
                const item = snapshot.components.find(item => item.component === record.component);
                return request("/api/updates/policy", { component: item.component, channel: item.channel, cadence: value });
              });
            } }, ...["off", "daily", "weekly", "monthly"].map(value => h("option", { key: value, value,
              disabled: value !== "off" && (!snapshot.unattended_enabled || record.channel === "latest") }, value === "off" ? "Automatic install off" : value)))))));
}

function refresh() {
  if (activeView !== "updates" || document.visibilityState === "hidden") return;
  if (refreshing) return refreshing;
  clearTimeout(refreshTimer);
  render();
  refreshing = (async () => {
    try {
      snapshot = await request("/api/updates", {});
      if (activeView !== "updates" || document.visibilityState === "hidden") return;
      authorized = true;
      accessDenied = false;
      if (!busy) actionMessage = "";
      stale = false;
      needsSignIn = false;
      notice(actionMessage || "Status refreshed. Release availability reflects the last successful release check.");
      render();
    } catch (error) {
      stale = true;
      notice(`Status could not be refreshed: ${error.message}`, "bad");
      render();
    } finally {
      refreshing = null;
      render();
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

function UsageCard({ database: initial, stale, revision }) {
  const [detail, setDetail] = useState(null);
  const [loading, setLoading] = useState(false);
  const database = detail?.source === initial ? detail.database : initial;
  const oldSample = stale && detail?.revision !== revision;
  const content = [];
  const title = database.kind === "core" ? "Core database" :
    database.policy?.kind === "dedicated" ? `Guild database · ${database.policy.guild_id}` : "Shared guild database";
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
  return h("section", { className: "card bg-base-100 shadow-xl", "data-usage-stale": oldSample ? "1" : undefined },
    oldSample && h("p", { className: "text-warning" }, "Stale: the latest usage refresh failed. These figures are not a current capacity sample."),
    h("div", { className: "card-body" }, h("h3", { className: "card-title" }, title), content,
      h("button", { type: "button", className: "btn btn-sm", disabled: loading, onClick: async () => {
        setLoading(true);
        try {
          let pending = detailRequests.get(database.id);
          if (!pending) {
            pending = request(`/api/database-usage/${encodeURIComponent(database.id)}`, {});
            detailRequests.set(database.id, pending);
            pending.finally(() => detailRequests.delete(database.id)).catch(() => {});
          }
          const updated = await pending;
          if (revision === usageRevision && activeView === "usage" && authorized) setDetail({ database: updated, revision, source: initial });
        } catch (error) {
          if (revision === usageRevision && activeView === "usage" && authorized) { usageMessage = error.message; render(); }
        }
        finally { setLoading(false); }
      } }, "Show current breakdown")));
}

async function doRefreshUsage() {
  if (activeView !== "usage" || document.visibilityState === "hidden") return;
  try {
    const next = await request("/api/database-usage", {});
    if (activeView !== "usage" || document.visibilityState === "hidden") return;
    needsSignIn = false;
    authorized = true;
    accessDenied = false;
    usageSnapshot = next;
    usageStale = false;
    usageMessage = `Usage response ${age(next.generated_at)}. Catalog ${next.catalog_status || "unknown"}. Unknown measurements are not treated as free capacity.`;
  } catch (error) { usageMessage = error.message; usageStale = true; }
  finally { usageRevision++; render(); scheduleRefresh(); }
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
  if (view === "usage") usageSnapshot = null;
  render();
  return view === "usage" ? refreshUsage() : refresh();
}

function Appearance() {
  const [preference, setPreference] = useState(() => {
    try { return window.localStorage.getItem("jmodguard.theme") || "system"; } catch (_) { return "system"; }
  });
  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => {
      const dark = preference === "dark" || (preference !== "light" && media.matches);
      document.documentElement.dataset.theme = dark ? "dark" : "light";
      document.querySelector('meta[name="theme-color"]').content = dark ? "#101e33" : "#e0eeff";
    };
    apply(); media.addEventListener("change", apply);
    return () => media.removeEventListener("change", apply);
  }, [preference]);
  return h("label", { className: "theme-picker" }, "Appearance ",
    h("select", { id: "appearance", className: "select select-bordered select-sm", "aria-label": "Appearance", value: preference,
      onChange: event => { const value = event.target.value; setPreference(value); try { window.localStorage.setItem("jmodguard.theme", value); } catch (_) {} } },
      ...["system", "light", "dark"].map(value => h("option", { key: value, value }, value[0].toUpperCase() + value.slice(1)))));
}

function Administration() {
  useEffect(() => {
    const visible = () => { if (document.visibilityState !== "hidden") { if (activeView === "usage") refreshUsage(); else refresh(); } };
    document.addEventListener("visibilitychange", visible);
    return () => { document.removeEventListener("visibilitychange", visible); clearTimeout(refreshTimer); };
  }, []);
  const loaded = window.JMODGUARD_WEBUI_SHA || "unknown (local or legacy asset)";
  const installedWeb = snapshot?.components.find(item => item.component === "webui")?.observed?.source_sha || "unknown or changed outside this updater";
  const differentWeb = /^[a-f0-9]{40}$/.test(loaded) && /^[a-f0-9]{40}$/.test(installedWeb) && loaded !== installedWeb;
  const details = [];
  line(details, "Loaded WebUI SHA", loaded); line(details, "Storage-observed SHA", installedWeb);
  const base = apiBase();
  const login = base ? `${base}/login?return_to=${encodeURIComponent(window.location.origin)}` : "/login?return_to=%2Fadministration%2Findex.html";
  return h(React.Fragment, null,
    h("header", { className: "administration-header" },
      h("div", null, h("span", { className: "eyebrow" }, "OWNER ADMINISTRATION"), h("h1", { id: "admin-title" }, activeView === "usage" ? "Database Usage" : "Updates")),
      h("div", { className: "administration-tools" }, h("button", { id: "refresh-status", type: "button", className: "btn btn-sm", disabled: busy,
        onClick: () => { if (!busy) { actionMessage = ""; return refresh(); } } }, "↻ Refresh status"), h(Appearance), h("a", { className: "btn btn-sm", href: "/" }, "← Back to workspace"))),
    h("p", { id: "update-status", className: `administration-notice administration-${status.tone}`, role: "status", "aria-live": "polite" }, status.message),
    h("div", { id: "sign-in", className: needsSignIn ? "" : "hidden" }, h("a", { id: "sign-in-link", className: "btn btn-primary", href: login }, "Sign in with Discord")),
    h("nav", { className: "administration-nav", "aria-label": "Administration pages" },
      h("button", { id: "show-updates", className: "btn btn-sm", type: "button", "aria-current": activeView === "updates" ? "page" : undefined, onClick: () => selectView("updates") }, "Updates"),
      h("button", { id: "show-database-usage", className: "btn btn-sm", type: "button", "aria-current": activeView === "usage" ? "page" : undefined, onClick: () => selectView("usage") }, "Database Usage")),
    h("div", { id: "updates", className: authorized && !accessDenied && activeView === "updates" ? "" : "hidden" },
      h("div", { className: "administration-overview" }, h("div", null, h("h2", null, "Component updates"), h("p", { className: "muted" }, "Check releases and manage the API, bot, and WebUI independently. Release discovery runs hourly."))),
      h("p", { id: "automatic-state", className: "muted" }, snapshot?.unattended_enabled ? "Automatic installation policies are available." : "Automatic installation is disabled for this deployment. Hourly checks and Check now still work."),
      h("div", { id: "browser-version", className: "administration-browser" }, snapshot && h(React.Fragment, null,
        h("details", null, h("summary", null, "Browser version"), h("div", null, details)),
        differentWeb && badge("A different WebUI version is installed. Reload this page to load it.", "warn", "↻"),
        differentWeb && h("button", { className: "btn btn-sm", type: "button", disabled: busy, onClick: () => window.location.reload() }, "Reload page"))),
      h("div", { id: "update-cards", className: "administration-grid" }, snapshot?.components.map(record => h(ComponentCard, { key: record.component, record, revision: policyRevision })))),
    h("section", { id: "database-usage", className: authorized && !accessDenied && activeView === "usage" ? "" : "hidden", "aria-labelledby": "database-usage-title" },
      h("h2", { id: "database-usage-title" }, "Database Usage"), h("p", null, "Physical allocation includes reusable pages. Each database has its own 1,000 MB limit. Category estimates are labeled when exact page attribution is unavailable."),
      h("p", { id: "usage-status", className: "administration-notice", role: "status" }, usageMessage),
      h("button", { id: "refresh-usage", className: "btn btn-sm", type: "button", onClick: () => refreshUsage() }, "Refresh usage"),
      h("div", { id: "usage-cards", className: "administration-grid" }, usageSnapshot?.databases.map(database => h(UsageCard, { key: database.id, database, stale: usageStale, revision: usageRevision })))));
}

render();
refresh();
