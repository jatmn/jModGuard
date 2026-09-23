/* Owner-only Administration page. The API enforces ownership and Origin for
 * every mutation; hidden controls do not provide authorization. */
const cards = document.getElementById("update-cards");
const statusNode = document.getElementById("update-status");
const updateView = document.getElementById("updates");
const signIn = document.getElementById("sign-in");
let snapshot = null;

function node(tag, text, className) {
  const element = document.createElement(tag);
  if (text != null) element.textContent = text;
  if (className) element.className = className;
  return element;
}

async function request(path, body) {
  const response = await fetch(path, {
    method: body === undefined ? "GET" : "POST",
    credentials: "same-origin", cache: "no-store",
    headers: body === undefined ? {} : { "Content-Type": "application/json" },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json().catch(() => ({}));
  if (response.status === 401 || (path === "/api/me" && response.status === 400) ||
      (response.status === 400 && ["not authenticated", "session expired"].includes(value.error))) {
    signIn.classList.remove("hidden");
    updateView.classList.add("hidden");
    throw new Error("Sign in again to manage updates.");
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
    const webuiRepacked = record.component === "webui" && record.installed?.source_sha === record.candidate?.source_sha && record.installed?.digest !== record.candidate?.digest;
    line(content, "Available release", candidateCurrent ? releaseText(record.candidate) : "Check now to confirm this channel");
    if (webuiRepacked) line(content, "Release warning", "This WebUI source SHA has different bytes; publish a new source commit");
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
    line(content, "Update state", record.check_error ? `Check failed: ${record.check_error}` : record.last_error ? `Install failed: ${record.last_error}` : record.state);
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
  try {
    const me = await request("/api/me", {});
    if (!me.is_bot_owner) {
      updateView.classList.add("hidden");
      statusNode.textContent = "Only the configured bot owner can manage updates.";
      return;
    }
    snapshot = await request("/api/updates", {});
    signIn.classList.add("hidden");
    updateView.classList.remove("hidden");
    statusNode.textContent = "Release state is current as of the last successful check.";
    render();
  } catch (error) {
    statusNode.textContent = error.message;
  }
}

refresh();
setInterval(refresh, 30000);
