import { createState, useStore } from "./state.js";
import { Appearance, useAppearance } from "./appearance.js";
import { TrendChart, TrendMetricPicker, sumSeries, useTrendMetric } from "./charts.js";
import { ACTION_ICONS, Icon, NAV_ICONS } from "./icons.js";
/* Owner Administration. React owns rendering; the API remains authoritative. */
import React, { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";

const h = React.createElement;
// Access, snapshot freshness and mutation activity are independent dimensions.
/** @typedef {"checking" | "allowed" | "denied"} OwnerAccess */
/** @typedef {"fresh" | "stale"} SnapshotFreshness */
const ui = createState({
  activeView: "updates",
  snapshot: null,
  usageSnapshot: null,
  usageFreshness: "fresh",
  usageRevision: 0,
  usageMessage: "Waiting to load usage…",
  analyticsSnapshot: null,
  analyticsFreshness: "fresh",
  analyticsRevision: 0,
  analyticsMessage: "Waiting to load analytics…",
  status: { message: "Checking your access and release state…", tone: "neutral" },
  activity: "idle",
  freshness: "stale",
  access: "checking",
  signIn: "optional",
  actionMessage: "",
  policyRevision: 0
});
const root = createRoot(document.getElementById("content"));

let refreshFlight = { kind: "idle" };
let usageFlight = { kind: "idle" };
let analyticsFlight = { kind: "idle" };
const detailRequests = new Map();

let rateLimitUntil = 0;
let refreshTimer;

function notice(message, tone = "neutral") { ui.set({ status: { message, tone } }); }
function restorePolicySelections() { ui.set({ policyRevision: ui.getSnapshot().policyRevision + 1 }); }
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
  if ((ui.getSnapshot().access === "denied") || document.visibilityState === "hidden") return;
  if (ui.getSnapshot().activeView === "usage") {
    refreshTimer = setTimeout(() => refreshUsage(), nextRefreshDelay(60000));
    return;
  }
  refreshTimer = setTimeout(() => refresh(), nextRefreshDelay(ui.getSnapshot().snapshot?.components.some(pending) ? 2000 : 30000));
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
    ui.set({ access: "denied", freshness: "stale" });
    clearTimeout(refreshTimer);
    ui.set({ signIn: "required" });

    throw new Error("Sign in again to manage updates.");
  }
  if (response.status === 403) {
    ui.set({ access: "denied", freshness: "stale" });
    clearTimeout(refreshTimer);
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
  if ((ui.getSnapshot().activity === "busy") || (ui.getSnapshot().freshness === "stale") || ui.getSnapshot().access !== "allowed") return;
  ui.set({ activity: "busy" });
  clearTimeout(refreshTimer);
  // Finish any older read before submitting; it must never overwrite the result.
  if (refreshFlight.kind === "running") await refreshFlight.promise;
  if (ui.getSnapshot().access !== "allowed" || (ui.getSnapshot().freshness === "stale")) { ui.set({ activity: "idle" }); restorePolicySelections(); scheduleRefresh(); return; }
  ui.set({ actionMessage: `${record.component.toUpperCase()}: ${label}…` });
  notice(ui.getSnapshot().actionMessage);

  try {
    const result = await callback();
    if (result.components) {
      ui.set({ snapshot: result, freshness: "fresh" });
    } else {
      await refresh();
    }
    // The queued acknowledgement is not progress. Say we are waiting only when
    // the snapshot just read still has that component's work pending.
    const current = ui.getSnapshot().snapshot?.components?.find((item) => item.component === record.component);
    const name = record.component.toUpperCase();
    ui.set({ actionMessage: result.queued
      ? current && pending(current)
        ? `${name}: update request accepted. Waiting for the component to report its result.`
        : `${name}: update request accepted.`
      : `${name}: ${label} completed.` });
    if (ui.getSnapshot().freshness !== "stale") notice(ui.getSnapshot().actionMessage, "good");
  } catch (error) {
    const message = `${record.component.toUpperCase()}: ${error.message}`;
    // A failed check may have persisted an error; show that state too.
    if ((ui.getSnapshot().access === "allowed")) await refresh();
    ui.set({ actionMessage: message });
    notice(message, "bad");
  } finally {
    ui.set({ activity: "idle" });
    restorePolicySelections();

    scheduleRefresh();
  }
}

function action(label, component, callback, props = {}) {
  return h("button", { type: "button", className: "btn btn-sm", ...props,
    onClick: () => mutate(ui.getSnapshot().snapshot.components.find(item => item.component === component), label,
      () => callback(ui.getSnapshot().snapshot.components.find(item => item.component === component))) }, label);
}

// Status badges and page controls draw the shared SVG icon set (frontend/icons.js), never glyphs.
function badge(text, tone, meaning) {
  const { icon, intent } = ACTION_ICONS[meaning];
  return h("span", { className: `badge badge-soft administration-badge badge-${{ good: "success", warn: "warning", bad: "error", neutral: "neutral" }[tone]}` },
    h(Icon, { name: icon, intent }), h("span", null, text));
}
function navButton(id, view, label, activeView) {
  return h("button", { id, className: "btn btn-sm", type: "button", "aria-current": activeView === view ? "page" : undefined, onClick: () => selectView(view) },
    h(Icon, { name: NAV_ICONS[view].icon, intent: NAV_ICONS[view].intent }), label);
}

function releaseText(release) {
  return release ? `${release.release_id} · ${release.source_sha} · ${release.published_at}` : "No checked candidate";
}

function ComponentCard({ record, revision }) {
  const [channel, setChannel] = useState(record.channel);
  const [cadence, setCadence] = useState(record.cadence);
  useEffect(() => { setChannel(record.channel); setCadence(record.cadence); }, [record.channel, record.cadence, revision]);
  const content = [], details = [], heading = [];
  const mismatch = ui.getSnapshot().snapshot.boot_mismatches?.[record.component];
  if (mismatch) {
    heading.push(badge("Boot mismatch", "warn", "warn"));
    line(content, "Startup problem", mismatch.reason);
    line(content, "Expected release", `${mismatch.expected.release_id} · ${mismatch.expected.source_sha}`);
    line(content, "Startup executable", mismatch.running || "Application stopped");
  }
  const fresh = record.observed_at && Date.now() / 1000 - record.observed_at < 7200 && record.state !== "drift";
  const observed = fresh ? record.observed : null;
  line(details, "Base image build SHA", observed?.base_image_sha || (record.component === "api" ? ui.getSnapshot().snapshot.base_image_sha : "unknown"));
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
  const compatibility = ui.getSnapshot().snapshot.compatibility?.[record.component];
  const launcher = ui.getSnapshot().snapshot.launchers?.[record.component];
  if (record.component !== "webui") {
    line(content, "Launcher requirement", candidateCurrent ? launcher?.reason || "Check with a launcher-aware API to determine this release's requirement." : "Check now to determine the release requirement.");
    if (launcher?.running != null) line(details, "Launcher capability", `${launcher.running} running; ${launcher.minimum} required; ${launcher.available} included in release`);
  }
  if (record.component !== "webui" && ui.getSnapshot().snapshot.coordinated_upgrade_path === "/api/service-upgrade/recovery") {
    const target = record.candidate;
    const changesSchema = target && observed && (target.result_schema !== observed.max_schema ||
    target.upgrade?.guild_schema !== observed.guild_schema);
    if (changesSchema) line(content, "Required action", "This release changes the database. Update bot and API together; both services will briefly stop.");
    if (ui.getSnapshot().snapshot.service_upgrade) line(content, "Paired upgrade", ui.getSnapshot().snapshot.service_upgrade.error || ui.getSnapshot().snapshot.service_upgrade.phase);
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
    // A direct image deploy records no managed install, so only the source SHA is confirmed.
    availability = "up to date";
    line(details, "Image digest", "unverified; no managed install is recorded, so only the running source SHA matches the available release");
    } else if (record.candidate && observed.source_sha !== record.candidate.source_sha) {
    availability = "update available";
    }
  }
  const inProgress = pending(record);
  const purgePending = record.component === "webui" && ui.getSnapshot().snapshot.webui_purge_pending;
  const problem = record.check_error || record.last_error || purgePending || record.state === "failed" || record.state === "drift";
  heading.push(badge((ui.getSnapshot().freshness === "stale") ? "Status unavailable" : inProgress ? "Update pending" : problem ? "Needs attention" : availability === "up to date" ? "Up to date" : availability === "update available" ? "Update available" : "Check status",
    (ui.getSnapshot().freshness === "stale") || problem ? "bad" : inProgress || availability === "update available" ? "warn" : availability === "up to date" ? "good" : "neutral",
    (ui.getSnapshot().freshness === "stale") || problem ? "attention" : inProgress ? "retry" : availability === "up to date" ? "approve" : "update"));
  line(content, "Availability", availability);
  if (record.desired_launcher && record.desired_launcher.digest !== record.installed_launcher?.digest) {
    line(content, "Launcher", "Update pending — waiting for the launcher to report its result");
  }
  line(content, "Update state", record.check_error ? `Check failed: ${record.check_error}` : record.last_error ? record.state === "drift" ? `Storage drift: ${record.last_error}` : `Install failed: ${record.last_error}` : record.state);
  if (purgePending) line(content, "CDN purge", `Pending: ${purgePending}. Check now retries the purge.`);
  line(details, "Launcher release", record.installed_launcher ? releaseText(record.installed_launcher) : "baked into base image");
  line(details, "Launcher SHA-256", record.launcher_sha256 || "unknown");
  const pairActive = ui.getSnapshot().snapshot.service_upgrade && !["complete", "aborted"].includes(ui.getSnapshot().snapshot.service_upgrade.phase);
  const canApply = !pairActive && ui.getSnapshot().snapshot.apply_enabled && candidateCurrent && !webuiRepacked && compatibility?.installable && (record.component === "webui" ? ui.getSnapshot().snapshot.webui_storage_configured : Boolean(observed?.self_update_enabled));
  const candidatePending = applicationPending(record) && record.desired?.digest === record.candidate?.digest;
  // WebUI keeps a same-digest publication available to repair Storage unless its card shows Up to date.
  // A service with a problem or a pending request for another digest stays updatable so the owner can retry or supersede it.
  const supersedable = applicationPending(record) && record.desired?.digest !== record.candidate?.digest;
  const applicationCurrent = record.component === "webui" ? availability === "up to date" && !problem
    : record.installed?.digest === record.candidate?.digest || (availability === "up to date" && !problem && !supersedable);
  const help = !ui.getSnapshot().snapshot.apply_enabled ? "On-demand installation is disabled for this deployment."
    : record.component === "webui" ? !ui.getSnapshot().snapshot.webui_storage_configured ? "Configure WebUI Storage to enable installation." : ""
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
        action("Check now", record.component, item => request("/api/updates/check", { component: item.component }), { disabled: (ui.getSnapshot().activity === "busy") || (ui.getSnapshot().freshness === "stale") }),
        action(record.component === "webui" ? "Update WebUI" : "Update", record.component,
          item => request("/api/updates/install", { component: item.component, digest: item.candidate.digest }),
          { hidden: !record.candidate, disabled: (ui.getSnapshot().activity === "busy") || (ui.getSnapshot().freshness === "stale") || !canApply || applicationCurrent || candidatePending }),
        record.component !== "webui" && h("details", { className: "administration-technical" },
          h("summary", null, "Advanced launcher controls"),
          h("p", null, "The launcher starts and recovers this service. Normal Update includes any required launcher update first. Use this control only for an optional launcher update. It restarts the service."),
          action("Update launcher", record.component,
            item => request("/api/updates/install", { component: item.component, digest: item.candidate.digest, launcher: true }),
            { hidden: !record.candidate, disabled: (ui.getSnapshot().activity === "busy") || (ui.getSnapshot().freshness === "stale") || pairActive || !ui.getSnapshot().snapshot.apply_enabled || !candidateCurrent || !launcher?.installable || record.desired_launcher?.digest === record.candidate?.digest || record.installed_launcher?.digest === record.candidate?.digest }))),
      h("fieldset", { className: "fieldset administration-policy", disabled: (ui.getSnapshot().activity === "busy") || (ui.getSnapshot().freshness === "stale") },
        h("legend", { className: "fieldset-legend" }, "Release policy"),
        h("label", { className: "fieldset field" }, h("span", { className: "fieldset-legend" }, "Release channel"),
          h("select", { className: "select select-sm", "aria-label": `${record.component} release channel`, value: channel,
            onChange: event => {
              const value = event.target.value; setChannel(value);
              return mutate(record, "Save release channel", () => {
                const item = ui.getSnapshot().snapshot.components.find(item => item.component === record.component);
                return request("/api/updates/policy", { component: item.component, channel: value, cadence: value === "latest" ? "off" : item.cadence });
              });
            } }, ...["latest", "stable", "nightly"].map(value => h("option", { key: value, value }, value === "latest" ? "Latest published build" : value === "stable" ? "Stable releases" : "Nightly releases")))),
        h("label", { className: "fieldset field" }, h("span", { className: "fieldset-legend" }, "Automatic installation"),
          h("select", { className: "select select-sm", "aria-label": `${record.component} automatic installation cadence`, value: cadence,
            onChange: event => {
              const value = event.target.value; setCadence(value);
              return mutate(record, "Save automatic installation", () => {
                const item = ui.getSnapshot().snapshot.components.find(item => item.component === record.component);
                return request("/api/updates/policy", { component: item.component, channel: item.channel, cadence: value });
              });
            } }, ...["off", "daily", "weekly", "monthly"].map(value => h("option", { key: value, value,
              disabled: value !== "off" && (!ui.getSnapshot().snapshot.unattended_enabled || record.channel === "latest") }, value === "off" ? "Automatic install off" : value)))))));
}

function AlertChannelCard({ revision }) {
  const saved = ui.getSnapshot().snapshot?.alert_channel || "";
  const [value, setValue] = useState(saved);
  useEffect(() => { setValue(saved); }, [saved, revision]);
  const blocked = (ui.getSnapshot().activity === "busy") || (ui.getSnapshot().freshness === "stale");
  return h("section", { id: "alert-channel", className: "card bg-base-100 administration-card" },
    h("div", { className: "card-body" },
      h("h2", { className: "card-title" }, "Boot repair alerts"),
      h("p", { className: "muted" }, "If boot repair cannot restore the recorded bot or API release, the bot posts the component, running version, expected release, and reason to this channel once per mismatch."),
      !saved && h("p", { id: "alert-channel-unset", className: "muted" }, "No alert channel is set. Mismatches are logged and shown on the component cards."),
      h("label", { className: "fieldset field" }, h("span", { className: "fieldset-legend" }, "Alert channel ID"),
        h("input", { id: "alert-channel-id", className: "input input-sm", inputMode: "numeric", value, disabled: blocked,
          onChange: event => setValue(event.target.value) })),
      h("div", { className: "card-actions" },
        h("button", { id: "save-alert-channel", type: "button", className: "btn btn-sm", disabled: blocked || value.trim() === saved,
          onClick: () => mutate({ component: "alerts" }, "Save alert channel",
            () => request("/api/updates/alert-channel", { channel_id: value.trim() })) }, "Save alert channel"))));
}

function refresh() {
  if (ui.getSnapshot().activeView !== "updates" || document.visibilityState === "hidden") return;
  if (refreshFlight.kind === "running") return refreshFlight.promise;
  clearTimeout(refreshTimer);

  refreshFlight = { kind: "running", promise: (async () => {
    try {
      ui.set({ snapshot: await request("/api/updates", {}) });
      if (ui.getSnapshot().activeView !== "updates" || document.visibilityState === "hidden") return;
      ui.set({ access: "allowed" });
      if (ui.getSnapshot().activity !== "busy") ui.set({ actionMessage: "" });
      ui.set({ freshness: "fresh", signIn: "optional" });
      notice(ui.getSnapshot().actionMessage || "Status refreshed. Release availability reflects the last successful release check.");
    } catch (error) {
      ui.set({ freshness: "stale" });
      notice(`Status could not be refreshed: ${error.message}`, "bad");
    } finally {
      refreshFlight = { kind: "idle" };

      if (ui.getSnapshot().activity !== "busy") scheduleRefresh();
    }
  })() };
  return refreshFlight.promise;
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
  return h("section", { className: "card bg-base-100", "data-usage-stale": oldSample ? "1" : undefined },
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
          if (revision === ui.getSnapshot().usageRevision && ui.getSnapshot().activeView === "usage" && (ui.getSnapshot().access === "allowed")) setDetail({ database: updated, revision, source: initial });
        } catch (error) {
          if (revision === ui.getSnapshot().usageRevision && ui.getSnapshot().activeView === "usage" && (ui.getSnapshot().access === "allowed")) { ui.set({ usageMessage: error.message }); }
        }
        finally { setLoading(false); }
      } }, "Show current breakdown")));
}

async function doRefreshUsage() {
  if (ui.getSnapshot().activeView !== "usage" || document.visibilityState === "hidden") return;
  try {
    const next = await request("/api/database-usage", {});
    if (ui.getSnapshot().activeView !== "usage" || document.visibilityState === "hidden") return;
    ui.set({ signIn: "optional", access: "allowed", usageSnapshot: next, usageFreshness: "fresh", usageMessage: `Usage response ${age(next.generated_at)}. Catalog ${next.catalog_status || "unknown"}. Unknown measurements are not treated as free capacity.` });
  } catch (error) { ui.set({ usageMessage: error.message }); ui.set({ usageFreshness: "stale" }); }
  finally { ui.set({ usageRevision: ui.getSnapshot().usageRevision + 1 }); scheduleRefresh(); }
}

function refreshUsage() {
  if (usageFlight.kind === "idle") {
    usageFlight = { kind: "running", promise: doRefreshUsage().finally(() => { usageFlight = { kind: "idle" }; }) };
  }
  return usageFlight.promise;
}

async function doRefreshAnalytics() {
  if (ui.getSnapshot().activeView !== "analytics" || document.visibilityState === "hidden") return;
  try {
    const next = await request("/api/platform/analytics", {});
    if (ui.getSnapshot().activeView !== "analytics" || document.visibilityState === "hidden") return;
    const members = next.members || {};
    const availability = (members.guilds_unavailable
      ? ` Member totals cover ${members.guilds_counted ?? 0} of ${members.servers ?? next.guilds?.length ?? 0} servers; the rest were unavailable.`
      : "") + (members.guilds_incomplete
      ? ` ${members.guilds_incomplete} counted ${members.guilds_incomplete === 1 ? "server has" : "servers have"} a member directory still importing or awaiting Reconcile.`
      : "");
    ui.set({ signIn: "optional", access: "allowed", analyticsSnapshot: next, analyticsFreshness: "fresh",
      analyticsMessage: `Analytics refreshed. ${members.status === "unavailable" ? "Member totals are unavailable." : `Member totals are ${members.status || "complete"}.`}${availability}` });
  } catch (error) { ui.set({ analyticsMessage: error.message }); ui.set({ analyticsFreshness: "stale" }); }
  finally { ui.set({ analyticsRevision: ui.getSnapshot().analyticsRevision + 1 }); }
}

function refreshAnalytics() {
  if (analyticsFlight.kind === "idle") {
    analyticsFlight = { kind: "running", promise: doRefreshAnalytics().finally(() => { analyticsFlight = { kind: "idle" }; }) };
  }
  return analyticsFlight.promise;
}

function platformStat(label, value) {
  return h("div", { className: "stat" },
    h("div", { className: "stat-title" }, label),
    h("div", { className: "stat-value" }, value));
}

const GUILD_COLUMNS = [
  ["warns", "warns", "Warnings"],
  ["bans", "bans", "Bans"],
  ["automod", "automod", "Spam hits"],
  ["modActions", "mod_actions", "Mod actions"],
  ["timeouts", "timeouts", "Timeouts"],
  ["joins", "joins", "Joins"],
  ["leaves", "leaves", "Leaves"],
];

function toTrendPoints(series) {
  return (series ?? []).map((point) => ({
    day: point.day,
    warns: point.warns,
    bans: point.bans,
    automod: point.automod,
    modActions: point.mod_actions,
    timeouts: point.timeouts,
    joins: point.joins,
    leaves: point.leaves,
  }));
}

function memberTotal(members) {
  if (!members || members.status === "unavailable") return "Unavailable";
  if (members.guilds_unavailable) return `${members.current ?? 0}+`;
  if (members.guilds_incomplete) return `${members.current ?? 0} (partial)`;
  return String(members.current ?? 0);
}

function AnalyticsCard({ snapshot }) {
  const [metric, setMetric] = useTrendMetric("warns");
  const points = toTrendPoints(snapshot?.series);
  const loading = !snapshot;
  const total = (key) => (loading ? "Loading…" : String(sumSeries(points, key)));
  const members = snapshot?.members;
  const stats = h("div", { className: "stats bg-base-100 w-full administration-stats" },
    platformStat("Servers", loading ? "Loading…" : String(snapshot?.servers ?? 0)),
    platformStat("Members now", loading ? "Loading…" : memberTotal(members)),
    platformStat("Known members", loading ? "Loading…" : members?.known != null ? String(members.known) : "Unavailable"),
    platformStat("Warnings", total("warns")),
    platformStat("Bans", total("bans")),
    platformStat("Spam hits", total("automod")),
    platformStat("Mod actions", total("modActions")),
    platformStat("Timeouts", total("timeouts")),
    platformStat("Joins", total("joins")),
    platformStat("Leaves", total("leaves")));
  const table = (snapshot?.guilds?.length ?? 0) > 0 && h("div", { className: "table-scroll rounded-box", tabIndex: 0 },
    h("table", { className: "table table-zebra table-sm" },
      h("thead", null, h("tr", null,
        h("th", { scope: "col" }, "Server"),
        ...GUILD_COLUMNS.map(([, , label]) => h("th", { key: label, scope: "col" }, label)))),
      h("tbody", null, snapshot.guilds.map((guild) =>
        h("tr", { key: guild.guild_id },
          h("td", null, guild.guild_id),
          ...GUILD_COLUMNS.map(([key, field]) => h("td", { key }, String(guild[field] ?? 0))))))));
  return h("section", { className: "card bg-base-100" },
    h("div", { className: "card-body" },
      h("h3", { className: "card-title" }, "Last 30 days, all servers"),
      stats,
      h(TrendMetricPicker, { metric, onChange: setMetric }),
      points.length
        ? h(TrendChart, { points, metric, title: "Platform activity" })
        : h("p", { className: "muted" }, loading ? "Loading analytics…" : "No platform trend data recorded yet."),
      table,
      h("p", { className: "muted" },
        "Per-server rows show guild IDs with recorded activity in the window, including servers that have since left until their data is erased; Servers counts current memberships. Days before the bot started recording — or while it was offline — show zero, not missing history. Members now is the current member directory across counted servers; known members includes departed members retained by ban history.")));
}

function selectView(view) {
  ui.set({ activeView: view });
  clearTimeout(refreshTimer);
  if (view === "usage") ui.set({ usageSnapshot: null });
  if (view === "analytics") ui.set({ analyticsSnapshot: null });

  if (view === "usage") return refreshUsage();
  if (view === "analytics") return refreshAnalytics();
  return refresh();
}

function Administration() {
  const appearance = useAppearance();
  const display = useStore(ui);
  useEffect(() => {
    const visible = () => {
      if (document.visibilityState !== "hidden") {
        if (ui.getSnapshot().activeView === "usage") refreshUsage();
        else if (ui.getSnapshot().activeView === "analytics") refreshAnalytics();
        else refresh();
      }
    };
    document.addEventListener("visibilitychange", visible);
    return () => { document.removeEventListener("visibilitychange", visible); clearTimeout(refreshTimer); };
  }, []);
  const loaded = window.JMODGUARD_WEBUI_SHA || "unknown (local or legacy asset)";
  const installedWeb = display.snapshot?.components.find(item => item.component === "webui")?.observed?.source_sha || "unknown or changed outside this updater";
  const differentWeb = /^[a-f0-9]{40}$/.test(loaded) && /^[a-f0-9]{40}$/.test(installedWeb) && loaded !== installedWeb;
  const details = [];
  line(details, "Loaded WebUI SHA", loaded); line(details, "Storage-observed SHA", installedWeb);
  const base = apiBase();
  const login = base ? `${base}/login?return_to=${encodeURIComponent(window.location.origin)}` : "/login?return_to=%2Fadministration%2Findex.html";
  return h(React.Fragment, null,
    h("header", { className: "administration-header" },
      h("div", null, h("span", { className: "eyebrow" }, "OWNER ADMINISTRATION"), h("h1", { id: "admin-title" }, display.activeView === "usage" ? "Database Usage" : display.activeView === "analytics" ? "Analytics" : "Updates")),
      h("div", { className: "administration-tools" }, h("button", { id: "refresh-status", type: "button", className: "btn btn-sm", disabled: (display.activity === "busy"),
        onClick: () => { if (ui.getSnapshot().activity !== "busy") { ui.set({ actionMessage: "" }); return refresh(); } } }, h(Icon, { name: ACTION_ICONS.retry.icon, intent: ACTION_ICONS.retry.intent }), "Refresh status"), h(Appearance, appearance), h("a", { className: "btn btn-sm", href: "/" }, h(Icon, { name: ACTION_ICONS.back.icon, intent: ACTION_ICONS.back.intent }), "Back to workspace"))),
    h("p", { id: "update-status", className: `alert alert-soft administration-notice ${{ good: "alert-success", warn: "alert-warning", bad: "alert-error", neutral: "" }[display.status.tone]}`, role: "status", "aria-live": "polite" }, display.status.message),
    h("div", { id: "sign-in", className: (display.signIn === "required") ? "" : "hidden" }, h("a", { id: "sign-in-link", className: "btn btn-primary", href: login }, "Sign in with Discord")),
    h("nav", { className: "administration-nav", "aria-label": "Administration pages" },
      navButton("show-updates", "updates", "Updates", display.activeView),
      navButton("show-database-usage", "usage", "Database Usage", display.activeView),
      navButton("show-analytics", "analytics", "Analytics", display.activeView)),
    h("div", { id: "updates", className: display.access === "allowed" && display.activeView === "updates" ? "" : "hidden" },
      h("div", { className: "administration-overview" }, h("div", null, h("h2", null, "Component updates"), h("p", { className: "muted" }, "Check releases and manage the API, bot, and WebUI independently. Release discovery runs hourly."))),
      h("p", { id: "automatic-state", className: "muted" }, display.snapshot?.unattended_enabled ? "Automatic installation policies are available." : "Automatic installation is disabled for this deployment. Hourly checks and Check now still work."),
      h("div", { id: "browser-version", className: "administration-browser" }, display.snapshot && h(React.Fragment, null,
        h("details", null, h("summary", null, "Browser version"), h("div", null, details)),
        differentWeb && badge("A different WebUI version is installed. Reload this page to load it.", "warn", "retry"),
        differentWeb && h("button", { className: "btn btn-sm", type: "button", disabled: (display.activity === "busy"), onClick: () => window.location.reload() }, "Reload page"))),
      h("div", { id: "update-cards", className: "administration-grid" }, display.snapshot?.components.map(record => h(ComponentCard, { key: record.component, record, revision: ui.getSnapshot().policyRevision }))),
      display.snapshot && h(AlertChannelCard, { revision: ui.getSnapshot().policyRevision })),
    h("section", { id: "database-usage", className: display.access === "allowed" && display.activeView === "usage" ? "" : "hidden", "aria-labelledby": "database-usage-title" },
      h("h2", { id: "database-usage-title" }, "Database Usage"), h("p", null, "Physical allocation includes reusable pages. Each database has its own 1,000 MB limit. Category estimates are labeled when exact page attribution is unavailable."),
      h("p", { id: "usage-status", className: `alert alert-soft administration-notice ${(display.usageFreshness === "stale") ? "alert-warning" : ""}`, role: "status" }, display.usageMessage),
      h("button", { id: "refresh-usage", className: "btn btn-sm", type: "button", onClick: () => refreshUsage() }, "Refresh usage"),
      h("div", { id: "usage-cards", className: "administration-grid" }, display.usageSnapshot?.databases.map(database => h(UsageCard, { key: database.id, database, stale: (ui.getSnapshot().usageFreshness === "stale"), revision: ui.getSnapshot().usageRevision })))),
    h("section", { id: "platform-analytics", className: display.access === "allowed" && display.activeView === "analytics" ? "" : "hidden", "aria-labelledby": "platform-analytics-title" },
      h("h2", { id: "platform-analytics-title" }, "Platform analytics"), h("p", null, "Fleet-wide moderation and membership trends over the last 30 days. Member totals come from each server's member directory; servers whose store is unavailable are reported as unknown instead of failing the view."),
      h("p", { id: "analytics-status", className: `alert alert-soft administration-notice ${(display.analyticsFreshness === "stale") ? "alert-warning" : ""}`, role: "status" }, display.analyticsMessage),
      h("button", { id: "refresh-analytics", className: "btn btn-sm", type: "button", onClick: () => refreshAnalytics() }, "Refresh analytics"),
      display.analyticsSnapshot && h(AnalyticsCard, { snapshot: display.analyticsSnapshot, revision: ui.getSnapshot().analyticsRevision })));
}

root.render(h(Administration));
refresh();
