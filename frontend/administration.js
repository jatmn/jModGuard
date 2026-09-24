/* Owner-only Administration page. The API enforces ownership and Origin for
 * every mutation; hidden controls do not provide authorization. */
const cards = document.getElementById("update-cards");
const statusNode = document.getElementById("update-status");
const updateView = document.getElementById("updates");
const signIn = document.getElementById("sign-in");
let snapshot = null;
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
  if (accessDenied) return;
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
    throw new Error("Sign in again to manage updates.");
  }
  if (response.status === 403) {
    authorized = false;
    accessDenied = true;
    stale = true;
    clearTimeout(refreshTimer);
    updateView.classList.add("hidden");
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
    actionMessage = result.queued
      ? `${record.component.toUpperCase()}: update request accepted. Waiting for the component to report its result.`
      : `${record.component.toUpperCase()}: ${label} completed.`;
    if (result.components) {
      snapshot = result;
      stale = false;
    } else {
      await refresh();
    }
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
    const record = view.record;
    return mutate(record, label, () => callback(record));
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
  view.install = action(record.component === "webui" ? "Update WebUI" : "Update application",
    (item) => request("/api/updates/install", { component: item.component, digest: item.candidate.digest }), view);
  row.append(view.check, view.install);
  if (record.component !== "webui") {
    view.launcher = action("Update launcher", (item) => request("/api/updates/install", { component: item.component, digest: item.candidate.digest, launcher: true }), view);
    row.append(view.launcher);
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
    const item = view.record;
    const value = view.channel.value;
    return mutate(item, "Save release channel", () => request("/api/updates/policy", { component: item.component, channel: value, cadence: value === "latest" ? "off" : item.cadence }));
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
    const item = view.record;
    const value = view.cadence.value;
    return mutate(item, "Save automatic installation", () => request("/api/updates/policy", { component: item.component, channel: item.channel, cadence: value }));
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
    const canApply = snapshot.apply_enabled && candidateCurrent && !webuiRepacked && compatibility?.installable && (record.component === "webui" ? snapshot.webui_storage_configured : Boolean(observed?.self_update_enabled));
    const candidatePending = applicationPending(record) && record.desired?.digest === record.candidate?.digest;
    const applicationCurrent = record.component !== "webui" && record.installed?.digest === record.candidate?.digest;
    view.check.disabled = busy || stale;
    view.install.hidden = !record.candidate;
    view.install.disabled = busy || stale || !canApply || applicationCurrent || candidatePending;
    if (view.launcher) {
      view.launcher.hidden = !record.candidate;
      view.launcher.disabled = busy || stale || !canApply || record.desired_launcher?.digest === record.candidate?.digest || record.installed_launcher?.digest === record.candidate?.digest;
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
  if (refreshing) return refreshing;
  clearTimeout(refreshTimer);
  refreshButton.disabled = busy;
  refreshing = (async () => {
    try {
      if (!authorized) {
        const me = await request("/api/me", {});
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

refreshButton.addEventListener("click", () => {
  if (busy) return;
  actionMessage = "";
  return refresh();
});
initTheme();
refresh();
