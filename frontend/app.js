import { createState, useStore } from "./state.js";
import { Appearance, useAppearance } from "./appearance.js";
import {
  ActivityHeatmap,
  CapacityGauge,
  ENFORCEMENT_METRICS,
  EnforcementChart,
  KpiTile,
  MembershipChart,
  RangeSelector,
  RankList,
  ShareBar,
  formatCount,
  formatDelta,
  formatExact,
  sumSeries,
} from "./charts.js";
import { ACTION_ICONS, DASHBOARD_ICONS, Icon, NAV_ICONS } from "./icons.js";
import React, { useState, useEffect, useRef, createContext, useContext } from "react";
import { flushSync } from "react-dom";
import { createRoot } from "react-dom/client";
import {
  Dialog,
  DialogPanel,
  Field,
  Label,
  HeadlessInput,
  HeadlessSelect,
  HeadlessTextarea,
} from "../tools/webui/components.js";

const h = React.createElement;
// Activity and edits are independent: an in-flight save may retain other drafts.
/** @typedef {"idle" | "busy"} EditorActivity */
/** @typedef {"clean" | "dirty"} EditorEdits */
const BusyContext = createContext(false);
const ui = createState({
  view: "startup-view",
  startupStatus: "Waiting for the API to finish starting…",
  previewAvailability: "unchecked",
  contentRevision: 0,
  editor: null,
  notification: null,
  activity: "idle",
  edits: "clean",
  copySource: null,
  drafts: {},
  // Saved values that replace a draft control's default; cleared with drafts.
  draftBaselines: {},
  pendingDrafts: null
});
const root = createRoot(document.getElementById("app"));

let toastTimer;

// The shared busy policy belongs to the application; React owns the controls.
const DraftGroup = createContext(null);
function DraftCard({ group, children, ...props }) {
  return h(DraftGroup.Provider, { value: group }, h("div", props, children));
}
function ActionButton({ onClick, savedFields, disabled, ...props }) {
  const working = useContext(BusyContext);
  const group = useContext(DraftGroup);
  return h("button", {
    ...props,
    disabled: working || disabled,
    onClick: onClick && ((event) => handleEvent(onClick, event, savedFields, group)),
  });
}

// Row and record actions are compact icon buttons (#582, #588): the shared icon set in
// frontend/icons.js gives each action one icon and one fixed color, sized by the .btn-icon
// rules in frontend/style.css. The accessible name and hover tooltip come from the label,
// and ActionButton keeps the shared busy/disabled path.
function IconButton({ action, label, className = "btn btn-sm btn-square", ...props }) {
  const { icon, intent } = ACTION_ICONS[action];
  return h(
    ActionButton,
    { className: `${className} btn-icon`, "aria-label": label, title: label, ...props },
    h(Icon, { name: icon, intent }),
  );
}
function useDraftControl({ draftKey, defaultValue, onChange, ...props }) {
  const group = useContext(DraftGroup);
  const working = useContext(BusyContext);
  const { drafts, draftBaselines } = useStore(ui);
  if (!draftKey) return { ...props, defaultValue, onChange, disabled: working || props.disabled };
  const baseline = String(draftBaselines[draftKey] ?? defaultValue ?? "");
  return {
    ...props,
    value: drafts[draftKey]?.value ?? baseline,
    disabled: working || props.disabled,
    onChange: event => {
      const value = event.target.value;
      const next = { ...ui.getSnapshot().drafts };
      if (value === baseline) delete next[draftKey];
      else next[draftKey] = { value, group };
      ui.set({ drafts: next, edits: Object.keys(next).length > 0 ? "dirty" : "clean" });
      onChange?.(event);
    },
  };
}
function Input(props) { return h(HeadlessInput, useDraftControl(props)); }
function Textarea(props) { return h(HeadlessTextarea, useDraftControl(props)); }
function Select(props) { return h(HeadlessSelect, useDraftControl(props)); }
function OwnerTools() {
  const profile = useRef(null);
  if (isPreview())
    return h(
      "div",
      { id: "owner-tools", className: "owner-tools" },
      h(
        ActionButton,
        {
          className: "btn btn-sm",
          onClick: async () => {
            try {
              await api("/api/preview", "DELETE");
            } catch (_) {}
            clearPreviewToken();
            window.location = "/";
          },
        },
        h(Icon, { name: NAV_ICONS["exit-preview"].icon, intent: NAV_ICONS["exit-preview"].intent, className: "nav-icon" }),
        "Exit preview",
      ),
    );
  if (!isBotOwner()) return null;
  return h(
    "div",
    { id: "owner-tools", className: "owner-tools" },
    h("a", { className: "btn btn-sm", href: "/administration/index.html" }, h(Icon, { name: NAV_ICONS["administration"].icon, intent: NAV_ICONS["administration"].intent, className: "nav-icon" }), "Administration"),
    h(
      ActionButton,
      {
        className: "btn btn-sm",
        onClick: async () => {
          if (!state.guildId) return toast("Select a server first", "error");
          const launched = await api("/api/preview", "POST", {
            guild_id: String(state.guildId),
            profile: profile.current.value,
          });
          window.open(
            `${location.pathname}#preview=${encodeURIComponent(launched.token)}`,
            "_blank",
            "noopener",
          );
        },
      },
      h(Icon, { name: NAV_ICONS["view-as"].icon, intent: NAV_ICONS["view-as"].intent, className: "nav-icon" }),
      "View as…",
    ),
    h(
      Select,
      { ref: profile, className: "select", "aria-label": "Preview profile" },
      ...["administrator", "moderator", "member"].map((value) =>
        h("option", { key: value, value }, value[0].toUpperCase() + value.slice(1)),
      ),
    ),
  );
}
function directoryTime(joinedAtNs) {
  if (joinedAtNs === null || joinedAtNs === undefined || joinedAtNs === "") return "Unknown";
  const date = new Date(Number(joinedAtNs) / 1e6);
  return Number.isNaN(date.getTime()) ? "Unknown" : date.toLocaleString(undefined, { timeZoneName: "short" });
}

function serverIconUrl(server) {
  return server.icon_hash
    ? `https://cdn.discordapp.com/icons/${server.guild_id}/${server.icon_hash}.png`
    : null;
}

// A server's Discord CDN icon (the icon hash /api/me already returns), or a
// letter avatar when it has none or the image fails to load.
function ServerIcon({ guild }) {
  const url = serverIconUrl({ guild_id: guild.id, icon_hash: guild.icon });
  const [failed, setFailed] = useState(null);
  if (url && failed !== url)
    return h("img", { className: "server-icon", src: url, alt: "", width: 20, height: 20, onError: () => setFailed(url) });
  const initial = Array.from(String(guild.name || "").trim())[0] || "?";
  return h("span", { className: "server-icon server-icon-letter", "aria-hidden": "true" }, initial.toUpperCase());
}

// Native <option> elements cannot show images, so the sidebar switcher is a
// small listbox: focus moves to the open list, which tracks the active option
// with aria-activedescendant. It makes no requests; choosing a different server
// runs the same guarded switch as before.
function ServerPicker({ guilds, selectedId, onSelect, children }) {
  const working = useContext(BusyContext);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const pickerRef = useRef(null);
  const triggerRef = useRef(null);
  const listRef = useRef(null);
  const selectedIndex = guilds.findIndex((guild) => guild.id === selectedId);
  const selected = guilds[selectedIndex];
  const disabled = working || !guilds.length;
  const optionId = (guild) => `guild-option-${guild.id}`;
  useEffect(() => {
    if (disabled && open) setOpen(false);
  }, [disabled, open]);
  useEffect(() => {
    if (!open) return undefined;
    listRef.current.focus();
    const outside = (event) => {
      if (!pickerRef.current?.contains(event.target)) setOpen(false);
    };
    document.addEventListener("mousedown", outside);
    return () => document.removeEventListener("mousedown", outside);
  }, [open]);
  useEffect(() => {
    if (open && guilds[active]) document.getElementById(optionId(guilds[active]))?.scrollIntoView?.({ block: "nearest" });
  }, [open, active]);
  const show = () => {
    setActive(Math.max(selectedIndex, 0));
    setOpen(true);
  };
  const close = (refocus) => {
    setOpen(false);
    if (refocus) triggerRef.current?.focus();
  };
  const choose = (index) => {
    close(true);
    if (guilds[index] && guilds[index].id !== selectedId) onSelect(guilds[index].id);
  };
  const onListKeyDown = (event) => {
    const moves = { ArrowDown: Math.min(active + 1, guilds.length - 1), ArrowUp: Math.max(active - 1, 0), Home: 0, End: guilds.length - 1 };
    if (Object.hasOwn(moves, event.key)) {
      event.preventDefault();
      setActive(moves[event.key]);
    } else if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      choose(active);
    } else if (event.key === "Escape") {
      event.preventDefault();
      close(true);
    } else if (event.key === "Tab") {
      // Focus returns to the trigger, so Tab continues from there.
      close(true);
    }
  };
  return h(
    "div",
    { ref: pickerRef, className: "server-picker" },
    h("label", { id: "guild-select-label", htmlFor: "guild-select" }, "Your server"),
    h(
      "button",
      {
        ref: triggerRef,
        id: "guild-select",
        type: "button",
        className: "select select-sm w-full server-picker-trigger",
        disabled,
        "aria-haspopup": "listbox",
        "aria-expanded": open ? "true" : "false",
        "aria-controls": "guild-select-list",
        "aria-labelledby": "guild-select-label guild-select-value",
        onClick: () => (open ? close(false) : show()),
        onKeyDown: (event) => {
          if (event.key !== "ArrowDown" && event.key !== "ArrowUp") return;
          event.preventDefault();
          show();
        },
      },
      selected && h(ServerIcon, { guild: selected }),
      h(
        "span",
        { id: "guild-select-value", className: "server-picker-name", title: selected?.name },
        selected ? selected.name : guilds.length ? "Select a server" : "No manageable servers",
      ),
    ),
    h(
      "ul",
      {
        ref: listRef,
        id: "guild-select-list",
        role: "listbox",
        tabIndex: -1,
        className: "menu menu-sm server-picker-list",
        "aria-labelledby": "guild-select-label",
        "aria-activedescendant": open && guilds[active] ? optionId(guilds[active]) : undefined,
        hidden: !open,
        onKeyDown: onListKeyDown,
      },
      open &&
        guilds.map((guild, index) =>
          h(
            "li",
            { key: guild.id, role: "none" },
            h(
              "div",
              {
                id: optionId(guild),
                role: "option",
                "aria-selected": guild.id === selectedId ? "true" : "false",
                className: [guild.id === selectedId && "menu-active", index === active && "menu-focus"].filter(Boolean).join(" ") || undefined,
                onMouseMove: () => index !== active && setActive(index),
                onClick: () => choose(index),
              },
              h(ServerIcon, { guild }),
              h("span", { className: "server-picker-name", title: guild.name }, guild.name),
            ),
          ),
        ),
    ),
    children,
  );
}

async function switchGuild(guildId) {
  if (!canLeave()) return;
  ui.set({ pendingDrafts: null, drafts: {}, draftBaselines: {}, copySource: null });
  pendingCase = null;
  state.guildId = guildId;
  if (!tabAllowed(state.tab)) state.tab = "dashboard";
  state.config = null;
  ui.set({ edits: "clean" });
  persistWorkspace();
  if (await loadConfig()) renderTab();
}

function directoryStatusLabel(server, receipt) {
  if (receipt) {
    if (botActionPhase(receipt) === "active") return ["Removing…", "badge-warning"];
    return [`Removal ${receipt.state}`, "badge-error"];
  }
  if (!server.active) return ["Departed", "badge-neutral"];
  return server.name ? ["Active", "badge-success"] : ["Active (details pending)", "badge-success"];
}

// A departure's request identity is saved before the first submission and kept
// across lost responses, reloads and navigation. Resubmitting with that same
// key recovers the original queued receipt instead of superseding it with a
// second request. It is cleared only once the outcome is known or the server
// refuses the identity for good. The reviewed membership (its enrollment token,
// also sent with the request so the server refuses a rejoined server) scopes
// it, so a rejoined server starts a fresh request.
function ownerLeaveRequestKey(server) {
  return `${workspaceKey()}:owner-leave:${server.guild_id}:${server.enrollment_id}`;
}

function ownerLeaveRequestId(server) {
  const key = ownerLeaveRequestKey(server);
  try {
    let requestId = window.sessionStorage.getItem(key);
    if (typeof requestId !== "string" || !requestId) {
      requestId = `${Math.floor(Date.now() / 1000)}_${crypto.randomUUID()}`;
      window.sessionStorage.setItem(key, requestId);
    }
    return requestId;
  } catch (_) {
    throw new Error("Browser session storage must be available to safely submit a server removal.");
  }
}

function clearOwnerLeaveRequest(server) {
  try {
    window.sessionStorage.removeItem(ownerLeaveRequestKey(server));
  } catch (_) {
    /* Storage may be unavailable; a stale key is refused by the server. */
  }
}

function TabOwnerServers() {
  const result = useApiRead("/api/owner-servers");
  const [receipts, setReceipts] = useState({});
  const timers = useRef(new Set());
  const mounted = useRef(true);
  useEffect(() => () => {
    mounted.current = false;
    for (const timer of timers.current) clearTimeout(timer);
    timers.current.clear();
  }, []);
  if (!isBotOwner()) {
    return emptyPanel(
      "Owner access required",
      "This section is available only to the configured bot owner.",
    );
  }
  const clearReceipt = (guildId) => {
    setReceipts((current) => {
      const next = { ...current };
      delete next[guildId];
      return next;
    });
  };
  const pollRemoval = (server, job, attempt) => {
    if (!mounted.current) return;
    setReceipts((current) => ({ ...current, [server.guild_id]: job }));
    if (botActionPhase(job) !== "active") {
      clearReceipt(server.guild_id);
      clearOwnerLeaveRequest(server);
      const label = server.name || `server ${server.guild_id}`;
      if (job.state === "succeeded") {
        toast(`The bot left ${label}. It can only return if someone adds it again.`, "success");
      } else {
        toast(`Removal ${job.state}: ${job.outcome || "verify the outcome in Discord."}`, "error");
      }
      result.refresh();
      return;
    }
    if (attempt >= actionReconcileDelays.length) {
      clearReceipt(server.guild_id);
      toast("The removal is still running. Refresh the directory to verify the outcome.", "info");
      return;
    }
    const timer = setTimeout(async () => {
      timers.current.delete(timer);
      try {
        const next = await api(
          `/api/owner-servers/${server.guild_id}/actions/${encodeURIComponent(job.id)}`,
          "POST",
        );
        pollRemoval(server, next, attempt + 1);
      } catch (error) {
        if (handleAuthLoss(error)) return;
        // Once the bot departs, the enrollment retirement hides the row from
        // enrollment-bound receipt reads; the refreshed directory tells the
        // truth.
        if (error.status === 404 || error.status === 409) {
          clearReceipt(server.guild_id);
          clearOwnerLeaveRequest(server);
          toast("The removal receipt is no longer listed. Refresh shows the current state.", "info");
          result.refresh();
          return;
        }
        // A lost response keeps the queued identity; retry the same receipt
        // instead of inventing an outcome.
        if (attempt + 1 < actionReconcileDelays.length) pollRemoval(server, job, attempt + 1);
        else {
          clearReceipt(server.guild_id);
          toast("Removal receipt unavailable. Refresh the directory to verify the outcome.", "error");
        }
      }
    }, actionReconcileDelays[attempt] || 30000);
    timers.current.add(timer);
  };
  const requestRemoval = async (server) => {
    if (!server.active) return;
    const label = server.name || `server ${server.guild_id}`;
    if (
      !confirm(
        `Remove the bot from ${label}? It will leave immediately and stop moderating that server. Someone must add it again for the bot to return.`,
      )
    )
      return;
    if (!server.enrollment_id) {
      toast("This server's membership is not known. Refresh the directory and try again.", "error");
      return;
    }
    let requestId;
    try {
      requestId = ownerLeaveRequestId(server);
    } catch (error) {
      toast(error.message, "error");
      return;
    }
    let job;
    try {
      job = await apiRequest(
        `/api/owner-servers/${server.guild_id}/leave`,
        "POST",
        { request_id: requestId },
        undefined,
        { "x-jmodguard-enrollment": server.enrollment_id },
      );
    } catch (error) {
      // An ambiguous failure (lost response, timeout, server error) keeps the
      // saved identity so the next attempt recovers the same request. A
      // definitive refusal can never succeed with that key, so it is released.
      if (
        (error.status === 410 && error.code === "action_request_expired") ||
        (error.status === 400 && error.code === "action_request_invalid") ||
        error.status === 409
      )
        clearOwnerLeaveRequest(server);
      reportApiError(error);
      return;
    }
    if (botActionPhase(job) === "invalid") {
      toast("Removal acknowledgment was invalid. Refresh the directory to verify the outcome.", "error");
      return;
    }
    pollRemoval(server, job, 0);
  };
  const servers = result.data?.servers || [];
  return h(
    "div",
    null,
    h("h2", null, "Bot servers"),
    h(
      "p",
      { className: "muted" },
      "Every server the bot is or was connected to, who added it, and when. Removing the bot makes it depart that server immediately.",
    ),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      h(ActionButton, { className: "btn", onClick: () => result.refresh() }, "Refresh"),
    ),
    h(
      DataTable,
      { headings: ["Server", "Server ID", "Added by", "Members", "Added", "Status", "Manage"] },
      result.loading || result.error || !servers.length
        ? h(
            "tr",
            null,
            h(
              "td",
              { colSpan: 7, className: result.error ? "err" : result.loading ? "" : "muted" },
              result.loading
                ? "Loading…"
                : result.error
                  ? "Could not load the directory: " + result.error.message
                  : "No servers yet. The bot appears here when someone adds it to a server.",
            ),
          )
        : servers.map((server) => {
            const [status, statusClass] = directoryStatusLabel(server, receipts[server.guild_id]);
            const icon = serverIconUrl(server);
            return h(
              "tr",
              { key: server.guild_id },
              h(
                "td",
                null,
                h(
                  "div",
                  { style: { display: "flex", alignItems: "center", gap: "8px" } },
                  icon &&
                    h("img", {
                      src: icon,
                      alt: "",
                      width: 24,
                      height: 24,
                      style: { borderRadius: "50%" },
                    }),
                  h("span", null, server.name || "Unknown server"),
                ),
              ),
              h("td", { className: "mono" }, server.guild_id),
              h(
                "td",
                null,
                server.added_by
                  ? `${server.added_by_name || "Unknown name"} (${server.added_by})`
                  : "Unknown",
              ),
              h(
                "td",
                null,
                server.member_count == null ? "Unknown" : Number(server.member_count).toLocaleString(),
              ),
              h("td", null, directoryTime(server.joined_at_ns)),
              h(
                "td",
                null,
                h("span", { className: `badge badge-soft badge-sm ${statusClass}` }, status),
              ),
              h(
                "td",
                null,
                server.active &&
                  h(IconButton, {
                    action: "leave",
                    label: "Remove",
                    disabled: Boolean(receipts[server.guild_id]),
                    onClick: () => requestRemoval(server),
                  }),
              ),
            );
          }),
    ),
  );
}

function ownerAccessRequired() {
  return emptyPanel("Owner access required", "This section is available only to the configured bot owner.");
}

// ---------------------------------------------------------------------------
// Database Usage (#613): one at-a-glance card per physical database. The API
// stays authoritative; an unknown measurement is never drawn as free capacity.
// ---------------------------------------------------------------------------

const MB = 1000000;
// The API reports each database's policy limits; these match jmodguard::capacity
// for a response that omits them.
const USAGE_DEFAULT_LIMITS = { limit: 1000 * MB, pressure: 700 * MB, capture: 800 * MB, growth: 900 * MB, snapshots: 400 * MB };

const USAGE_HELP =
  "Physical allocation includes reusable pages. Each database has its own 1,000 MB limit; the application starts faster cleanup at 700 MB (pressure), pauses snapshot capture at 800 MB and stops growth at 900 MB. Unknown measurements are not treated as free capacity. Category estimates are labeled when exact page attribution is unavailable. Details lists every diagnostic value.";

// One badge per operating state the API reports; `state` is the full explanation.
const USAGE_HEALTH = {
  normal: { label: "OK", tone: "success", state: "Measured allocation below pressure threshold; cross-process guard unknown" },
  pressure: { label: "Pressure", tone: "warning", state: "Pressure: cleanup accelerates at 700 MB" },
  capture_paused: { label: "Capture paused", tone: "warning", state: "Capture pause threshold reached at 800 MB" },
  growth_stopped: { label: "Growth stopped", tone: "error", state: "Application growth stop threshold reached at 900 MB" },
  application_paused: { label: "Writes paused", tone: "error", state: "API process growth guard paused writes" },
  stale: { label: "Stale", tone: "neutral", state: "Stale measurement" },
  unavailable: { label: "Unknown", tone: "neutral", state: "Measurement unavailable" },
};
const USAGE_FILL_TONES = { success: "ok", warning: "warn", error: "bad", neutral: "muted" };

const USAGE_CATEGORIES = [
  ["Snapshot table", "snapshot_data_bytes"],
  ["Moderation and evidence tables", "moderation_evidence_data_bytes"],
  ["Configuration tables", "configuration_data_bytes"],
  ["Operational tables", "operational_data_bytes"],
  ["Indexes", "index_bytes"],
  ["Reusable pages", "reusable_bytes"],
  ["System / unattributed", "system_unattributed_bytes"],
];

function usageMB(bytes) {
  return bytes == null ? null : `${(bytes / MB).toFixed(2)} MB`;
}

function thresholdMB(bytes) {
  return `${formatExact(Math.round(bytes / MB))} MB`;
}

function usageAge(seconds) {
  if (!seconds) return null;
  const elapsed = Math.max(0, Math.floor(Date.now() / 1000) - seconds);
  if (elapsed < 60) return "just now";
  if (elapsed < 3600) return `${Math.floor(elapsed / 60)} min ago`;
  if (elapsed < 86400) return `${Math.floor(elapsed / 3600)} h ago`;
  return `${Math.floor(elapsed / 86400)} d ago`;
}

function usageAgeMs(ms) {
  return ms == null ? null : usageAge(Math.floor(ms / 1000));
}

function usageLimits(database) {
  const policy = database.policy_limits || {};
  const limit = database.limit_bytes ?? policy.provider_hard_limit_bytes ?? USAGE_DEFAULT_LIMITS.limit;
  const marks = [
    { key: "pressure", name: "pressure", bytes: policy.pressure_bytes ?? USAGE_DEFAULT_LIMITS.pressure },
    { key: "capture", name: "capture pause", bytes: policy.capture_pause_bytes ?? USAGE_DEFAULT_LIMITS.capture },
    { key: "growth", name: "growth stop", bytes: policy.growth_stop_bytes ?? USAGE_DEFAULT_LIMITS.growth },
    { key: "limit", name: "hard limit", bytes: limit },
  ].map((mark) => ({ ...mark, short: formatExact(Math.round(mark.bytes / MB)) }));
  return { limit, marks, snapshots: database.snapshot_budget_bytes ?? policy.snapshot_budget_bytes ?? USAGE_DEFAULT_LIMITS.snapshots };
}

function usageTitle(database) {
  if (database.kind === "core") return "Core database";
  return database.policy?.kind === "dedicated" ? `Guild database · ${database.policy.guild_id}` : "Shared guild database";
}

// Every diagnostic value, for Details. Unknown values are listed by name only,
// on one line, so they never read as measured zeros or free capacity.
function usageFacts(database, health, limits) {
  const known = [];
  const unknown = [];
  const fact = (label, value) => {
    if (value == null || value === "" || value === "unknown" || value === "unavailable") unknown.push(label);
    else known.push([label, String(value)]);
  };
  const usage = database.usage;
  const measurement = database.measurement || {};
  const admission = database.application_admission || {};
  const physicalPart = (value, total) =>
    value == null || !total
      ? null
      : `${usageMB(value)} (${((value * 100) / total).toFixed(2)}% of allocated; ${((value * 100) / limits.limit).toFixed(2)}% of ${thresholdMB(limits.limit)} limit)`;
  fact("Database ID", database.id);
  fact("Role", database.role);
  fact("Availability", database.availability);
  fact("Operating state", health.state);
  fact("Application thresholds", limits.marks.map((mark) => `${mark.name} ${thresholdMB(mark.bytes)}`).join(" · "));
  fact("Headroom to hard limit", usageMB(database.headroom_bytes));
  fact("Pages", usage?.page_count != null && usage?.page_size != null ? `${formatExact(usage.page_count)} × ${formatExact(usage.page_size)} bytes` : null);
  fact("Reusable free pages", usage?.freelist_count == null ? null : formatExact(usage.freelist_count));
  fact("Measurement source", measurement.source);
  fact("Measurement capability", measurement.capability);
  if (measurement.error_code) fact("Probe status", measurement.error_code);
  fact("Provider reported bytes", usageMB(database.provider_usage?.bytes));
  fact("API growth guard", admission.api_process_growth_paused == null ? null : admission.api_process_growth_paused ? "paused" : "no local pause observed");
  fact("API startup", admission.api_startup_deferred == null ? null : admission.api_startup_deferred ? "deferred maintenance" : "normal");
  fact("Bot growth guard", admission.bot_cross_process_guard);
  fact("Capture", database.capture_status);
  if (database.kind === "guild") {
    const capture = database.capture || {};
    const counts = [capture.retained_count, capture.lost_count, capture.expired_count, capture.evicted_count];
    fact("Capture reason", capture.reason);
    fact("Bot heartbeat age", capture.heartbeat_age_ms == null ? null : `${Math.floor(capture.heartbeat_age_ms / 1000)} seconds`);
    fact("Retention maximum", capture.configured_retention_hours == null ? null : `${capture.configured_retention_hours} hours`);
    fact("Oldest retained observation", usageAgeMs(capture.oldest_available_snapshot_at_ms));
    fact("Capture epoch / counters reset", capture.epoch == null && capture.counters_reset_at_ms == null
      ? null : `${capture.epoch ?? "unknown"} / ${usageAgeMs(capture.counters_reset_at_ms) ?? "unknown"}`);
    fact("Retained / lost / expired / evicted", counts.every((count) => count == null) ? null : counts.map((count) => count ?? "unknown").join(" / "));
    fact("Verified effective snapshot allowance", usageMB(database.policy_limits?.effective_snapshot_allowance_bytes));
  }
  fact("Cleanup", database.cleanup_status);
  if (usage?.breakdown_error_code) fact("Breakdown status", usage.breakdown_error_code);
  if (usage?.breakdown_quality === "measured") {
    fact("Category basis", "Measured allocated pages; categories are disjoint");
    for (const [label, key] of USAGE_CATEGORIES) fact(label, physicalPart(usage.categories?.[key], usage.allocated_bytes));
    fact("Category sample", usageAge(usage.category_sampled_at));
  } else if (usage?.breakdown_quality === "estimated") {
    fact("Category basis", "Physical category allocation unavailable; snapshot value is encoded payload estimate");
    fact("Snapshot payload estimate", usageMB(usage.snapshot_logical_bytes));
    fact("Reusable pages", usageMB(usage.reusable_bytes));
    fact("Indexes and other allocation", null);
  } else if (usage) {
    fact("Category basis", "Physical category allocation unavailable");
    fact("Reusable pages", usageMB(usage.reusable_bytes));
  }
  if (usage?.snapshot_count != null) fact("Retained snapshots", formatExact(usage.snapshot_count));
  fact("External evidence file bytes", usageMB(database.external_evidence_storage?.bytes));
  if (!usage || (usage.breakdown_quality === "unavailable" && usage.snapshot_count == null)) fact("Breakdown", null);
  return { known, unknown };
}

function usageMetric(label, body, caption) {
  return h(
    "div",
    { className: "du-metric" },
    h("span", { className: "du-metric-label" }, label),
    body,
    h("span", { className: "du-metric-caption" }, caption),
  );
}

function UsageCard({ database: initial, stale, revision }) {
  const [detail, setDetail] = useState(null);
  const [detailRead, setDetailRead] = useState({ kind: "idle" });
  const currentRevision = useRef(revision);
  currentRevision.current = revision;
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  // A detail read replaces only the aggregate sample it was requested from.
  const database = detail?.source === initial ? detail.database : initial;
  const oldSample = stale && detail?.revision !== revision;
  const health = USAGE_HEALTH[database.status] || { label: "Unknown", tone: "neutral", state: "Unknown" };
  const limits = usageLimits(database);
  const usage = database.usage;
  const allocated = usage?.allocated_bytes ?? null;
  const percent = allocated == null ? null : database.percent_of_limit ?? (allocated * 100) / limits.limit;
  const next = allocated == null ? null : limits.marks.find((mark) => allocated < mark.bytes);
  const title = usageTitle(database);
  const headingId = `usage-${database.id}`;
  const figures = allocated == null
    ? [h("strong", { key: "size", className: "du-size" }, "Allocation unknown"), h("span", { key: "note" }, "Not counted as free capacity")]
    : [
        h("strong", { key: "size", className: "du-size" }, `${usageMB(allocated)} allocated`),
        h("span", { key: "percent" }, `${percent.toFixed(2)}% of ${thresholdMB(limits.limit)}`),
        h("span", { key: "next" }, next ? `${usageMB(next.bytes - allocated)} to ${next.name} (${thresholdMB(next.bytes)})` : "At or over the hard limit"),
      ];
  const thresholds = limits.marks.map((mark) => `${mark.name} ${thresholdMB(mark.bytes)}`).join(", ");
  const gaugeSummary = allocated == null
    ? `${title}: allocation unknown, not counted as free capacity. Thresholds: ${thresholds}.`
    : `${title}: ${usageMB(allocated)} allocated, ${percent.toFixed(2)}% of the ${thresholdMB(limits.limit)} limit. Thresholds: ${thresholds}.`;
  const measurement = database.measurement || {};
  const measured = usageAge(measurement.sampled_at);
  const lastSuccess = usageAge(measurement.last_success_at);
  const stamp = (seconds) => (seconds ? new Date(seconds * 1000).toLocaleString(undefined, { timeZoneName: "short" }) : "unavailable");
  const freshness = measured
    ? `Measured ${measured}${lastSuccess && measurement.last_success_at !== measurement.sampled_at ? ` · last success ${lastSuccess}` : ""}`
    : lastSuccess ? `Last success ${lastSuccess}` : "Not measured";
  const pages = usage?.page_count;
  const free = usage?.freelist_count;
  const pageMetric = pages != null && free != null
    ? usageMetric(
        "Pages",
        h(ShareBar, {
          total: pages,
          parts: [{ key: "used", value: pages - free, tone: "accent" }, { key: "free", value: free, tone: "muted" }],
          summary: `Pages: ${formatExact(pages - free)} in use and ${formatExact(free)} reusable of ${formatExact(pages)}.`,
        }),
        [
          h("span", { key: "used", className: "du-legend du-tone-accent" }, `${formatExact(pages - free)} used`),
          h("span", { key: "free", className: "du-legend du-tone-muted" }, `${formatExact(free)} reusable`),
          ` of ${formatExact(pages)}${usage.page_size != null ? ` × ${formatExact(usage.page_size)} B` : ""}`,
        ],
      )
    : usageMetric("Pages", h(ShareBar, { total: 1, parts: [], unknown: true, summary: "Page counts unavailable." }), "Page counts unavailable");
  const estimate = database.snapshot_usage_estimate_bytes;
  const snapshotMetric = database.kind === "guild" &&
    usageMetric(
      "Snapshots",
      h(ShareBar, {
        total: limits.snapshots,
        unknown: estimate == null,
        parts: estimate == null ? [] : [{ key: "estimate", value: estimate, tone: "accent" }],
        summary: estimate == null
          ? `Snapshot footprint unknown; budget ${thresholdMB(limits.snapshots)}.`
          : `Snapshot footprint estimate ${usageMB(estimate)} of the ${thresholdMB(limits.snapshots)} budget.`,
      }),
      estimate == null
        ? `Footprint unknown · ${thresholdMB(limits.snapshots)} budget`
        : `${usageMB(estimate)} of ${thresholdMB(limits.snapshots)} budget (${((estimate * 100) / limits.snapshots).toFixed(2)}%; conservative logical estimate)`,
    );
  const facts = usageFacts(database, health, limits);
  const showBreakdown = async () => {
    const asked = revision;
    const source = initial;
    setDetailRead({ kind: "loading" });
    try {
      const updated = await apiRead(`/api/database-usage/${encodeURIComponent(database.id)}`);
      if (!mounted.current) return;
      if (asked === currentRevision.current) setDetail({ database: updated, revision: asked, source });
      setDetailRead({ kind: "idle" });
    } catch (error) {
      if (mounted.current) setDetailRead(asked === currentRevision.current ? { kind: "failed", message: error.message } : { kind: "idle" });
    }
  };
  return h(
    "section",
    { className: "card bg-base-100 card-border du-card", "aria-labelledby": headingId, "data-usage-stale": oldSample ? "1" : undefined },
    h(
      "div",
      { className: "card-body" },
      h(
        "header",
        { className: "du-head" },
        h("h3", { id: headingId, className: "du-title" }, title),
        h("span", { className: `badge badge-soft badge-sm badge-${health.tone} du-health`, title: health.state }, health.label),
        h("span", { className: "du-chip", title: `Measured: ${stamp(measurement.sampled_at)}. Last successful sample: ${stamp(measurement.last_success_at)}.` }, freshness),
      ),
      oldSample && h("p", { className: "du-stale" }, "Stale: the latest usage refresh failed. These figures are not a current capacity sample."),
      h("p", { className: "du-figures" }, ...figures),
      h(CapacityGauge, { used: allocated, limit: limits.limit, marks: limits.marks, tone: USAGE_FILL_TONES[health.tone], summary: gaugeSummary }),
      h(
        "div",
        { className: "du-metrics" },
        pageMetric,
        snapshotMetric,
        usageMetric("Trend", h("span", { className: "du-trend-empty" }), "No usage history recorded yet"),
      ),
      h(
        "details",
        { className: "du-details" },
        h("summary", null, "Details"),
        h("dl", { className: "du-facts" }, ...facts.known.map(([label, value]) => h("div", { key: label }, h("dt", null, label), h("dd", null, value)))),
        facts.unknown.length > 0 && h("p", { className: "du-unknown" }, `Not measured or unknown: ${facts.unknown.join(", ")}.`),
        h(
          "div",
          { className: "du-detail-actions" },
          h(ActionButton, { className: "btn btn-sm", disabled: detailRead.kind === "loading", onClick: () => { showBreakdown(); } }, "Show current breakdown"),
          detailRead.kind === "failed" && h("span", { className: "err", role: "status" }, `Breakdown unavailable: ${detailRead.message}`),
        ),
      ),
    ),
  );
}

function TabDatabaseUsage() {
  return isBotOwner() ? h(DatabaseUsage) : ownerAccessRequired();
}

function DatabaseUsage() {
  const [view, setView] = useState({ snapshot: null, freshness: "fresh", revision: 0, error: null });
  const flight = useRef(null);
  const mounted = useRef(true);
  // Overlapping refreshes share one read. A failed read keeps the last
  // successful figures, marked stale, instead of clearing them.
  const refresh = () => {
    if (flight.current) return flight.current;
    // Re-render relative sample ages even while a visibility-triggered read waits.
    setView((current) => ({ ...current }));
    flight.current = apiRead("/api/database-usage")
      .then((snapshot) => {
        if (mounted.current && document.visibilityState !== "hidden") setView((current) => ({ snapshot, freshness: "fresh", revision: current.revision + 1, error: null }));
      })
      .catch((error) => {
        if (mounted.current) setView((current) => ({ ...current, freshness: "stale", revision: current.revision + 1, error }));
      })
      .finally(() => {
        flight.current = null;
      });
    return flight.current;
  };
  useEffect(() => {
    mounted.current = true;
    let timer;
    const poll = () => {
      clearTimeout(timer);
      if (!mounted.current || document.visibilityState === "hidden") return;
      refresh().finally(() => {
        clearTimeout(timer);
        if (mounted.current && document.visibilityState !== "hidden") timer = setTimeout(poll, 60000);
      });
    };
    const visible = () => {
      clearTimeout(timer);
      if (document.visibilityState !== "hidden") poll();
    };
    poll();
    document.addEventListener("visibilitychange", visible);
    return () => {
      mounted.current = false;
      clearTimeout(timer);
      document.removeEventListener("visibilitychange", visible);
    };
  }, []);
  const snapshot = view.snapshot;
  const databases = snapshot?.databases ?? [];
  return h(
    "div",
    { className: "contents database-usage-page" },
    pageHeader(
      "Database Usage",
      snapshot ? `Response ${usageAge(snapshot.generated_at) ?? "time unknown"} · catalog ${snapshot.catalog_status || "unknown"}` : "Every physical database",
      h(ActionButton, { id: "refresh-usage", className: "btn btn-sm", onClick: () => { refresh(); } }, "Refresh"),
      h(InfoTip, { id: "usage-help", label: "About database usage" }, USAGE_HELP),
    ),
    view.error &&
      h(
        "p",
        { id: "usage-status", className: "alert alert-soft alert-warning", role: "status" },
        snapshot
          ? `Usage refresh failed: ${view.error.message}. The figures below are from the last successful response.`
          : `Database usage unavailable: ${view.error.message}`,
      ),
    !snapshot && !view.error && h("p", { className: "muted", role: "status", "aria-busy": "true" }, "Loading database usage…"),
    snapshot && !databases.length && h("p", { className: "muted" }, "No databases reported."),
    h(
      "div",
      { id: "usage-cards", className: "du-grid" },
      databases.map((database) => h(UsageCard, { key: database.id, database, stale: view.freshness === "stale", revision: view.revision })),
    ),
  );
}

// Platform analytics (#618): the fleet-wide owner view. One owner-only read
// per range or Refresh; every panel renders from that response.
const PLATFORM_METRICS = [
  ["warns", "Warnings", "an-warns"],
  ["bans", "Bans", "an-bans"],
  ["automod", "Spam hits", "an-spam"],
  ["mod_actions", "Mod actions", "an-mod"],
  ["timeouts", "Timeouts", "an-timeouts"],
  ["joins", "Joins", "an-joins"],
  ["leaves", "Leaves", "an-leaves"],
];

const PLATFORM_HELP =
  "Counts come from records the bot stored, bucketed by UTC day; days before recording started show zero. Changes compare with the previous period of the same length; joins and leaves have no 90-day comparison. Actions count automod enforcement and moderator commands once each. Share is a server's part of the fleet total.";

const PLATFORM_COVERAGE = {
  importing: { label: "importing", tone: "warn", title: "The initial member import is still running." },
  uncertain: { label: "unverified", tone: "warn", title: "A coverage gap awaits Reconcile on the Members page." },
  unavailable: { label: "unavailable", tone: "warn", title: "This server's member directory could not be read." },
};

const PLATFORM_SERVER_FILTERS = [
  ["all", "All servers"],
  ["active", "Active"],
  ["departed", "Departed"],
  ["activity", "With activity"],
  ["directory", "Member directory incomplete"],
];

function platformMemberStatus(members) {
  if (!members || members.status === "unavailable") return "Member totals are unavailable.";
  const unavailable = members.guilds_unavailable
    ? ` Member totals cover ${members.guilds_counted ?? 0} of ${members.servers ?? 0} servers; the rest were unavailable.`
    : "";
  const incomplete = members.guilds_incomplete
    ? ` ${members.guilds_incomplete} counted ${members.guilds_incomplete === 1 ? "server has" : "servers have"} a member directory still importing or awaiting Reconcile.`
    : "";
  return `Member totals are ${members.status || "complete"}.${unavailable}${incomplete}`;
}

// Enforcement actions counted once each: automod enforcement (including
// dedicated spam bans) plus moderator commands, as in the activity heat map.
function platformActions(totals) {
  return (totals?.automod ?? 0) + (totals?.mod_actions ?? 0);
}

function platformServerLabel(row) {
  return row.name || `Server ${row.guild_id}`;
}

// One server row with its display facts, enforcement actions and directory state.
function platformRows(snapshot) {
  const byGuild = snapshot?.members?.by_guild ?? {};
  return (snapshot?.guilds ?? []).map((guild) => {
    // An active server missing from the directory totals could not be read.
    const directory = guild.active ? byGuild[guild.guild_id] : null;
    return {
      ...guild,
      actions: platformActions(guild),
      previousActions: platformActions(guild.previous),
      members: directory ? directory.current : null,
      coverage: !guild.active ? null : !directory ? "unavailable" : directory.coverage === "complete" ? null : directory.coverage,
    };
  });
}

function sharePercentText(value, total) {
  if (!total) return "0%";
  const percent = (value * 100) / total;
  return percent >= 10 || percent === 0 ? `${Math.round(percent)}%` : `${percent.toFixed(1).replace(/\.0$/, "")}%`;
}

function platformRankRows(rows, value, text) {
  return rows.map((row) => ({
    key: row.guild_id,
    label: platformServerLabel(row),
    title: `${platformServerLabel(row)} (${row.guild_id})`,
    count: value(row),
    text: text?.(row),
  }));
}

// Most and least active current servers, the largest changes against the
// previous window, and which servers drive a metric's fleet total.
function platformRankings(rows, totals) {
  const byName = (a, b) => platformServerLabel(a).localeCompare(platformServerLabel(b)) || a.guild_id.localeCompare(b.guild_id);
  const active = rows.filter((row) => row.active);
  const most = active.filter((row) => row.actions > 0).sort((a, b) => b.actions - a.actions || byName(a, b)).slice(0, 5);
  const least = [...active].sort((a, b) => a.actions - b.actions || byName(a, b)).slice(0, 5);
  const change = (row) => row.actions - row.previousActions;
  const movers = rows.filter((row) => change(row) !== 0).sort((a, b) => Math.abs(change(b)) - Math.abs(change(a)) || byName(a, b)).slice(0, 5);
  const share = (field) =>
    platformRankRows(
      rows.filter((row) => row[field] > 0).sort((a, b) => b[field] - a[field] || byName(a, b)).slice(0, 5),
      (row) => row[field],
      (row) => sharePercentText(row[field], totals[field]),
    );
  return {
    most: platformRankRows(most, (row) => row.actions),
    least: platformRankRows(least, (row) => row.actions),
    movers: platformRankRows(movers, (row) => Math.abs(change(row)), (row) => formatDelta(row.actions, row.previousActions).text),
    share,
  };
}

const PLATFORM_SORTS = {
  name: (row) => platformServerLabel(row).toLowerCase(),
  members: (row) => row.members ?? -1,
  actions: (row) => row.actions,
  ...Object.fromEntries(PLATFORM_METRICS.map(([field]) => [field, (row) => row[field] ?? 0])),
};

function PlatformServerTable({ rows, totals, rangeLabel }) {
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState("all");
  const [sort, setSort] = useState({ key: "actions", direction: "descending" });
  const needle = query.trim().toLowerCase();
  const shown = rows
    .filter((row) => !needle || platformServerLabel(row).toLowerCase().includes(needle) || row.guild_id.includes(needle))
    .filter((row) => ({
      all: true,
      active: row.active,
      departed: !row.active,
      activity: row.actions > 0 || PLATFORM_METRICS.some(([field]) => row[field] > 0),
      directory: Boolean(row.coverage),
    })[filter])
    .sort((a, b) => {
      const key = PLATFORM_SORTS[sort.key];
      const [x, y] = [key(a), key(b)];
      const order = x < y ? -1 : x > y ? 1 : 0;
      return (sort.direction === "ascending" ? order : -order) || a.guild_id.localeCompare(b.guild_id);
    });
  const header = (key, label) =>
    h(
      "th",
      { key, scope: "col", "aria-sort": sort.key === key ? sort.direction : "none" },
      h(
        "button",
        {
          type: "button",
          className: "pa-sort",
          onClick: () =>
            setSort((current) => ({
              key,
              direction: current.key === key ? (current.direction === "descending" ? "ascending" : "descending") : key === "name" ? "ascending" : "descending",
            })),
        },
        label,
        sort.key === key
          ? h(Icon, { name: sort.direction === "descending" ? "chevron-down" : "chevron-up", intent: "neutral", className: "pa-sort-mark" })
          : h("span", { className: "pa-sort-mark", "aria-hidden": "true" }),
      ),
    );
  const metricCell = (row, field, color) => {
    const value = row[field] ?? 0;
    const share = totals[field] ? (value * 100) / totals[field] : 0;
    return h(
      "td",
      { key: field, className: "pa-metric", title: `${formatExact(value)} · ${sharePercentText(value, totals[field])} of fleet` },
      h("span", { className: "pa-num" }, formatCount(value)),
      h("span", { className: `pa-share ${color}`, "aria-hidden": "true" }, value > 0 ? h("span", { className: "pa-share-fill", style: { width: `${Math.max(3, share).toFixed(1)}%` } }) : null),
    );
  };
  return h(
    "div",
    { className: "pa-servers" },
    h(
      "div",
      { className: "pa-controls" },
      h("input", {
        type: "search",
        className: "input input-sm",
        placeholder: "Filter by name or ID",
        "aria-label": "Filter servers",
        value: query,
        onChange: (event) => setQuery(event.target.value),
      }),
      h(
        "select",
        { className: "select select-sm", "aria-label": "Server status", value: filter, onChange: (event) => setFilter(event.target.value) },
        ...PLATFORM_SERVER_FILTERS.map(([value, label]) => h("option", { key: value, value }, label)),
      ),
      h("span", { className: "pa-count muted", role: "status" }, `${shown.length} of ${rows.length} servers`),
    ),
    h(
      "div",
      { className: "table-scroll rounded-box", tabIndex: 0, role: "region", "aria-label": "Servers" },
      h(
        "table",
        { className: "table table-sm pa-table" },
        h(
          "thead",
          null,
          h("tr", null, header("name", "Server"), header("members", "Members"), header("actions", "Actions"), ...PLATFORM_METRICS.map(([field, label]) => header(field, label))),
        ),
        h(
          "tbody",
          null,
          shown.length
            ? shown.map((row) => {
                const delta = formatDelta(row.actions, row.previousActions);
                const coverage = PLATFORM_COVERAGE[row.coverage];
                return h(
                  "tr",
                  { key: row.guild_id },
                  h(
                    "th",
                    { scope: "row", title: `${platformServerLabel(row)} (${row.guild_id})` },
                    h(
                      "div",
                      { className: "pa-server" },
                      h(ServerIcon, { guild: { id: row.guild_id, icon: row.icon_hash, name: row.name } }),
                      h(
                        "span",
                        { className: "pa-server-text" },
                        h("span", { className: "pa-server-name" }, row.name || "Unknown server"),
                        h("span", { className: "pa-server-id mono" }, row.guild_id),
                      ),
                      !row.active && h("span", { className: "badge badge-soft badge-sm badge-neutral" }, "Departed"),
                    ),
                  ),
                  h(
                    "td",
                    { className: "pa-metric" },
                    h("span", { className: "pa-num", title: row.members == null ? undefined : formatExact(row.members) }, row.active ? (row.members == null ? "—" : formatCount(row.members)) : "—"),
                    coverage && h("span", { className: `an-badge an-badge-${coverage.tone}`, title: coverage.title }, coverage.label),
                  ),
                  h(
                    "td",
                    { className: "pa-metric", title: `${formatExact(row.actions)} actions, ${delta.exact} vs previous ${rangeLabel}` },
                    h("span", { className: "pa-num" }, formatCount(row.actions)),
                    h("span", { className: `an-delta an-delta-${delta.direction}` }, delta.text),
                  ),
                  ...PLATFORM_METRICS.map(([field, , color]) => metricCell(row, field, color)),
                );
              })
            : h("tr", null, h("td", { colSpan: 3 + PLATFORM_METRICS.length, className: "muted" }, rows.length ? "No servers match this filter." : "No servers yet.")),
        ),
      ),
    ),
  );
}

function TabPlatformAnalytics() {
  return isBotOwner() ? h(PlatformAnalytics) : ownerAccessRequired();
}

function PlatformAnalytics() {
  const [days, setDays] = useState(30);
  const result = useApiRead(`/api/platform/analytics?days=${days}`, { retainData: true });
  const snapshot = result.data;
  const loading = !snapshot && result.loading;
  const failed = !snapshot && Boolean(result.error);
  const rangeLabel = `${days} days`;
  const points = toTrendPoints(snapshot?.series);
  const previous = snapshot?.previous ?? null;
  const members = snapshot?.members;
  const rows = platformRows(snapshot);
  const totals = Object.fromEntries(PLATFORM_METRICS.map(([field]) => [field, sumSeries(points, field === "mod_actions" ? "modActions" : field)]));
  const rankings = platformRankings(rows, totals);
  const tile = (label, field, color) =>
    h(KpiTile, {
      key: label,
      label,
      value: totals[field],
      delta: snapshot ? formatDelta(totals[field], previous?.[field]) : null,
      values: snapshot ? points.map((point) => point[field === "mod_actions" ? "modActions" : field] ?? 0) : null,
      color,
      loading,
      unavailable: failed,
      rangeLabel,
    });
  const membersUnavailable = failed || (snapshot && (!members || members.status === "unavailable"));
  const memberBadge = members?.status === "partial" ? { label: "partial", tone: "warn", title: platformMemberStatus(members) } : null;
  const netPrevious = previous?.joins == null || previous?.leaves == null ? null : previous.joins - previous.leaves;
  const body = (render) =>
    snapshot ? render() : h("p", { className: "an-empty muted", "aria-busy": loading ? "true" : "false" }, failed ? "Unavailable" : "Loading…");
  const rank = (title, id, props) => analyticsPanel(title, id, body(() => h(RankList, props)), "an-span-4");
  const note = result.error
    ? h("p", { id: "platform-analytics-status", className: "alert alert-soft alert-warning", role: "status" }, snapshot
        ? `Platform analytics refresh failed: ${result.error.message}. Showing the last successful response (stale). ${members?.status === "complete" ? "" : platformMemberStatus(members)}`.trim()
        : `Platform analytics unavailable: ${result.error.message}`)
    : snapshot && members?.status !== "complete" && h("p", { id: "platform-analytics-status", className: "an-note muted", role: "status" }, platformMemberStatus(members));
  return h(
    "div",
    { className: "contents analytics-page platform-page" },
    pageHeader(
      "Platform analytics",
      `All servers · last ${rangeLabel} (UTC)`,
      h(RangeSelector, { value: days, onChange: setDays }),
      h(ActionButton, { id: "refresh-platform-analytics", className: "btn btn-sm", onClick: () => result.refresh() }, "Refresh"),
      h(InfoTip, { id: "platform-analytics-help", label: "About these figures" }, PLATFORM_HELP),
    ),
    note,
    h(
      "div",
      { id: "platform-analytics", className: "contents" },
      h(
        "section",
        { className: "an-kpis", "aria-label": `Fleet totals for the last ${rangeLabel}` },
        h(KpiTile, { label: "Servers", value: snapshot?.servers ?? 0, loading, unavailable: failed, rangeLabel }),
        h(KpiTile, { label: "Members", value: members?.current ?? 0, loading, unavailable: membersUnavailable, badge: memberBadge, rangeLabel }),
        h(KpiTile, {
          label: "Net growth",
          value: totals.joins - totals.leaves,
          delta: snapshot ? formatDelta(totals.joins - totals.leaves, netPrevious) : null,
          values: snapshot ? runningNetShape(points) : null,
          color: "an-joins",
          loading,
          unavailable: failed,
          rangeLabel,
        }),
        tile("Joins", "joins", "an-joins"),
        tile("Leaves", "leaves", "an-leaves"),
        ...PLATFORM_METRICS.slice(0, 5).map(([field, label, color]) => tile(label, field, color)),
      ),
      h(
        "div",
        { className: "an-grid" },
        analyticsPanel(
          "Enforcement per day",
          "pa-enforcement",
          body(() => (points.length ? h(EnforcementChart, { points }) : h("p", { className: "an-empty muted" }, "No platform trend data recorded yet."))),
          "an-span-7",
        ),
        analyticsPanel(
          "Fleet membership per day",
          "pa-membership",
          body(() => (points.length ? h(MembershipChart, { points }) : h("p", { className: "an-empty muted" }, "No platform trend data recorded yet."))),
          "an-span-5",
        ),
        rank("Most active servers", "pa-most", { label: "Most active servers by actions", empty: "No actions in this range.", rows: rankings.most }),
        rank("Least active servers", "pa-least", { label: "Least active servers by actions", empty: "No active servers.", color: "an-timeouts", rows: rankings.least }),
        rank("Biggest movers", "pa-movers", {
          label: `Largest change in actions vs previous ${rangeLabel}`,
          empty: `No change from the previous ${rangeLabel}.`,
          color: "an-joins",
          rows: rankings.movers,
        }),
        rank("Share of bans", "pa-share-bans", { label: "Share of fleet bans by server", empty: "No bans in this range.", color: "an-bans", rows: rankings.share("bans") }),
        rank("Share of spam hits", "pa-share-spam", { label: "Share of fleet spam hits by server", empty: "No spam hits in this range.", color: "an-spam", rows: rankings.share("automod") }),
        rank("Share of mod actions", "pa-share-mod", { label: "Share of fleet mod actions by server", empty: "No mod actions in this range.", rows: rankings.share("mod_actions") }),
        analyticsPanel("Servers", "pa-servers", body(() => h(PlatformServerTable, { rows, totals, rangeLabel })), "an-span-12"),
      ),
    ),
  );
}

function App() {
  const appearance = useAppearance();
  const display = useStore(ui);

  useEffect(() => {
    const beforeUnload = (event) => {
      if ((ui.getSnapshot().edits === "dirty") || (ui.getSnapshot().activity === "busy")) {
        event.preventDefault();
        event.returnValue = "";
      }
    };
    window.addEventListener("beforeunload", beforeUnload);
    return () => window.removeEventListener("beforeunload", beforeUnload);
  }, []);
  // Each item's icon and its fixed meaning color come from NAV_ICONS (frontend/icons.js).
  const navigationBase = [
    ["dashboard", "Dashboard"],
    ["rules", "Automod Rules"],
    ["badwords", "Badwords"],
    ["known-spam", "Known spam"],
    ["known-bad-images", "Known-bad images"],
    ["commands", "Commands"],
    ["templates", "Templates"],
    ["logchannels", "Log Channels"],
    ["analytics", "Analytics"],
    ["events", "Audit Log"],
    ["members", "Members"],
    ["bans", "Bans"],
    ["actions", "Bot actions"],
    ["cases", "Cases"],
    ["settings", "Settings"],
  ];
  // Owner sections live in the main interface, not on the separate
  // /administration page (Updates only). They appear only for the bot owner.
  const navigation = isBotOwner()
    ? [...navigationBase, ["servers", "Bot servers"], ["usage", "Database Usage"], ["platform-analytics", "Platform analytics"]]
    : navigationBase;
  return h(
    BusyContext.Provider,
    { value: (display.activity === "busy") },
    isPreview() &&
      h(
        "div",
        { id: "preview-banner", className: "preview-banner alert alert-soft alert-warning", role: "status" },
        "Preview — changes disabled",
      ),
    display.view === "main-view"
      ? h(
          "div",
          { id: "main-view" },
          h(
            "aside",
            { className: "sidebar" },
            h(
              "div",
              { className: "brand" },
              h("span", { className: "brand-mark", "aria-hidden": true }, "jG"),
              h("span", null, "jModGuard", h("small", null, "MODERATION WORKSPACE")),
            ),
            h(
              ServerPicker,
              { guilds: state.guilds, selectedId: state.guildId, onSelect: switchGuild },
              state.me?.role_discovery_incomplete && h("p", { className: "muted", role: "status" },
                "Some server access checks could not finish. ",
                h(ActionButton, { className: "btn btn-xs", onClick: async () => {
                  if (canLeave()) await init();
                } }, "Retry server discovery"),
              ),
            ),
            h(
              "nav",
              { "aria-label": "Workspace navigation" },
              h(
                "ul",
                { id: "tabs", className: "menu w-full" },
                ...navigation.flatMap(([tab, label], i) => [
                  (i === 0 || i === 6 || i === navigationBase.length) &&
                    h(
                      "li",
                      { key: `group-${i}`, className: "nav-label menu-title" },
                      i === 0 ? "Workspace" : i === 6 ? "Community" : "Owner",
                    ),
                  tabAllowed(tab) &&
                  h(
                    "li",
                    { key: tab },
                    h(
                      ActionButton,
                      {
                        "data-tab": tab,
                        className: state.tab === tab ? "active menu-active btn-active" : "",
                        "aria-current": state.tab === tab ? "page" : undefined,
                        onClick: () => navigate(tab),
                      },
                      h(Icon, { name: NAV_ICONS[tab].icon, intent: NAV_ICONS[tab].intent, className: "nav-icon" }),
                      label,
                    ),
                  ),
                ]),
              ),
            ),
            h(OwnerTools),
            h(
              "div",
              { className: "sidebar-footer" },
              h(Appearance, appearance),
              h(
                "div",
                { className: "sidebar-user" },
                h(
                  "span",
                  { id: "user-chip" },
                  `${state.me?.user.username} (${state.me?.user.global_name || ""})`.trim(),
                ),
                h(
                  ActionButton,
                  { id: "logout-btn", className: "btn btn-sm btn-ghost", onClick: logout },
                  h(Icon, { name: NAV_ICONS["sign-out"].icon, intent: NAV_ICONS["sign-out"].intent, className: "nav-icon" }),
                  "Sign out",
                ),
              ),
            ),
          ),
          h(
            "div",
            { className: "workspace" },
            h(
              "header",
              { className: "navbar bg-base-100 workspace-header" },
              h(
                "div",
                { className: "workspace-title" },
                h("span", { className: "eyebrow" }, "SERVER WORKSPACE"),
                h("strong", { id: "workspace-name" }, selectedGuild()?.name || "Your servers"),
              ),
              h(
                "span",
                { id: "workspace-status" },
                isPreview()
                  ? "Preview — changes disabled"
                  : guildAccess() === "dashboard"
                    ? "Read-only access"
                  : (display.activity === "busy")
                    ? "Working…"
                    : (display.edits === "dirty")
                      ? "Unsaved changes"
                      : "Changes saved with each action",
              ),
            ),
            h(
              "main",
              { id: "content", tabIndex: -1, onChange: trackDraft },
              h(Content, { key: display.contentRevision }),
            ),
            h(
              "footer",
              { className: "workspace-footer" },
              "jModGuard ",
              h("span", null, "Built for better communities."),
            ),
          ),
        )
      : h(
          "div",
          { id: display.view, className: "welcome-view" },
          h(Appearance, appearance),
          h(
            "div",
            { className: "login-card card bg-base-100" },
            h(
              "div",
              { className: "card-body" },
              h("span", { className: "brand-mark", "aria-hidden": true }, "jG"),
              h(
                "p",
                { className: "eyebrow" },
                display.view === "login-view" ? "JMODGUARD / MODERATION WORKSPACE" : "YOUR COMMUNITY, IN GOOD HANDS",
              ),
              display.view === "login-view"
                ? h(
                    React.Fragment,
                    null,
                    h("h1", null, "A calmer place", h("br"), "to keep order."),
                    h(
                      "p",
                      null,
                      "Thoughtful protection for your Discord community.",
                      h("br"),
                      "Manage your rules, people, and settings in one place.",
                    ),
                    h(
                      "a",
                      { id: "login-link", className: "btn btn-primary", href: authUrl("/login") },
                      "Continue with Discord",
                      h(Icon, { name: "external-link" }),
                    ),
                    h("small", null, "For server managers and community builders."),
                  )
                : h(
                    React.Fragment,
                    null,
                    h("h1", null, "Getting things ready."),
                    h("p", { id: "startup-status", role: "status" }, display.startupStatus),
                    display.previewAvailability === "unavailable" &&
                      h(
                        "a",
                        {
                          id: "preview-return-link",
                          className: "btn btn-primary",
                          href: "/",
                          onClick: returnFromPreview,
                        },
                        "Return to owner dashboard",
                      ),
                  ),
            ),
          ),
        ),
    (!display.editor || display.view !== "main-view") && h(Notification),
    display.view === "main-view" &&
      h(
        Dialog,
        {
          open: !!display.editor,
          onClose: () => {
            if (canLeave()) closeOverlay();
          },
          className: "modal modal-open modal-overlay",
          "aria-label": "Configuration editor",
        },
        display.editor,
        h(Notification),
      ),
  );
}
function Notification() {
  return h(
    "div",
    {
      id: "toast",
      role: "status",
      "aria-live": "polite",
      "aria-atomic": true,
      className: `toast toast-end toast-bottom ${ui.getSnapshot().notification?.kind || ""}`,
      hidden: !ui.getSnapshot().notification,
    },
    ui.getSnapshot().notification && h("div", {
      className: `alert alert-soft ${ui.getSnapshot().notification.kind === "error" ? "alert-error" : ui.getSnapshot().notification.kind === "success" ? "alert-success" : ""}`,
    }, ui.getSnapshot().notification.message),
  );
}
function trackDraft(event) {
  if (
    event.target.matches("input, textarea, select") &&
    !event.target.hasAttribute("data-search") &&
    !["events", "members", "bans"].includes(state.tab)
  )
    markDirty();
}
function Content() {
  const components = {
    dashboard: tabDashboard,
    analytics: tabAnalytics,
    rules: tabRules,
    badwords: tabBadwords,
    "known-spam": tabKnownSpam,
    "known-bad-images": tabKnownBadImages,
    commands: tabCommands,
    templates: tabTemplates,
    logchannels: tabLogChannels,
    events: tabEvents,
    members: tabMembers,
    bans: tabBans,
    actions: tabActions,
    cases: tabCases,
    settings: tabSettings,
  };
  // Owner sections cover every server the bot serves; they need no selected
  // guild or loaded configuration.
  const ownerSections = { servers: TabOwnerServers, usage: TabDatabaseUsage, "platform-analytics": TabPlatformAnalytics };
  if (Object.hasOwn(ownerSections, state.tab)) return h(ownerSections[state.tab]);
  if (!state.guildId)
    return emptyPanel(
      state.me?.role_discovery_incomplete ? "Server discovery incomplete" : "No servers to manage yet",
      state.me?.role_discovery_incomplete
        ? "Some server access checks could not finish. Retry to check your servers again."
        : state.me?.role_scope_missing
        ? "Sign out and sign in again so jModGuard can check your server roles."
        : "Sign in with an account that has Manage Server permission, or a role a server admin granted Web UI access, in a server using jModGuard.",
    );
  if (!state.config)
    return emptyPanel(
      (state.configRead.activity === "loading") ? "Loading your workspace…" : "Configuration unavailable",
      state.configRead.error || "Fetching saved settings for this server.",
      state.configRead.activity !== "loading" &&
        h(
          ActionButton,
          {
            className: "btn btn-primary",
            onClick: async () => {
              if (await loadConfig()) renderTab();
            },
          },
          "Retry",
        ),
      { role: "status" },
    );
  return h((tabAllowed(state.tab) && components[state.tab]) || tabDashboard);
}
function renderTab() {
  const pending = ui.getSnapshot().pendingDrafts;
  const matching = pending && pending.guildId === state.guildId && pending.tab === state.tab &&
    (!state.config || pending.enrollmentId === state.config.enrollment_id);
  const drafts = matching ? pending.drafts : {};
  ui.set({
    contentRevision: ui.getSnapshot().contentRevision + 1,
    drafts,
    edits: Object.keys(drafts).length > 0 ? "dirty" : "clean",
    pendingDrafts: matching && !state.config ? pending : null,
  });
}

function setBusy(value) {
  ui.set({ activity: value ? "busy" : "idle" });
}
function showView(id) {
  ui.set({ view: id });
}
function setStartupStatus(text) {
  ui.set({ startupStatus: text });
}
function showPreviewUnavailable() {
  ui.set({ view: "startup-view", previewAvailability: "unavailable", startupStatus: "This preview is no longer signed in. Return to the owner dashboard and launch a new preview." });
}
function toast(message, kind = "info") {
  ui.set({ notification: { message, kind } });
  clearTimeout(toastTimer);
  toastTimer = setTimeout(
    () => {
      ui.set({ notification: null });
    },
    kind === "error" ? 5000 : 2600,
  );
}
function closeOverlay() {
  ui.set({ editor: null, edits: "clean" });
}
function editRule(record) {
  if (isCustomRuleId(record.rule_id)) {
    ui.set({ editor: h(CustomRuleEditor, { record, creating: false }) });
    return;
  }
  ui.set({ editor: h(RuleEditor, { record }) });
}
function editCommand(record) {
  ui.set({ editor: h(CommandEditor, { record }) });
}
async function init() {
  try {
    state.me = await waitForApiMe();
    state.guilds = state.me.guilds;
    restoreWorkspace();
    showView("main-view");
    if (await loadConfig()) renderTab();
  } catch (error) {
    if (!handleAuthLoss(error)) {
      showView("startup-view");
      setStartupStatus("Failed to load: " + (error.message || String(error)));
      toast("Failed to load: " + error.message, "error");
    }
  }
}
async function logout() {
  if (!canLeave()) return;
  ui.set({ edits: "clean" });
  stopPrivateWatchers();
  state.guildId = null;
  if (isPreview()) {
    try {
      await api("/api/preview", "DELETE");
    } catch (_) {}
    clearPreviewToken();
    window.location = "/";
    return;
  }
  clearPreviewToken();
  try {
    const response = await fetch(authUrl("/logout"), { method: "POST", credentials: "include" });
    const data = await response.json();
    window.location = data.return_to || "/";
  } catch (_) {
    window.location = "/";
  }
}

let state = {
  me: null,
  guilds: [],
  guildId: null,
  config: null,
  tab: "dashboard",
  configRead: { activity: "idle", error: null },
};

// Request generation fences publication; retained configuration is independent
// of read activity and the most recent read error.
let configRequest = 0;
function ownsConfigRead(request, guildId) {
  return request === configRequest && guildId === state.guildId;
}

const pendingLogChannelActions = new Map();
const logChannelPolls = new Map();
const authLossCleanups = new Set();

const actionReconcileDelays = [1000, 1500, 2500, 5000, 10000, 20000, 40000, 80000, 120000, 150000];

const tabs = [
  "dashboard",
  "analytics",
  "rules",
  "badwords",
  "known-spam",
  "known-bad-images",
  "commands",
  "templates",
  "logchannels",
  "events",
  "members",
  "bans",
  "actions",
  "cases",
  "settings",
  "servers",
  "usage",
  "platform-analytics",
];

let pendingCase = null;

function workspaceKey() {
  return `jmodguard.workspace:${apiBase()}:${state.me.user.id}`;
}

function restoreWorkspace() {
  let saved = {};
  try {
    saved = JSON.parse(window.localStorage.getItem(workspaceKey())) || {};
  } catch (_) {
    /* Storage may be unavailable. */
  }
  state.guildId = state.guilds.find((g) => g.id === saved.guildId)?.id || state.guilds[0]?.id || null;
  state.tab = tabs.includes(saved.tab) && tabAllowed(saved.tab) ? saved.tab : "dashboard";
}

function persistWorkspace() {
  try {
    window.localStorage.setItem(workspaceKey(), JSON.stringify({ guildId: state.guildId, tab: state.tab }));
  } catch (_) {
    /* Session still works without storage. */
  }
}

function canLeave() {
  return ui.getSnapshot().activity !== "busy" && (ui.getSnapshot().edits !== "dirty" || confirm("Discard unsaved changes?"));
}

function markDirty() {
  ui.set({ edits: "dirty" });
}

function currentDrafts() {
  return ui.getSnapshot().drafts;
}

function captureDrafts(savedFields, group) {
  return Object.fromEntries(Object.entries(currentDrafts()).filter(([key, draft]) =>
    savedFields ? !savedFields.includes(key) : !group || draft.group !== group));
}

function retainDrafts(guildId, tab, drafts, enrollmentId) {
  if (guildId !== state.guildId || tab !== state.tab) return;
  if (state.config && enrollmentId !== state.config.enrollment_id) return;
  ui.set({ drafts, edits: Object.keys(drafts).length > 0 ? "dirty" : "clean",
    pendingDrafts: state.config ? null : { guildId, tab, drafts, enrollmentId } });
}

async function handleEvent(handler, event, savedFields, group) {
  if ((ui.getSnapshot().activity === "busy")) return;
  const guildId = state.guildId, tab = state.tab, enrollmentId = state.config?.enrollment_id ?? ui.getSnapshot().pendingDrafts?.enrollmentId;
  const revision = ui.getSnapshot().contentRevision;
  const drafts = captureDrafts(savedFields, group);
  try {
    const result = handler(event);
    if (result && typeof result.then === "function") {
      setBusy(true);
      await result;
      if (revision !== ui.getSnapshot().contentRevision) retainDrafts(guildId, tab, drafts, enrollmentId);
    }
  } catch (e) {
    reportApiError(e);
  } finally {
    if ((ui.getSnapshot().activity === "busy")) setBusy(false);
  }
}

function navigate(tab) {
  if (!canLeave()) return false;
  ui.set({ view: "main-view", pendingDrafts: null, drafts: {}, draftBaselines: {}, copySource: null });
  state.tab = tabs.includes(tab) && tabAllowed(tab) ? tab : "dashboard";
  persistWorkspace();
  renderTab();
  return true;
}

function isBotOwner() {
  return !!(state.me && state.me.is_bot_owner);
}

// Web UI access in the selected server: "manage" (owner or Manage Server),
// or a level a server admin mapped to one of the user's Discord roles. The API
// enforces every level; hiding controls here is only presentation.
function guildAccess() {
  return selectedGuild()?.access || "manage";
}

function canEditSettings() {
  return guildAccess() !== "dashboard";
}

function canManageServer() {
  return guildAccess() === "manage";
}

const settingsTabs = ["rules", "badwords", "known-spam", "known-bad-images", "commands", "templates", "logchannels", "settings"];

function tabAllowed(tab) {
  return canEditSettings() || !settingsTabs.includes(tab);
}

let previewCapability = { kind: "absent" };

function capturedPreviewToken() {
  return previewCapability.kind === "captured" ? previewCapability.token : null;
}

function capturePreviewToken() {
  const fragment = new URLSearchParams(location.hash.replace(/^#/, ""));
  const token = fragment.get("preview");
  if (!token) return;
  // Keep an in-memory source of truth before touching browser storage. A
  // blocked sessionStorage must never silently restore the owner's write access.
  previewCapability = { kind: "captured", token };
  try {
    window.sessionStorage.setItem("jmodguard_preview", token);
  } catch (_) {
    /* The in-memory fallback remains active for this tab. */
  }
  // The tab-bound fragment remains the reload-safe source if storage later
  // disappears. Fragments are not sent in HTTP requests or Referer headers.
}

function currentPreviewToken() {
  try {
    return window.sessionStorage.getItem("jmodguard_preview") || capturedPreviewToken();
  } catch (_) {
    return capturedPreviewToken();
  }
}

function clearPreviewToken() {
  previewCapability = { kind: "absent" };
  try {
    window.sessionStorage.removeItem("jmodguard_preview");
  } catch (_) {}
  const fragment = new URLSearchParams(location.hash.replace(/^#/, ""));
  if (fragment.has("preview")) {
    fragment.delete("preview");
    const hash = fragment.toString();
    try {
      history.replaceState({}, "", location.pathname + location.search + (hash ? `#${hash}` : ""));
    } catch (_) {
      /* The following explicit navigation still drops the fragment. */
    }
  }
}

function leaveExpiredPreview() {
  clearPreviewToken();
  if (typeof window !== "undefined" && window.location) {
    if (typeof window.location.replace === "function") window.location.replace("/");
    else window.location.href = "/";
  }
}

function isPreview() {
  return !!(state.me && state.me.preview);
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

function parseRetryAfterMs(header, capSeconds = 30) {
  if (!header) return 0;
  const seconds = Number(header);
  if (!Number.isFinite(seconds) || seconds < 0) return 0;
  return Math.min(seconds, capSeconds) * 1000;
}

function isDiscordRateLimited(err) {
  return !!err && (err.status === 429 || err.code === "discord_rate_limited");
}

function isUnauthenticated(err) {
  const msg = String(err && err.message);
  return (
    err.status === 401 ||
    err.code === "auth_lost" ||
    msg.includes("discord authorization failed") ||
    msg.includes("not authenticated") ||
    msg === "session expired" ||
    msg.includes("401")
  );
}

function stopPrivateWatchers() {
  analyticsCache.clear();
  for (const dispose of authLossCleanups) dispose();
  for (const key of pendingLogChannelActions.keys()) clearPendingLogChannelEntry(key);
}

function handleAuthLoss(error) {
  if (!isUnauthenticated(error)) return false;
  stopPrivateWatchers();
  if (currentPreviewToken()) {
    showPreviewUnavailable();
    return true;
  }
  if (ui.getSnapshot().view !== "login-view") {
    state.me = null;
    state.guilds = [];
    state.guildId = null;
    showView("login-view");
  }
  return true;
}

function reportApiError(error) {
  if (!handleAuthLoss(error)) toast(error.message || String(error), error.code === "save_pending" ? "info" : "error");
}

function isApiWarmup(err) {
  return err.status === 503 && String(err && err.message) === "not ready";
}

function isOriginStarting(err) {
  if (isDiscordRateLimited(err)) return false;
  if (isApiWarmup(err)) return true;
  if (!err || !err.status) return true;
  if (err.status === 502 || err.status === 504) return true;
  return err.status === 503 && String(err && err.message) === "HTTP 503";
}

const API_WARMUP_BUDGET_MS = 8 * 60 * 1000;

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

function returnFromPreview() {
  clearPreviewToken();
}

function pageEnrollment(path, method = "GET") {
  // Bind edits and scoped reads to the membership this page loaded. Never
  // fetch a new token or retry against a replacement installation.
  const edit = path.match(
    /^\/api\/guilds\/(\d+)\/(settings|rules|badwords|testbadwords|known-spam|known-bad-images|commands|templates|copy|logchannels|logfilters|unban|actions|events|members|member-sync|bans|stats|analytics|cases|case-delete-roles|access-roles)(?:\/|\?|$)/,
  );
  if (
    edit &&
    (method !== "GET" ||
      [
        "rules",
        "badwords",
        "known-spam",
        "known-bad-images",
        "commands",
        "templates",
        "logchannels",
        "actions",
        "events",
        "members",
        "member-sync",
        "bans",
        "stats",
        "analytics",
        "cases",
        "case-delete-roles",
        "access-roles",
      ].includes(edit[2])) &&
    edit[1] !== "0" &&
    edit[1] === state.guildId &&
    state.config?.enrollment_id
  ) {
    return state.config.enrollment_id;
  }
  return null;
}

function credentialedOpts(path, method) {
  // credentials: include so cross-origin CDN SPA (JMODGUARD_API_BASE) sends the session cookie.
  const opts = { method, headers: {}, credentials: "include", cache: "no-store" };
  const enrollment = pageEnrollment(path, method);
  if (enrollment) opts.headers["x-jmodguard-enrollment"] = enrollment;
  const preview = currentPreviewToken();
  if (preview) opts.headers["x-jmodguard-preview"] = preview;
  return opts;
}

function throwApiFailure(data, status, retryAfterMs) {
  const msg = data && (data.error || data.message) ? data.error || data.message : `HTTP ${status}`;
  const error = apiError(msg, status, retryAfterMs);
  error.code = data && data.code;
  if (error.code === "preview_expired") leaveExpiredPreview();
  else handleAuthLoss(error);
  throw error;
}

async function api(path, method = "GET", body = null, signal, reconcileRecovered) {
  const save = ["POST", "PUT"].includes(method) &&
    /^\/api\/guilds\/\d+\/(settings|rules|badwords|known-spam|known-bad-images|commands|templates|copy|logfilters|case-delete-roles|access-roles)(?:\/|$)/.test(path) &&
    (method !== "POST" || body !== null);
  if (save) return submitSave(path, method, body, signal, reconcileRecovered);
  return apiRequest(path, method, body, signal);
}

function canonicalSaveBody(value) {
  if (Array.isArray(value)) return value.map(canonicalSaveBody);
  if (value && typeof value === "object") {
    return Object.fromEntries(Object.keys(value).sort().map(key => [key, canonicalSaveBody(value[key])]));
  }
  return value;
}

// Failed/pending requests retain identity; only the existing explicit
// expiry or reviewed-unknown paths may discard it.
function saveFailureState(error) {
  if (error.code === "save_expired") return "expired";
  if (error.code === "save_unknown") return "unknown";
  if (error.code === "save_pending") return "pending";
  return "unresolved";
}

async function submitSave(path, method, body, signal, reconcileRecovered) {
  // A remount reads the current role revision while retaining an unresolved
  // role draft. Match that intent by its roles, then look up the captured revision.
  const roleSave = method === "PUT" && /^\/api\/guilds\/\d+\/(case-delete-roles|access-roles)$/.test(path);
  const canonical = canonicalSaveBody(roleSave ? { roles: body.roles } : body);
  const key = `${workspaceKey()}:save:${method}:${path}:${JSON.stringify(canonical)}`;
  const enrollment = pageEnrollment(path, method);
  let raw, saved;
  /** @type {"submit" | "lookup"} */
  let saveMode;
  try {
    raw = window.sessionStorage.getItem(key);
    saveMode = raw !== null ? "lookup" : "submit";
  } catch (_) {
    throw new Error("Browser session storage must be available and valid to safely save changes.");
  }
  // Only a new save needs the initial read. An unresolved save already owns
  // its original revision and must remain lookup-only while that read is pending.
  if (roleSave && saveMode === "submit" && !body.revision) {
    throw new Error("Roles have not loaded yet. Wait for them to load before saving.");
  }
  try {
    saved = raw ? JSON.parse(raw) : {
      id: (crypto.randomUUID() + crypto.randomUUID()).replaceAll("-", ""),
      enrollment,
      ...(roleSave ? { revision: body.revision } : {}),
    };
    window.sessionStorage.setItem(key, JSON.stringify(saved));
  } catch (_) {
    throw new Error("Browser session storage must be available and valid to safely save changes.");
  }
  if (!saved || !/^[0-9a-f]{64}$/.test(saved.id) || saved.enrollment !== enrollment ||
    (roleSave && typeof saved.revision !== "string")) {
    if (confirm("This saved request belongs to an earlier page membership. Check its outcome before clearing it. Clear the saved request without submitting another save?")) {
      window.sessionStorage.removeItem(key);
    }
    throw new Error("Saved request was not submitted. Reload and review this server before saving again.");
  }
  try {
    const capturedBody = roleSave ? { ...body, revision: saved.revision } : body;
    let result = await apiRequest(path, method, capturedBody, signal, {
      "x-jmodguard-save": saved.id,
      "x-jmodguard-save-lookup": saveMode === "lookup" ? "1" : "0",
    });
    // Specialized editors also consume fields outside config_patch. Keep the
    // identity until their current-state read succeeds, so a failed read retries
    // the acknowledgement without submitting another mutation.
    if (saveMode === "lookup" && reconcileRecovered) result = await reconcileRecovered();
    try { window.sessionStorage.removeItem(key); } catch (_) { /* A retained ID only queries its result. */ }
    // The page may already include this commit, or a newer edit. Reconcile
    // current configuration instead of applying a historical patch again.
    return saveMode === "lookup" ? { ...result, config_patch: null } : result;
  } catch (error) {
    // A lookup-only attempt never submits a new save, whether or not the server answered it.
    if (saveMode === "lookup" && error && typeof error === "object") error.saveLookup = true;
    if (saveFailureState(error) === "expired") {
      try { window.sessionStorage.removeItem(key); } catch (_) { /* Keep lookup-only behavior if storage fails. */ }
    }
    if (saveFailureState(error) === "unknown" && confirm("The earlier save may already have completed, but its result is unavailable. Have you checked the current configuration and want to clear this saved request? This will not submit another save.")) {
      window.sessionStorage.removeItem(key);
      throw new Error("Saved request cleared. Review the current configuration before saving again.");
    }
    throw error;
  }
}

async function apiRequest(path, method = "GET", body = null, signal, headers = {}) {
  const opts = credentialedOpts(path, method);
  Object.assign(opts.headers, headers);
  const controller = signal ? null : new AbortController();
  const timeout = controller ? setTimeout(() => controller.abort(), 30000) : null;
  opts.signal = signal || controller.signal;
  try {
    if (body !== null) {
      opts.headers["content-type"] = "application/json";
      opts.body = JSON.stringify(body);
    }
    const res = await fetch(apiUrl(path), opts);
    const text = await res.text();
    let data = null;
    try {
      data = text ? JSON.parse(text) : null;
    } catch (_) {
      data = text;
    }
    if (!res.ok || data?.code === "save_pending") throwApiFailure(data, res.status, parseRetryAfterMs(res.headers.get("retry-after"), Number.MAX_SAFE_INTEGER / 1000));
    return data;
  } catch (e) {
    if (e && (e.name === "AbortError" || e.name === "TimeoutError")) {
      throw apiError(
        signal ? "Failed to fetch" : "Request timed out. Refresh to verify saved state before trying again.",
      );
    }
    throw e;
  } finally {
    if (timeout !== null) clearTimeout(timeout);
  }
}

function apiRead(path, signal) {
  return api(path, "POST", null, signal);
}

async function downloadPrivate(path, filename) {
  const opts = credentialedOpts(path, "GET");
  const res = await fetch(apiUrl(path), opts);
  if (!res.ok) {
    let data = null;
    try {
      data = JSON.parse(await res.text());
    } catch (_) {}
    throwApiFailure(data, res.status);
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

// Receipt progress is distinct from reconciliation of the saved mapping.
function botActionPhase(job) {
  if (["pending", "claimed", "running"].includes(job.state)) return "active";
  if (job.state === "succeeded") return "succeeded";
  if (["failed", "uncertain", "cancelled", "expired"].includes(job.state)) return "terminal";
  return "invalid";
}

async function submitBotAction(path, method, body) {
  const canonicalBody = Object.fromEntries(
    Object.keys(body)
      .sort()
      .map((key) => [key, body[key]]),
  );
  const key = `${workspaceKey()}:action:${method}:${path}:${JSON.stringify(canonicalBody)}`;
  const enrollmentId = state.config?.enrollment_id;
  const guildId = path.split("/")[3];
  if (!enrollmentId || guildId !== state.guildId)
    throw new Error("Reload this server before requesting an action.");
  let saved;
  try {
    const raw = window.sessionStorage.getItem(key);
    saved = raw
      ? JSON.parse(raw)
      : {
          request_id: `${Math.floor(Date.now() / 1000)}_${crypto.randomUUID()}`,
          enrollment_id: enrollmentId,
        };
    window.sessionStorage.setItem(key, JSON.stringify(saved));
  } catch (_) {
    throw new Error("Browser session storage must be available and valid to safely submit bot actions.");
  }
  if (!saved || typeof saved.request_id !== "string" || saved.enrollment_id !== enrollmentId) {
    if (
      confirm(
        "This saved request belongs to an earlier bot membership. Check Discord for its outcome. Clear the saved request without submitting another action?",
      )
    ) {
      window.sessionStorage.removeItem(key);
    }
    throw new Error(
      "Saved request was not submitted. Review the current server before making a new request.",
    );
  }
  const requestId = saved.request_id;
  let job;
  try {
    job = await api(path, method, { ...body, request_id: requestId });
  } catch (error) {
    if (
      ((error.status === 410 && error.code === "action_request_expired") ||
        (error.status === 400 && error.code === "action_request_invalid") ||
        (error.status === 409 && error.code === "action_enrollment_changed")) &&
      confirm(
        "The earlier action may already have run, and its result may no longer be listed. Have you checked Bot actions and Discord, and want to clear this saved request? This will not submit another action.",
      )
    ) {
      window.sessionStorage.removeItem(key);
      throw new Error(
        "Saved request cleared. Review the current state, then submit again only if another action is needed.",
      );
    }
    throw error;
  }
  if (
    !job ||
    typeof job.id !== "string" ||
    job.guild_id !== guildId ||
    job.actor_id !== state.me.user.id ||
    job.enrollment_id !== enrollmentId ||
    botActionPhase(job) === "invalid"
  ) {
    throw new Error(
      "Action acknowledgment was invalid. Check Bot actions; retrying here will reuse the same request ID.",
    );
  }
  try {
    window.sessionStorage.removeItem(key);
  } catch (_) {
    /* Retaining the same ID is safe. */
  }
  const completed = job.state === "succeeded";
  const waiting = botActionPhase(job) === "active";
  toast(
    completed
      ? "Bot action completed. See Bot actions for details."
      : waiting
        ? "Action queued. Check Bot actions for the result."
        : `Action ${job.state}: ${job.outcome || "Check Bot actions for details."}`,
    completed ? "success" : waiting ? "info" : "error",
  );
  return job;
}

function logChannelRow(event) {
  return state.config?.log_channels?.find((row) => row.event === event);
}

function logChannelActionKey(job) {
  return `${job.guild_id}:${job.enrollment_id}:${job.action.event}`;
}

function clearPendingLogChannelEntry(key) {
  const entry = pendingLogChannelActions.get(key);
  pendingLogChannelActions.delete(key);
  if (!entry) return;
  const groupKey = logChannelPollKey(entry.job);
  if (![...pendingLogChannelActions.values()].some(item => logChannelPollKey(item.job) === groupKey)) {
    clearTimeout(logChannelPolls.get(groupKey)?.timer);
    logChannelPolls.delete(groupKey);
  }
}

function overlayPendingLogChannels(config, guildId) {
  if (!config?.enrollment_id) return [];
  const overlaid = [];
  for (const [key, entry] of pendingLogChannelActions) {
    const job = entry.job;
    if (job.guild_id !== guildId || job.enrollment_id !== config.enrollment_id) continue;
    const row = config.log_channels?.find((candidate) => candidate.event === job.action.event);
    if (!row) continue;
    row.effective = entry.requested;
    row.guild_override = entry.requested;
    row.pending_action = { id: job.id, state: job.state, previous: entry.previous };
    overlaid.push({ key, entry });
  }
  return overlaid;
}

function renderLogChannelsPreservingDrafts() {
  if (state.tab !== "logchannels") return;
  ui.set({ pendingDrafts: {
    guildId: state.guildId, tab: state.tab, drafts: currentDrafts(), enrollmentId: state.config?.enrollment_id,
  } });
  renderTab();
}

function applyQueuedLogChannel(job) {
  if (
    !actionIsLogChannel(job) ||
    job.guild_id !== state.guildId ||
    job.enrollment_id !== state.config?.enrollment_id
  )
    return false;
  const row = logChannelRow(job.action.event);
  if (!row) return false;
  const active = botActionPhase(job) === "active";
  const terminal = ["succeeded", "terminal"].includes(botActionPhase(job));
  if (!active && !terminal) return false;
  const key = logChannelActionKey(job);
  const existing = pendingLogChannelActions.get(key);
  if (
    existing &&
    existing.job.id !== job.id &&
    terminal &&
    botActionPhase(existing.job) === "active"
  ) {
    // A retained request key can replay an older terminal receipt after a
    // newer request has become this row's active owner. Keep watching newer.
    return true;
  }
  const previous = existing?.previous ||
    row.pending_action?.previous || {
      effective: row.effective ?? null,
      guild_override: row.guild_override ?? null,
    };
  const requested = job.action.channel_id ?? null;
  row.effective = requested;
  row.guild_override = requested;
  if (job.state === "succeeded") {
    clearPendingLogChannelEntry(key);
    row.pending_action = null;
  } else {
    const entry = {
      job,
      requested,
      previous,
    };
    pendingLogChannelActions.set(key, entry);
    const poll = logChannelPolls.get(logChannelPollKey(job));
    if (poll && existing?.job.id !== job.id) {
      poll.attempt = 0;
      ++poll.admission;
      poll.dueAt = Math.max(poll.dueAt, Date.now() + actionReconcileDelays[0]);
    }
    row.pending_action = { id: job.id, state: job.state, previous };
  }
  // A snapshot started before this acknowledged request must not replace its
  // queued representation after the button has already returned success.
  ++configRequest;
  state.configRead.activity = "idle";
  state.configRead.error = null;
  return true;
}

function logChannelPollKey(job) {
  return `${job.guild_id}:${job.enrollment_id}`;
}

function logChannelPollVisible(job) {
  return document.visibilityState === "visible" && state.tab === "logchannels" &&
    state.guildId === job.guild_id && state.config?.enrollment_id === job.enrollment_id;
}

function pauseLogChannelPolls(invalidate = false) {
  for (const poll of logChannelPolls.values()) {
    clearTimeout(poll.timer);
    poll.timer = null;
    if (invalidate) ++poll.generation;
  }
}

function resumeLogChannelPolls() {
  for (const { job } of pendingLogChannelActions.values()) scheduleLogChannelReconcile(job);
}

function scheduleLogChannelReconcile(job) {
  const key = logChannelPollKey(job);
  let poll = logChannelPolls.get(key);
  if (!poll) {
    poll = { timer: null, dueAt: Date.now() + actionReconcileDelays[0], retryUntil: 0, attempt: 0, flight: { kind: "idle" }, generation: 0, admission: 0 };
    logChannelPolls.set(key, poll);
  }
  if (poll.timer !== null || poll.flight.kind === "running" || !logChannelPollVisible(job) ||
    poll.attempt >= actionReconcileDelays.length) return;
  poll.timer = setTimeout(() => {
    poll.timer = null;
    if (!logChannelPollVisible(job)) return;
    if (Date.now() < poll.dueAt) return scheduleLogChannelReconcile(job);
    const current = [...pendingLogChannelActions.values()].find(item => logChannelPollKey(item.job) === key);
    if (current) return reconcileQueuedLogChannel(current.job);
  }, Math.min(Math.max(0, poll.dueAt - Date.now()), 2147483647));
}

async function reconcileQueuedLogChannel(original) {
  const entry = pendingLogChannelActions.get(logChannelActionKey(original));
  if (!entry || entry.job.id !== original.id || original.guild_id !== state.guildId ||
    original.enrollment_id !== state.config?.enrollment_id || document.visibilityState !== "visible") return;
  const key = logChannelPollKey(original);
  scheduleLogChannelReconcile(original);
  const poll = logChannelPolls.get(key);
  if (poll.flight.kind === "running") return poll.flight.promise;
  if (Date.now() < poll.retryUntil || ((poll.attempt > 0 || poll.admission > 0) && Date.now() < poll.dueAt)) return;
  clearTimeout(poll.timer);
  poll.timer = null;
  const generation = poll.generation;
  const admission = poll.admission;
  const current = () => logChannelPolls.get(key) === poll && poll.generation === generation;
  poll.flight = { kind: "running", promise: (async () => {
    let retryUntil = 0;
    try {
      const entries = [...pendingLogChannelActions.values()].filter(item => logChannelPollKey(item.job) === key);
      const jobs = entries.some(item => botActionPhase(item.job) === "active")
        ? await apiRead(`/api/guilds/${original.guild_id}/actions`) : [];
      for (const item of entries) {
        if (!current() || document.visibilityState !== "visible") break;
        try {
          await reconcileLogChannelEntry(item.job, jobs, current);
        } catch (error) {
          if (isUnauthenticated(error)) throw error;
          // One unavailable mapping must not strand other terminal receipts.
          retryUntil = Math.max(retryUntil, Date.now() + (error.retryAfterMs || 0));
        }
      }
    } catch (error) {
      if (isUnauthenticated(error)) return;
      retryUntil = Math.max(retryUntil, Date.now() + (error.retryAfterMs || 0));
    } finally {
      poll.flight = { kind: "idle" };
      if (logChannelPolls.get(key) === poll) {
        if (poll.admission === admission) ++poll.attempt;
        poll.retryUntil = retryUntil;
        poll.dueAt = Math.max(Date.now() + (actionReconcileDelays[poll.attempt] || 0), retryUntil);
        if (poll.attempt >= actionReconcileDelays.length && current())
          toast("Could not confirm the log-channel result. See Bot actions for details.", "info");
        else scheduleLogChannelReconcile(original);
      }
    }
  })() };
  return poll.flight.promise;
}

async function reconcileLogChannelEntry(original, jobs, current) {
  const key = logChannelActionKey(original);
  let entry = pendingLogChannelActions.get(key);
  let row = logChannelRow(original.action.event);
  if (
    !current() ||
    !entry ||
    entry.job.id !== original.id ||
    !row?.pending_action ||
    row.pending_action.id !== original.id ||
    original.guild_id !== state.guildId ||
    original.enrollment_id !== state.config?.enrollment_id
  )
    return;
  const originalTerminal = ["succeeded", "terminal"].includes(botActionPhase(original));
  let job = original;
  if (!originalTerminal) {
    job = jobs.find((candidate) => candidate.id === original.id);
    entry = pendingLogChannelActions.get(key);
    row = logChannelRow(original.action.event);
    if (
      !current() ||
      !entry ||
      entry.job.id !== original.id ||
      !row?.pending_action ||
      row.pending_action.id !== original.id ||
      original.guild_id !== state.guildId ||
      original.enrollment_id !== state.config?.enrollment_id
    )
      return;
  }
  if (!job || botActionPhase(job) === "active") {
    if (job) {
      entry.job = job;
      row.pending_action.state = job.state;
    }
    return;
  }
  // A newer request for this event owns the row now; never let an older
  // terminal receipt overwrite or roll it back.
  if (row.pending_action?.id !== original.id) return;
  entry.job = job;
  row.pending_action.state = job.state;
  if (job.state === "succeeded") {
    clearPendingLogChannelEntry(key);
    row.pending_action = null;
    renderLogChannelsPreservingDrafts();
    toast("Log channel saved", "success");
    return;
  }

  let authoritative;
  try {
    authoritative = await apiRead(
      `/api/guilds/${original.guild_id}/logchannels/${encodeURIComponent(original.action.event)}`,
    );
  } catch (error) {
    if (isUnauthenticated(error)) return;
    if (current()) renderLogChannelsPreservingDrafts();
    throw error;
  }
  entry = pendingLogChannelActions.get(key);
  row = logChannelRow(original.action.event);
  if (
    !current() ||
    !entry ||
    entry.job.id !== original.id ||
    !row?.pending_action ||
    original.guild_id !== state.guildId ||
    original.enrollment_id !== state.config?.enrollment_id
  )
    return;
  if (row.pending_action?.id !== original.id) return;
  row.effective = authoritative.effective;
  row.guild_override = authoritative.guild_override;
  clearPendingLogChannelEntry(key);
  row.pending_action = null;
  renderLogChannelsPreservingDrafts();
  toast(`Log-channel action ${job.state}: ${job.outcome || "the saved mapping was not changed"}`, "error");
}

async function waitForApiMe() {
  let warmup = { kind: "unused" };
  let reachability = { kind: "unused" };
  let delay = 500;
  let startupDisplay = "unshown";
  while (true) {
    const attemptStarted = Date.now();
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), ORIGIN_REACHABILITY_BUDGET_MS);
    try {
      const me = await apiRead("/api/me", controller.signal);
      return me;
    } catch (e) {
      if (!isOriginStarting(e)) throw e;
      const now = Date.now();
      if (isApiWarmup(e)) {
        reachability = { kind: "unused" };
        if (warmup.kind === "unused") warmup = { kind: "running", startedAt: attemptStarted };
        if (now - warmup.startedAt >= API_WARMUP_BUDGET_MS) {
          throw apiError(originGaveUpMessage(e), e.status || 0, 0);
        }
      } else {
        if (reachability.kind === "unused") reachability = { kind: "running", startedAt: attemptStarted };
        if (now - reachability.startedAt >= ORIGIN_REACHABILITY_BUDGET_MS) {
          throw apiError(originGaveUpMessage(e), e.status || 0, 0);
        }
      }
      if (startupDisplay === "unshown") {
        startupDisplay = "shown";
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

async function loadConfig() {
  const guildId = state.guildId;
  const request = ++configRequest;
  if (!guildId) {
    renderTab();
    return;
  }
  state.configRead.activity = "loading";
  state.configRead.error = null;
  if (!state.config) renderTab();
  try {
    const config = await apiRead(`/api/guilds/${guildId}/config`);
    if (!ownsConfigRead(request, guildId)) return;
    if (state.config && state.config.enrollment_id !== config.enrollment_id) {
      ui.set({ pendingDrafts: null, drafts: {}, draftBaselines: {}, copySource: null, edits: "clean" });
      for (const [key, entry] of pendingLogChannelActions) {
        if (entry.job.guild_id === guildId && entry.job.enrollment_id !== config.enrollment_id) {
          clearPendingLogChannelEntry(key);
        }
      }
      toast("Bot membership changed. Configuration reloaded; review it before making changes.", "info");
    }
    state.config = config;
    for (const { entry } of overlayPendingLogChannels(config, guildId)) scheduleLogChannelReconcile(entry.job);
    return true;
  } catch (e) {
    if (!ownsConfigRead(request, guildId)) return;
    if (isUnauthenticated(e)) return;
    state.config = null;
    ui.set({ editor: null });
    state.configRead.error = e.message || String(e);
    toast("Could not refresh configuration: " + state.configRead.error, "error");
  } finally {
    if (request === configRequest) {
      state.configRead.activity = "idle";
      if (state.configRead.error) renderTab();
    }
  }
}

function selectedGuild() {
  return state.guilds.find((g) => g.id === state.guildId);
}

function applyConfigPatch(res) {
  const patch = res?.config_patch;
  const config = state.config;
  if (!patch || !config) return false;
  // Any snapshot requested before this acknowledged commit is now stale.
  ++configRequest;
  state.configRead.activity = "idle";
  state.configRead.error = null;
  if (patch.kind === "rule") {
    const custom = isCustomRuleId(patch.id);
    if (custom && patch.value == null) {
      config.rules = (config.rules || []).filter((rule) => rule.rule_id !== patch.id);
      return true;
    }
    let row = config.rules?.find((rule) => rule.rule_id === patch.id);
    if (!row && custom && patch.value) {
      config.rules ||= [];
      config.rules.push({
        rule_id: patch.id,
        name: patch.value.name,
        description: patch.value.description || "",
        effective: patch.value,
        guild_override: patch.value,
        global: null,
      });
      return true;
    }
    if (!row || (patch.value == null && (patch.scope === "global" || row.global == null))) return false;
    if (patch.scope === "global") {
      row.global = patch.value;
      if (patch.cleared_guild_override) row.guild_override = null;
      if (!row.guild_override) row.effective = patch.value;
    } else {
      row.guild_override = patch.value;
      row.effective = patch.value ?? row.global;
    }
    if (custom) row.name = row.effective.name;
    return true;
  }
  if (patch.kind === "badword") {
    if (!Array.isArray(config.badwords)) return false;
    if (patch.operation === "add" && patch.value) config.badwords.push(patch.value);
    // Batch acknowledgements contain only IDs. Reload authoritative rows/notes.
    else if (patch.operation === "add_many") return false;
    else if (patch.operation === "delete")
      config.badwords = config.badwords.filter((word) => word.id !== patch.id);
    else return false;
    return true;
  }
  if (patch.kind === "command" && patch.value) {
    config.commands ||= [];
    let row = config.commands.find((command) => command.name === patch.name);
    if (!row) {
      row = {
        name: patch.name,
        scope: patch.scope,
        effective: patch.value,
        guild_override: null,
        global: null,
      };
      config.commands.push(row);
    }
    if (patch.scope === "global") {
      row.global = patch.value;
      if (!row.guild_override) {
        row.scope = "global";
        row.effective = patch.value;
      }
    } else {
      row.scope = "guild";
      row.guild_override = patch.value;
      row.effective = patch.value;
    }
    return true;
  }
  if (patch.kind === "template") {
    const row = config.templates?.find((template) => template.event === patch.event);
    if (!row || (patch.value == null && (patch.scope === "global" || row.global == null))) return false;
    if (patch.scope === "global") {
      row.global = patch.value;
      if (row.guild_override == null) row.effective = patch.value;
    } else {
      row.guild_override = patch.value;
      row.effective = patch.value ?? row.global;
    }
    return true;
  }
  if (patch.kind === "settings" && !patch.sync_nickname && patch.settings) {
    if (patch.bot_name_present && !patch.nickname_reconcile) return false;
    const fields = [
      ["prefix", patch.prefix_present],
      ["bot_name", patch.bot_name_present],
    ];
    if (patch.scope === "global") {
      config.global_settings = patch.settings;
      config.global_settings_override ||= {};
      for (const [field, present] of fields) {
        if (!present) continue;
        config.global_settings_override[field] = patch[field] ?? null;
        if (config.settings_override?.[field] == null) config.settings[field] = patch.settings[field];
      }
    } else {
      config.settings = patch.settings;
      config.settings_override ||= {};
      for (const [field, present] of fields) {
        if (!present) continue;
        const parent = config.global_settings?.[field];
        config.settings_override[field] =
          patch[field] == null || patch.settings[field] === parent ? null : patch.settings[field];
      }
      if (patch.bot_name_present) config.nickname_reconcile = patch.nickname_reconcile;
    }
    return true;
  }
  return false;
}

async function reloadAfter(work, onSuccess) {
  const res = await work();
  if (!applyConfigPatch(res) && !(await loadConfig())) return;
  if (onSuccess) onSuccess(res);
  renderTab();
}

function emptyPanel(title, body, action, attrs) {
  return h(
    "div",
    { className: "hero bg-base-200 rounded-box", ...(attrs || {}) },
    h(
      "div",
      { className: "hero-content" },
      h("div", { className: "hero-copy" }, h("h2", {}, title), h("p", {}, body), action || null),
    ),
  );
}

function field(label, ...children) {
  return h(Field, { className: "fieldset field" }, h(Label, { className: "fieldset-legend" }, label), ...children);
}

function eventHeading(event) {
  return h(
    "div",
    { className: "event-heading" },
    h(
      "h3",
      { className: "card-title" },
      event.replaceAll("_", " ").replace(/^./, (c) => c.toUpperCase()),
    ),
    h("code", { className: "muted" }, event),
  );
}

// One header line per page (#590): the title, an optional muted detail and actions.
function pageHeader(title, detail, ...actions) {
  return h(
    "header",
    { className: "page-header" },
    h("h2", {}, title),
    detail ? h("span", { className: "page-header-detail muted" }, detail) : null,
    actions.length ? h("div", { className: "page-header-actions" }, ...actions) : null,
  );
}

// Explanatory text behind a keyboard-focusable info button. The text is the
// button's accessible description; it shows on hover or focus. The pointer can
// move onto the text (it sits inside the hover area), and Escape hides it
// wherever focus is, so hover-opened text is dismissible too (WCAG 1.4.13).
function InfoTip({ id, label, children }) {
  const [open, setOpen] = useState(false);
  useEffect(() => {
    if (!open) return undefined;
    const dismiss = (event) => {
      if (event.key === "Escape") setOpen(false);
    };
    document.addEventListener("keydown", dismiss);
    return () => document.removeEventListener("keydown", dismiss);
  }, [open]);
  return h(
    "span",
    {
      className: "info-tip",
      onMouseEnter: () => setOpen(true),
      onMouseLeave: () => setOpen(false),
    },
    h(
      "button",
      {
        type: "button",
        className: "info-tip-trigger",
        "aria-label": label,
        "aria-describedby": id,
        "aria-expanded": open,
        onFocus: () => setOpen(true),
        onBlur: () => setOpen(false),
        onClick: () => setOpen(true),
      },
      h("span", { "aria-hidden": "true" }, "ⓘ"),
    ),
    h(
      "span",
      { id, role: "tooltip", className: "info-tip-text", hidden: !open },
      // An array is one short idea per line (#620).
      Array.isArray(children) ? children.map((line, index) => h("span", { key: index, className: "info-tip-line" }, line)) : children,
    ),
  );
}

// Short notes a moderator may want, collapsed by default (#620): one idea per line.
function GoodToKnow({ items, summary = "Good to know" }) {
  return h(
    "details",
    { className: "good-to-know" },
    h("summary", null, summary),
    h("ul", null, ...items.filter(Boolean).map((item, index) => h("li", { key: index }, item))),
  );
}

// Getting started is complete when the loaded configuration already has at least
// one enabled automod rule, one enabled command and one routed log channel: one
// per setup step. Missing collections count as incomplete, so the guide stays.
function setupComplete(cfg) {
  const enabled = (records) => (records ?? []).some((record) => record?.effective?.enabled);
  const routed = (cfg?.log_channels ?? []).some((entry) => entry?.effective && String(entry.effective) !== "0");
  return enabled(cfg?.rules) && enabled(cfg?.commands) && routed;
}

const MODERATION_COUNTS_HELP =
  "Moderation records exclude routine history and older plan summaries whose action type was not recorded. Those summaries remain in the Audit Log.";

function tabDashboard() {
  const guildId = useRef(state.guildId).current;
  const g = selectedGuild();
  const cfg = state.config;
  const rules = cfg.rules ?? [];
  const commands = cfg.commands ?? [];
  const rulesOn = rules.filter((r) => r.effective?.enabled).length;
  const rulesDry = rules.filter((r) => r.effective?.dry_run).length;
  const commandsOn = commands.filter((c) => c.effective?.enabled).length;
  const chans = (cfg.log_channels ?? []).filter((l) => l.effective && String(l.effective) !== "0").length;
  const result = useApiRead(`/api/guilds/${guildId}/stats`);
  const count = (key) =>
    result.loading ? "Loading…" : result.data?.[key] == null ? "Unavailable" : String(result.data[key]);
  return h(
    "div",
    { className: "contents" },
    pageHeader(
      "Dashboard",
      g?.name,
      h(ActionButton, { className: "btn btn-sm btn-primary", onClick: () => navigate("rules") }, "Review protection"),
      h(ActionButton, { className: "btn btn-sm btn-outline", onClick: () => navigate("events") }, "View audit log"),
    ),
    !setupComplete(cfg) &&
      h(
        "section",
        { className: "card bg-base-100 card-border getting-started" },
        h(
          "div",
          { className: "card-body" },
          h("h3", { className: "card-title" }, "Getting started"),
          h(
            "div",
            { className: "setup-links" },
            ...[
              ["01", "Tune automod", "Review rules and test them in dry-run.", "rules"],
              ["02", "Configure commands", "Choose commands and moderator permissions.", "commands"],
              ["03", "Route your logs", "Connect event reports to Discord channels.", "logchannels"],
            ].map(([number, title, description, tab]) =>
              h(
                ActionButton,
                { className: "btn setup-link", onClick: () => navigate(tab) },
                h("span", { className: "setup-number" }, number),
                h("span", {}, h("strong", {}, title), h("span", {}, description)),
                // The sidebar icon of the page the step opens, where the decorative arrow was.
                h(Icon, { name: NAV_ICONS[tab].icon, intent: NAV_ICONS[tab].intent }),
              ),
            ),
          ),
          h(
            "p",
            { className: "setup-note" },
            "Save in each editor. The bot picks up changes within a few seconds." +
              (isBotOwner() ? " As bot owner, you can also edit Global defaults." : ""),
          ),
        ),
      ),
    h(
      "section",
      { className: "dashboard-section", "aria-labelledby": "dashboard-overview" },
      h("h3", { className: "card-title", id: "dashboard-overview" }, "Overview"),
      h(
        "div",
        { className: "stats bg-base-100 w-full dashboard-stats" },
        // One row each for protection, moderation activity and server configuration.
        stat("Rules enabled", `${rulesOn} / ${rules.length}`, "rules-enabled"),
        stat("Rules in dry-run", rulesDry, "rules-dry-run"),
        stat("Commands enabled", `${commandsOn} / ${commands.length}`, "commands-enabled"),
        stat("Log channels", chans, "log-channels"),
        stat(
          ["Moderation records", h(InfoTip, { id: "moderation-counts-help", label: "About these counts" }, MODERATION_COUNTS_HELP)],
          count("events"),
          "moderation-records",
        ),
        stat("Warns", count("warns"), "warns"),
        stat("Bans", count("bans"), "bans"),
        stat("Cases", count("cases"), "cases"),
        stat("Prefix", cfg.settings?.prefix, "prefix"),
        stat("Bot name", cfg.settings?.bot_name || "default", "bot-name"),
        stat("Badword patterns", (cfg.badwords ?? []).length, "badword-patterns"),
        stat("Routine chat collection", "Off", "routine-chat-collection"),
      ),
    ),
  );
}

// Dashboard tiles pass a DASHBOARD_ICONS key; the decorative icon leads the label (#608).
function stat(label, value, icon) {
  const tile = icon && DASHBOARD_ICONS[icon];
  return h(
    "div",
    { className: "stat" },
    h("div", { className: "stat-title" }, tile && h(Icon, { name: tile.icon, intent: tile.intent }), ...[label].flat()),
    h("div", { className: "stat-value" }, value),
  );
}

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

const ANALYTICS_HELP = [
  "Counts come from what the bot recorded, by UTC day. Nothing is fetched from Discord.",
  "Days before the bot started, or while it was offline, show zero.",
  "Each change compares with the previous period of the same length.",
  "Members has no 90-day change: join and leave counts are kept for 92 days.",
  "Some numbers overlap (a warn is also a mod action), so they're never added up.",
  "The heat map counts each automod action, moderator command and spam ban once.",
  "Members is partial while the first import runs.",
  "If Members shows unverified, the Members page says whether to Reconcile or wait for it to clear.",
];

// Loaded analytics summaries per guild, page membership and range. Switching
// back to a loaded range, or returning to the tab, reuses it without a request.
const ANALYTICS_CLIENT_TTL_MS = 60000;
const analyticsCache = new Map();

function analyticsKey(guildId, days) {
  return `${guildId}|${state.config?.enrollment_id ?? ""}|${days}`;
}

function cachedAnalytics(key) {
  const hit = analyticsCache.get(key);
  if (hit && Date.now() - hit.at < ANALYTICS_CLIENT_TTL_MS) return hit.data;
  if (hit) analyticsCache.delete(key);
  return null;
}

function useAnalyticsSummary(guildId, days) {
  const key = analyticsKey(guildId, days);
  const [result, setResult] = useState(() => ({ key, data: cachedAnalytics(key), error: null }));
  useEffect(() => {
    const cached = cachedAnalytics(key);
    if (cached) {
      setResult({ key, data: cached, error: null });
      return undefined;
    }
    setResult({ key, data: null, error: null });
    const controller = new AbortController();
    let current = true;
    const timeout = setTimeout(() => controller.abort(), 30000);
    apiRead(`/api/guilds/${guildId}/analytics?days=${days}`, controller.signal)
      .then((data) => {
        if (!current) return;
        const now = Date.now();
        for (const [stale, hit] of analyticsCache) if (now - hit.at >= ANALYTICS_CLIENT_TTL_MS) analyticsCache.delete(stale);
        analyticsCache.set(key, { at: now, data });
        setResult({ key, data, error: null });
      })
      .catch((error) => {
        if (current)
          setResult({
            key,
            data: null,
            error: controller.signal.aborted ? apiError("Request timed out. Refresh to try again.") : error,
          });
      })
      .finally(() => clearTimeout(timeout));
    return () => {
      current = false;
      clearTimeout(timeout);
      controller.abort();
    };
  }, [key]);
  const fresh = result.key === key;
  return {
    data: fresh ? result.data : null,
    error: fresh ? result.error : null,
    loading: !fresh || (!result.data && !result.error),
  };
}

const MOD_ACTION_LABELS = {
  warn: "Warn",
  warnclear: "Clear warnings",
  ban: "Ban",
  softban: "Softban",
  unban: "Unban",
  kick: "Kick",
  timeout: "Timeout",
  purge: "Purge",
  slowmode: "Slowmode",
};

// Running net membership change, shifted to start at zero so the sparkline
// shows its shape even when the server shrinks.
function runningNetShape(points) {
  let running = 0;
  const net = points.map((point) => (running += (point.joins ?? 0) - (point.leaves ?? 0)));
  const floor = Math.min(0, ...net);
  return net.map((value) => value - floor);
}

function analyticsPanel(title, id, body, className = "") {
  return h(
    "section",
    { className: `an-panel card bg-base-100 card-border ${className}`, "aria-labelledby": id },
    h("h3", { className: "an-panel-title", id }, title),
    body,
  );
}

function tabAnalytics() {
  const guildId = useRef(state.guildId).current;
  const g = selectedGuild();
  const [days, setDays] = useState(30);
  const result = useAnalyticsSummary(guildId, days);
  const data = result.data;
  const loading = result.loading;
  const failed = Boolean(result.error) && !data;
  const points = toTrendPoints(data?.series);
  const previous = data?.previous ?? null;
  const rangeLabel = `${days} days`;
  const members = data?.members ?? null;
  const total = (key) => sumSeries(points, key);
  const prev = (key) => (previous ? previous[key] : null);
  const daily = (key) => points.map((point) => point[key] ?? 0);
  const coverage = {
    importing: { label: "partial", tone: "warn", title: "The initial member import is still running." },
    uncertain: { label: "unverified", tone: "warn", title: "A coverage gap awaits Reconcile on the Members page." },
  }[members?.coverage];
  const tile = (label, key, color) =>
    h(KpiTile, {
      key: label,
      label,
      value: total(key),
      delta: data ? formatDelta(total(key), prev(key === "modActions" ? "mod_actions" : key)) : null,
      values: data ? daily(key) : null,
      color,
      loading,
      unavailable: failed,
      rangeLabel,
    });
  const joins = total("joins");
  const leaves = total("leaves");
  const netPrevious = previous?.joins == null || previous?.leaves == null ? null : previous.joins - previous.leaves;
  const rules = Object.fromEntries((state.config?.rules ?? []).map((rule) => [rule.rule_id, rule.name]));
  const pending = (text) => h("p", { className: "an-empty muted", "aria-busy": "true" }, text);
  const unavailable = h("p", { className: "an-empty muted" }, "Unavailable");
  const body = (render) => (data ? render() : failed ? unavailable : pending("Loading…"));
  return h(
    "div",
    { className: "contents analytics-page" },
    pageHeader(
      "Analytics",
      g ? `${g.name} · last ${rangeLabel} (UTC)` : `Last ${rangeLabel} (UTC)`,
      h(RangeSelector, { value: days, onChange: setDays }),
      h(InfoTip, { id: "analytics-help", label: "About these trends" }, ANALYTICS_HELP),
    ),
    failed
      ? h(
          "aside",
          { className: "dashboard-note", role: "status" },
          h("strong", {}, "Trends unavailable"),
          h("p", {}, String(result.error?.message ?? result.error ?? "Refresh to try again.")),
        )
      : null,
    h(
      "section",
      { className: "an-kpis", "aria-label": `Totals for the last ${rangeLabel}` },
      h(KpiTile, {
        label: "Members",
        value: members?.current ?? 0,
        loading,
        unavailable: failed || (data && !members),
        badge: coverage,
        rangeLabel,
      }),
      h(KpiTile, {
        label: "Net growth",
        value: joins - leaves,
        delta: data ? formatDelta(joins - leaves, netPrevious) : null,
        values: data ? runningNetShape(points) : null,
        color: "an-joins",
        loading,
        unavailable: failed,
        rangeLabel,
      }),
      tile("Joins", "joins", "an-joins"),
      tile("Leaves", "leaves", "an-leaves"),
      ...ENFORCEMENT_METRICS.map(([key, label, color]) => tile(label, key, color)),
    ),
    h(
      "div",
      { className: "an-grid" },
      analyticsPanel("Enforcement per day", "an-enforcement", body(() => h(EnforcementChart, { points })), "an-span-7"),
      analyticsPanel("Membership per day", "an-membership", body(() => h(MembershipChart, { points })), "an-span-5"),
      analyticsPanel(
        "Top automod rules",
        "an-rules",
        body(() =>
          h(RankList, {
            label: "Automod enforcement by rule",
            empty: "No automod enforcement in this range.",
            color: "an-spam",
            rows: (data.top_rules ?? []).map((row) => ({ key: row.key, label: rules[row.key] ?? row.key, title: row.key, count: row.count })),
          }),
        ),
        "an-span-4",
      ),
      analyticsPanel(
        "Mod actions by type",
        "an-kinds",
        body(() =>
          h(RankList, {
            label: "Mod actions by type",
            empty: "No mod actions in this range.",
            rows: (data.mod_action_kinds ?? []).map((row) => ({ key: row.key, label: MOD_ACTION_LABELS[row.key] ?? row.key, count: row.count })),
          }),
        ),
        "an-span-4",
      ),
      analyticsPanel(
        "Mod actions by moderator",
        "an-moderators",
        body(() =>
          h(RankList, {
            label: "Mod actions by moderator",
            empty: "No attributed mod actions in this range.",
            color: "an-timeouts",
            rows: (data.moderators ?? []).map((row) => ({
              key: row.moderator_id,
              label: row.name || `User ${row.moderator_id}`,
              title: `${row.name ? `${row.name} ` : ""}(${row.moderator_id})`,
              count: row.count,
            })),
          }),
        ),
        "an-span-4",
      ),
      analyticsPanel("Activity by weekday and hour (UTC)", "an-heat", body(() => h(ActivityHeatmap, { cells: data.heatmap ?? [] })), "an-span-12"),
    ),
  );
}

function isCustomRuleId(id) {
  const rest = String(id || "").slice("custom:".length);
  return id?.startsWith("custom:") && rest.length >= 8 && rest.length <= 32 && /^[0-9a-f]+$/.test(rest);
}

function newCustomRuleDraft() {
  const hex = crypto.randomUUID().replace(/-/g, "").slice(0, 16);
  return {
    rule_id: `custom:${hex}`,
    name: "Custom rule",
    description: "",
    effective: {
      name: "Custom rule",
      description: "",
      enabled: true,
      dry_run: true,
      params: { kind: "pattern", pattern: "" },
      actions: [{ at: 1, action: "delete", duration_minutes: 0, delete_days: 0, reason: "" }],
      whitelist_roles: [],
      whitelist_channels: [],
      whitelist_users: [],
      skip_moderators: false,
      include_channels: [],
      log_automod: true,
      open_case: true,
    },
    guild_override: null,
    global: null,
  };
}

function tabRules() {
  const rules = state.config.rules.filter((r) => r.rule_id !== "known_spam");
  return h(
    "div",
    {},
    h("h2", {}, "Automod rules"),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      h(
        ActionButton,
        {
          className: "btn btn-primary btn-sm",
          onClick: () => ui.set({ editor: h(CustomRuleEditor, { record: newCustomRuleDraft(), creating: true }) }),
        },
        "Add custom rule",
      ),
    ),
    h("p", { className: "muted" }, "Each rule catches one kind of abuse. Use dry-run while you tune it."),
    h(GoodToKnow, {
      items: [
        "Each time a rule catches a member, they get a strike. Actions are set per strike count.",
        "If a member has reached several steps, only the highest one runs.",
        "Exact image spam and Repeated text delete the message and ban the member.",
        "In dry-run, a rule only logs what it would have done.",
      ],
    }),
    ...rules.map((r) => ruleCard(r)),
  );
}

function ruleCard(r) {
  const rule = r.effective;
  const spam = !isCustomRuleId(r.rule_id) && spamRule(r.rule_id);
  const supported = !spam || spamConfigured(r.rule_id, rule);
  const badge = !rule.enabled || !supported ? "off" : rule.dry_run ? "dry" : "on";
  const label = !rule.enabled
    ? "disabled"
    : !supported
      ? "inactive — convert this rule"
      : rule.dry_run
        ? "dry-run"
        : spam
          ? "delete and ban"
          : "enabled";
  return h(
    "div",
    { className: "card bg-base-100 card-border" },
    h(
      "div",
      { className: "card-body" },
      h(
        "div",
        { className: "row", style: { justifyContent: "space-between" } },
        h(
          "div",
          { className: "rule-heading" },
          h("h3", { className: "card-title" }, r.name),
          h(
            "div",
            { className: "tag-group", "aria-label": "Rule status and source" },
            h(
              "span",
              {
                className: `badge badge-soft badge-sm ${{ off: "badge-error", dry: "badge-warning", on: "badge-success" }[badge]}`,
              },
              label,
            ),
            r.guild_override
              ? h("span", { className: "badge badge-soft badge-sm badge-neutral override-tag" }, "server override")
              : null,
            r.global ? h("span", { className: "badge badge-soft badge-sm badge-primary scope-tag" }, "global") : null,
          ),
        ),
        h(
          "div",
          { className: "card-actions" },
          h(IconButton, { action: "edit", label: "Edit", onClick: () => editRule(r) }),
        ),
      ),
      h("p", {}, r.description),
    ),
  );
}

function spamRule(id) {
  return id === "attachment_flood" || id === "repeat_text";
}

function spamConfigured(id, rule) {
  const p = rule.params || {};
  if (id === "attachment_flood") {
    const test =
      (p.mode === "exact_image_test" || (p.mode === "exact_image" && rule.dry_run === true)) &&
      (rule.actions || []).every((a) => a.action === "log") &&
      rule.dry_run === true;
    const enforce =
      p.mode === "exact_image" &&
      rule.dry_run === false &&
      (rule.actions || []).some((a) => a.action === "delete") &&
      (rule.actions || []).some((a) => a.action === "ban") &&
      (rule.actions || []).every((a) => ["delete", "ban", "log"].includes(a.action));
    return (test || enforce) && spamThresholds(p, id);
  }
  if (id === "repeat_text") {
    const test =
      p.mode === "exact_text" &&
      rule.dry_run === true &&
      (rule.actions || []).every((a) => a.action === "log");
    const enforce =
      p.mode === "exact_text" &&
      rule.dry_run === false &&
      (rule.actions || []).some((a) => a.action === "delete") &&
      (rule.actions || []).some((a) => a.action === "ban") &&
      (rule.actions || []).every((a) => ["delete", "ban", "log"].includes(a.action));
    return (test || enforce) && spamThresholds(p, id);
  }
  return false;
}

function spamThresholds(p, id) {
  const keys =
    id === "repeat_text"
      ? ["mode", "window_secs", "min_channels", "min_length"]
      : ["mode", "window_secs", "min_channels", "min_attachments"];
  if (!Object.keys(p).every((k) => keys.includes(k))) return false;
  const ranges =
    id === "repeat_text"
      ? [
          [p.window_secs, 1, 600],
          [p.min_channels, 2, 10],
          [p.min_length, 1, 2000],
        ]
      : [
          [p.window_secs, 1, 600],
          [p.min_channels, 2, 10],
          [p.min_attachments, 1, 10],
        ];
  return ranges.every(([n, min, max]) => Number.isInteger(n) && n >= min && n <= max);
}

function buildCustomRuleSaveBody(rule, creating) {
  const body = JSON.parse(JSON.stringify(rule));
  if (creating) body.dry_run = true;
  return body;
}

function CustomRuleEditor({ record: r, creating }) {
  const [rule] = useState(() => JSON.parse(JSON.stringify(r.effective)));
  const [, bumpForm] = useState(0);
  const kind = rule.params?.kind || "pattern";
  return h(
    DialogPanel,
    { className: "modal-box rule-editor", onInput: trackDraft },
    h("h3", { className: "card-title" }, creating ? "Add custom rule" : `Edit custom rule: ${r.name || r.rule_id}`),
    field("Name", h(Input, { type: "text", defaultValue: rule.name, className: "input", onInput: (e) => (rule.name = e.target.value) })),
    field(
      "Type",
      h(
        Select,
        {
          className: "select",
          defaultValue: kind,
          onChange: (e) => {
            const next = e.target.value;
            const prev = rule.params || {};
            rule.params = { kind: next };
            if (next === "pattern") rule.params.pattern = prev.pattern || "";
            if (next === "word") rule.params.words = Array.isArray(prev.words) ? prev.words : [];
            if (next === "channel_content") {
              rule.params.links = prev.links !== false;
              rule.params.attachments = prev.attachments === true;
            }
            if (next === "honeypot_role") {
              rule.params.role_id = prev.role_id || "";
              const current = rule.actions?.[0]?.action;
              if (!current || current === "delete") {
                rule.actions = [{
                  at: 1,
                  action: "kick",
                  duration_minutes: 0,
                  delete_days: 0,
                  reason: rule.actions?.[0]?.reason || "",
                }];
              }
            }
            markDirty();
            bumpForm((n) => n + 1);
          },
        },
        h("option", { value: "pattern" }, "Pattern (regex)"),
        h("option", { value: "word" }, "Word list"),
        h("option", { value: "channel_content" }, "Channel content"),
        h("option", { value: "everyone_attempt" }, "@everyone / @here attempt"),
        h("option", { value: "honeypot_role" }, "Honeypot role"),
      ),
    ),
    kind === "pattern"
      ? field("Pattern", h(Input, { type: "text", className: "input mono", defaultValue: rule.params.pattern || "", onInput: (e) => (rule.params.pattern = e.target.value) }))
      : null,
    kind === "word"
      ? field("Words (comma-separated)", h(Input, { type: "text", className: "input", defaultValue: (rule.params.words || []).join(","), onInput: (e) => (rule.params.words = e.target.value.split(",").map((w) => w.trim()).filter(Boolean)) }))
      : null,
    kind === "channel_content"
      ? h(
          "div",
          { className: "row checkbox-group" },
          h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.params.links, onChange: (e) => (rule.params.links = e.target.checked) }), "Links"),
          h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.params.attachments, onChange: (e) => (rule.params.attachments = e.target.checked) }), "Attachments"),
        )
      : null,
    kind === "honeypot_role"
      ? h(
          "p",
          { className: "muted" },
          "Default action is kick. For a hidden case, turn on Open a case and turn off Post to automod log. Role id is a snowflake string.",
          field("Role id", h(Input, { type: "text", className: "input mono", defaultValue: String(rule.params.role_id || ""), onInput: (e) => (rule.params.role_id = e.target.value.trim()) })),
        )
      : null,
    field(
      "Action",
      h(
        Select,
        {
          key: `action-${rule.actions?.[0]?.action || "delete"}`,
          className: "select",
          value: rule.actions?.[0]?.action || "delete",
          onChange: (e) => {
            const action = e.target.value;
            const prev = rule.actions?.[0] || {};
            let duration = prev.duration_minutes || 0;
            if (action === "timeout" && !duration) duration = 60;
            rule.actions = [{
              at: 1,
              action,
              duration_minutes: duration,
              delete_days: action === "ban" ? prev.delete_days || 0 : 0,
              purge_last_hour: action === "ban" && prev.purge_last_hour === true,
              reason: prev.reason || "",
            }];
            markDirty();
            bumpForm((n) => n + 1);
          },
        },
        ["delete", "warn", "timeout", "kick", "ban"].map((a) => h("option", { key: a, value: a }, a)),
      ),
    ),
    rule.actions?.[0]?.action === "timeout"
      ? field("Timeout (minutes)", h(Input, { type: "number", className: "input", defaultValue: rule.actions[0].duration_minutes || 60, onInput: (e) => (rule.actions[0].duration_minutes = Number(e.target.value)) }))
      : null,
    rule.actions?.[0]?.action === "ban"
      ? h(BanOptions, { action: rule.actions[0] })
      : null,
    field("Reason template", h(Input, { type: "text", className: "input", defaultValue: rule.actions?.[0]?.reason || "", onInput: (e) => (rule.actions[0].reason = e.target.value) })),
    h(
      "div",
      { className: "row checkbox-group" },
      h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.enabled, onChange: (e) => (rule.enabled = e.target.checked) }), "Enabled"),
      h(
        "label",
        { className: "checkbox-label" },
        h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.dry_run, disabled: creating, onChange: (e) => (rule.dry_run = e.target.checked) }),
        "Dry-run (log only)",
      ),
      h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.log_automod, onChange: (e) => (rule.log_automod = e.target.checked) }), "Post to automod log"),
      h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.open_case, onChange: (e) => (rule.open_case = e.target.checked) }), "Open a case"),
      h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.skip_moderators, onChange: (e) => (rule.skip_moderators = e.target.checked) }), "Skip moderators"),
    ),
    field("Only these channels (empty means all)", h(Input, { type: "text", className: "input", defaultValue: (rule.include_channels || []).join(","), onInput: (e) => (rule.include_channels = splitIds(e.target.value)) })),
    h(
      "div",
      { className: "row form-row" },
      field("Skip roles (ids, comma)", h(Input, { type: "text", className: "input", defaultValue: (rule.whitelist_roles || []).join(","), onInput: (e) => (rule.whitelist_roles = splitIds(e.target.value)) })),
      field("Skip channels (ids)", h(Input, { type: "text", className: "input", defaultValue: (rule.whitelist_channels || []).join(","), onInput: (e) => (rule.whitelist_channels = splitIds(e.target.value)) })),
      field("Skip users (ids)", h(Input, { type: "text", className: "input", defaultValue: (rule.whitelist_users || []).join(","), onInput: (e) => (rule.whitelist_users = splitIds(e.target.value)) })),
    ),
    h(
      "div",
      { className: "modal-action" },
      h(
        ActionButton,
        {
          className: "btn btn-primary",
          onClick: async () => {
            const data = buildCustomRuleSaveBody(rule, creating);
            const saved = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", { scope: "guild", data });
            if (!applyConfigPatch(saved) && !(await loadConfig())) return;
            toast("Rule saved", "success");
            closeOverlay();
            renderTab();
          },
        },
        "Save",
      ),
      !creating
        ? h(
            ActionButton,
            {
              className: "btn btn-error",
              onClick: async () => {
                await reloadAfter(() => api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" }), () => {
                  toast("Rule deleted", "success");
                  closeOverlay();
                });
              },
            },
            "Delete",
          )
        : null,
      h(ActionButton, { className: "btn btn-outline", onClick: () => canLeave() && closeOverlay() }, "Cancel"),
    ),
  );
}

function RuleEditor({ record: r }) {
  const [rule] = useState(() => JSON.parse(JSON.stringify(r.effective)));
  const [, setActionsRevision] = useState(0);
  const renderActions = () => setActionsRevision((value) => value + 1);
  const spam = spamRule(r.rule_id);
  const textRule = r.rule_id === "repeat_text";
  const image = r.rule_id === "attachment_flood";
  const knownSpam = r.rule_id === "known_spam";
  const imageChannelsRef = useRef(null);
  const imageChannels = h(Input, {
    ref: imageChannelsRef,
    type: "number",
    min: 2,
    max: 10,
    defaultValue: rule.params?.min_channels ?? 3,
    className: "input",
  });
  const imageCountRef = useRef(null);
  const imageCount = h(Input, {
    ref: imageCountRef,
    type: "number",
    min: 1,
    max: 10,
    defaultValue: rule.params?.min_attachments ?? 2,
    className: "input",
  });
  const imageWindowRef = useRef(null);
  const imageWindow = h(Input, {
    ref: imageWindowRef,
    type: "number",
    min: 1,
    max: 600,
    defaultValue: rule.params?.window_secs ?? 60,
    className: "input",
  });
  const textLengthRef = useRef(null);
  const textLength = h(Input, {
    ref: textLengthRef,
    type: "number",
    min: 1,
    max: 2000,
    defaultValue: rule.params?.min_length ?? 20,
    className: "input",
  });
  const [spamBan] = useState(() => ({
    ...(rule.actions.find((action) => action.action === "ban") || rule.actions[0] || {}),
  }));
  // Inactive legacy image rows should convert to the safe dry-run mode by default.
  const dryRunRef = useRef(null);
  const dryRun = h(Input, {
    ref: dryRunRef,
    type: "checkbox",
    defaultChecked: spam ? !spamConfigured(r.rule_id, rule) || rule.dry_run : rule.dry_run,
    onChange: (e) => (rule.dry_run = e.target.checked),
    className: "checkbox",
  });
  const numberParams = {
    mention_spam: [
      ["window_secs", "Window (seconds)", 10],
      ["max_mentions_in_window", "Maximum mentions in window", 5],
    ],
    new_member_url: [["grace_secs", "Seconds after joining", 120]],
    velocity: [
      ["window_secs", "Window (seconds)", 10],
      ["max_messages", "Maximum messages in window", 8],
    ],
    account_age: [["min_age_days", "Minimum account age (days)", 3]],
  }[r.rule_id] || [];
  const numberParamsRef = useRef({});
  const inviteCodesRef = useRef(null);
  const inviteCodes = (value) => Array.isArray(value)
    ? value.filter((code) => typeof code === "string") : [];
  const detectionFields = numberParams.map(([key, label, fallback]) =>
    h(React.Fragment, { key }, field(label, h(Input, {
      ref: (node) => { numberParamsRef.current[key] = node; },
      type: "number",
      min: key === "window_secs" ? 1 : 0,
      max: key === "window_secs" ? 600 : Number.MAX_SAFE_INTEGER,
      step: 1,
      required: true,
      defaultValue: rule.params?.[key] ?? fallback,
      className: "input",
    }))),
  );
  if (r.rule_id === "invite") {
    detectionFields.push(h(React.Fragment, { key: "invite_codes" }, field("Allowed invite codes (comma-separated)", h(Input, {
      ref: inviteCodesRef,
      type: "text",
      defaultValue: inviteCodes(rule.params?.allowlist_codes).join(", "),
      placeholder: "code-one, code-two",
      className: "input",
    }))));
  }

  const whitelistTextRef = useRef(null);
  const whitelistText = h(Input, {
    ref: whitelistTextRef,
    type: "text",
    defaultValue: (rule.whitelist_roles || []).join(","),
    className: "input",
  });
  const channelsTextRef = useRef(null);
  const channelsText = h(Input, {
    ref: channelsTextRef,
    type: "text",
    defaultValue: (rule.whitelist_channels || []).join(","),
    className: "input",
  });
  const usersTextRef = useRef(null);
  const usersText = h(Input, {
    ref: usersTextRef,
    type: "text",
    defaultValue: (rule.whitelist_users || []).join(","),
    className: "input",
  });
  const skipModRef = useRef(null);
  const skipMod = h(Input, {
    ref: skipModRef,
    type: "checkbox",
    defaultChecked: rule.skip_moderators,
    onChange: (e) => (rule.skip_moderators = e.target.checked),
    className: "checkbox",
  });
  const includeChannelsRef = useRef(null);
  const includeChannels = h(Input, {
    ref: includeChannelsRef,
    type: "text",
    defaultValue: (rule.include_channels || []).join(","),
    className: "input",
  });
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef);

  const actionsRows = h(
    "div",
    null,
    ...rule.actions.map((action, i) =>
      h(RuleAction, {
        key: itemKey(action),
        action,
        index: i,
        remove: () => {
          rule.actions.splice(i, 1);
          markDirty();
          renderActions();
        },
      }),
    ),
  );

  const box = h(
    DialogPanel,
    { className: "modal-box rule-editor", onInput: trackDraft },
    h(
      "div",
      { className: "editor-heading" },
      h("span", { className: "eyebrow" }, "AUTOMOD RULE"),
      h("h3", { className: "card-title" }, `Edit rule: ${r.name}`),
      h(
        "p",
        { className: "muted" },
        r.description || "Configure detection, actions and exemptions for this rule.",
      ),
    ),
    h(
      "div",
      { className: "row checkbox-group" },
      h(
        "label",
        { className: "checkbox-label" },
        h(Input, {
          type: "checkbox",
          defaultChecked: rule.enabled,
          onChange: (e) => (rule.enabled = e.target.checked),
          className: "checkbox",
        }),
        "Enabled",
      ),
      h("label", { className: "checkbox-label" }, dryRun, "Dry-run (log only)"),
    ),
    spam
      ? h(
          "div",
          {},
          h(
            "p",
            {},
            textRule
              ? "Exact text after trimming ends only. No case folding. Empty text never matches. Default minimum length is 20 Unicode scalars."
              : "Exact common image bytes across channels. Extra images are allowed. Creates and relevant attachment edits participate. Work limits can reduce coverage.",
          ),
          field("Distinct channels", imageChannels),
          image
            ? field("Common distinct images per message", imageCount)
            : null,
          textRule
            ? field("Minimum length (Unicode scalars after trim)", textLength)
            : null,
          field("Window (seconds)", imageWindow),
          h(BanOptions, { action: spamBan }),
        )
      : r.rule_id === "badword"
        ? h("p", { className: "field-help" }, "Manage words and advanced regex in Badwords. These settings apply to that saved list.")
        : knownSpam
          ? h("p", { className: "field-help" }, "First match: Ban and purge the previous hour (3600 seconds). No strike ladder. Manage samples and URLs on Known spam.")
          : detectionFields,
    spam || knownSpam
      ? null
      : field(
          "Action escalation",
          h(
            "p",
            { className: "field-help" },
            "Strike count is the threshold for an action. Timeout minutes apply to timeout. Choose Ban or Ban and purge for each ban action.",
          ),
          actionsRows,
          h(
            ActionButton,
            {
              className: "btn btn-sm",
              onClick: () => {
                rule.actions.push({
                  at: rule.actions.length + 1,
                  action: "warn",
                  duration_minutes: 0,
                  delete_days: 0,
                  reason: "",
                });
                markDirty();
                renderActions();
              },
            },
            "+ add action",
          ),
        ),
    h(
      "div",
      { className: "row form-row" },
      field("Whitelist roles (ids, comma)", whitelistText),
      field("Whitelist channels (ids)", channelsText),
      field("Whitelist users (ids)", usersText),
    ),
    h(
      "div",
      { className: "row checkbox-group" },
      h("label", { className: "checkbox-label" }, skipMod, "Skip moderators"),
      field("Only these channels (empty means all)", includeChannels),
    ),
    h(
      "div",
      { className: "modal-action" },
      field("Applies to", scopeSel),
      h(
        ActionButton,
        {
          className: "btn btn-primary",
          onClick: async () => {
            try {
              if (spam) {
                const punish = !dryRunRef.current.checked;
                if (textRule) {
                  rule.params = {
                    mode: "exact_text",
                    min_channels: Number(imageChannelsRef.current.value),
                    window_secs: Number(imageWindowRef.current.value),
                    min_length: Number(textLengthRef.current.value),
                  };
                } else {
                  rule.params = {
                    mode: punish ? "exact_image" : "exact_image_test",
                    min_channels: Number(imageChannelsRef.current.value),
                    min_attachments: Number(imageCountRef.current.value),
                    window_secs: Number(imageWindowRef.current.value),
                  };
                }
                rule.dry_run = !punish;
                rule.actions = punish
                  ? [
                      { at: 1, action: "delete", reason: "spam match", duration_minutes: 0, delete_days: 0 },
                      { at: 1, action: "ban", reason: "spam match", duration_minutes: 0, delete_days: spamBan.delete_days || 0, purge_last_hour: spamBan.purge_last_hour === true },
                    ]
                  : [
                      {
                        at: 1,
                        action: "log",
                        reason: textRule ? "Repeated text test" : "Exact image spam test",
                        duration_minutes: 0,
                        delete_days: spamBan.delete_days || 0,
                        purge_last_hour: spamBan.purge_last_hour === true,
                      },
                    ];
                if (!spamConfigured(r.rule_id, rule))
                  throw new Error(
                    "Channels must be 2–10 and window 1–600 seconds, all whole numbers. Images 1–10. Text length 1–2000.",
                  );
              } else if (r.rule_id !== "badword" && !knownSpam) {
                const params = {};
                for (const [key, label] of numberParams) {
                  const input = numberParamsRef.current[key];
                  const value = Number(input.value);
                  if (input.value.trim() === "" || !Number.isSafeInteger(value) || value < 0)
                    throw new Error(`${label} must be a non-negative whole number.`);
                  if (key === "window_secs" && (value < 1 || value > 600))
                    throw new Error("Window must be 1–600 seconds.");
                  params[key] = value;
                }
                if (r.rule_id === "invite") {
                  // Legacy allowlist_guilds was never read by the bot. Do not
                  // reinterpret those guild IDs as invite codes on edit.
                  params.allowlist_codes = inviteCodes(inviteCodesRef.current.value
                    .split(",")).map((code) => code.trim()).filter(Boolean);
                }
                rule.params = params;
              }
            } catch (e) {
              toast("Invalid detection settings: " + e.message, "error");
              return;
            }
            rule.whitelist_roles = splitIds(whitelistTextRef.current.value);
            rule.whitelist_channels = splitIds(channelsTextRef.current.value);
            rule.whitelist_users = splitIds(usersTextRef.current.value);
            rule.skip_moderators = skipModRef.current?.checked ?? rule.skip_moderators;
            rule.include_channels = splitIds(includeChannelsRef.current?.value || "");
            const scope = scopeSelRef.current.value;
            const saved = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", {
              scope,
              data: rule,
              clear_guild_override: scope === "global" && state.guildId !== "0",
            });
            const savedLocally = applyConfigPatch(saved);
            if (!savedLocally && !(await loadConfig())) return;
            toast("Rule saved", "success");
            closeOverlay();
            renderTab();
          },
        },
        "Save",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-error",
          onClick: async () => {
            await reloadAfter(
              () => api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "DELETE", { scope: "guild" }),
              () => {
                toast("Rule reset", "success");
                closeOverlay();
              },
            );
          },
        },
        "Reset to global",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-outline",
          onClick: () => {
            if (canLeave()) closeOverlay();
          },
        },
        "Cancel",
      ),
    ),
  );
  return box;
}

function parseSnowflake(raw) {
  const text = String(raw ?? "").trim();
  if (!text) return "";
  const mention = text.match(/^<(?:#|@!?|@&)(\d+)>$/);
  if (mention) return mention[1];
  const url = text.match(/discord(?:app)?\.com\/channels\/\d+\/(\d+)/i);
  if (url) return url[1];
  return /^\d+$/.test(text) ? text : "";
}

function parseLogFilterId(raw, kind) {
  const value = String(raw ?? "").trim();
  const match = kind === "user" ? value.match(/^<@!?(\d+)>$/) : value.match(/^<#(\d+)>$/);
  const url =
    kind === "channel"
      ? value.match(/^https?:\/\/(?:canary\.|ptb\.)?discord(?:app)?\.com\/channels\/\d+\/(\d+)\/?$/i)
      : null;
  const id = match?.[1] || url?.[1] || (/^\d+$/.test(value) ? value : "");
  return id && /[1-9]/.test(id) ? id : "";
}

function splitIds(s) {
  const ids = [];
  for (const raw of String(s ?? "").split(",")) {
    const token = raw.trim();
    if (!token) continue;
    const id = parseSnowflake(token);
    if (!id) throw new Error("Enter Discord IDs as digits, mentions, or channel URLs");
    ids.push(id);
  }
  return ids;
}

function applyPlanStepInput(step, type, raw) {
  if (type === "reply" || type === "dm" || type === "log") step.template = raw;
  else if (type === "add_role" || type === "remove_role") step.role_id = parseSnowflake(raw);
  else if (type === "timeout") step.minutes = Number(raw) || 1;
  else if (type === "ban") step.delete_days = Math.min(7, Math.max(0, Number(raw) || 0));
  else if (type === "react") step.emoji = raw;
  else if (type === "wait") step.secs = Math.max(1, Number(raw) || 1);
}

// A rule's escalation steps in plain words (#620), e.g. "Strike 1: Warn · Strike 3: Timeout 10 min".
// Lists only the steps the bot can run: at each strike it picks the last step with the highest
// strike count reached (RuleDef::action_for_strike), and a timeout lasts at least one minute.
function describeRuleActions(rule) {
  const actions = rule.actions ?? [];
  const name = (step) => {
    if (step.action === "timeout") return `Timeout ${Math.max(1, Number(step.duration_minutes) || 0)} min`;
    if (step.action === "ban")
      return step.purge_last_hour
        ? "Ban and delete the last hour of messages"
        : step.delete_days
          ? `Ban and delete ${step.delete_days} day(s) of messages`
          : "Ban";
    const action = String(step.action || "unknown");
    return action[0].toUpperCase() + action.slice(1);
  };
  const at = (step) => step.at ?? 0;
  const forStrike = (strike) =>
    actions.reduce((best, step) => (at(step) <= strike && (!best || at(step) >= at(best)) ? step : best), null);
  // Strikes start at 1, so a step at 0 first runs on strike 1.
  const strikes = [...new Set(actions.map((step) => Math.max(1, at(step))))].sort((a, b) => a - b);
  return strikes.length
    ? strikes.map((strike) => `Strike ${strike}: ${name(forStrike(strike))}`).join(" · ")
    : "No actions set";
}

// Badwords splits a words paste on commas and new lines (jmodguard::normalize::badword_patterns).
const BADWORD_MAX_ENTRIES = 32;
const BADWORD_AUTO_HANDLED = [
  ["Upper and lower case", "SCAM, Scam and scam all match."],
  ["Accents", "Letters with accents match their plain letter."],
  ["Hidden characters", "Invisible zero-width characters are ignored."],
  ["Spacing", "Letters split up with spaces still match."],
  ["Leetspeak", "Common swaps like 5c4m for scam still match."],
  ["Whole words", "A word only matches on its own, not inside a longer word."],
];

function tabBadwords() {
  const words = state.config.badwords;
  const { drafts } = useStore(ui);
  const mode = drafts["badwords:mode"]?.value || "words";
  const entryCount = String(drafts["badwords:pattern"]?.value ?? "")
    .split(/[,\n\r]/)
    .filter((entry) => entry.trim()).length;
  const badwordRule = state.config.rules.find((rule) => rule.rule_id === "badword");
  const patternRef = useRef(null);
  const pattern = h(Textarea, {
    ref: patternRef,
    draftKey: "badwords:pattern",
    rows: 3,
    maxLength: mode === "regex" ? 512 : 4096,
    "aria-label": mode === "regex" ? "Advanced regex pattern" : "Words or phrases",
    placeholder: mode === "regex" ? "Advanced regex, e.g. (scam|pump and dump)" : "Words or phrases, separated by commas or newlines",
    style: { width: "100%" },
    className: "textarea",
  });
  const noteRef = useRef(null);
  const note = h(Input, {
    ref: noteRef,
    draftKey: "badwords:note",
    type: "text",
    placeholder: "note (optional)",
    className: "input",
  });
  const scopeRef = useRef(null);
  const scope = scopeSelect(scopeRef, "This server", "Global", true, "badwords:scope");
  const sampleRef = useRef(null);
  const sample = h(Input, {
    ref: sampleRef,
    draftKey: "badwords:sample",
    type: "text",
    placeholder: "sample text to test, e.g. 5c@m 🅿🆁🅾🅼🅾",
    style: { flex: "2" },
    className: "input",
  });
  const [result, setResult] = useState("");

  return h(
    "div",
    {},
    h("h2", {}, "Badwords (bypass-resistant)"),
    h("p", { className: "muted" }, "Block words and phrases in your server's messages."),
    h(
      "ul",
      { className: "tag-group", "aria-label": "Words and phrases handle automatically" },
      h("li", { className: "muted" }, "Words and phrases handle automatically:"),
      ...BADWORD_AUTO_HANDLED.map(([label, title]) =>
        h("li", { key: label, className: "badge badge-soft badge-sm badge-neutral", title }, label),
      ),
    ),
    h(GoodToKnow, {
      items: [
        isBotOwner()
          ? "Global entries apply to every server."
          : "Global entries apply to every server. Only the bot owner can change them.",
      ],
    }),
    badwordRule
      ? h(
          "div",
          { className: "card bg-base-100 card-border" },
          h(
            "div",
            { className: "card-body" },
            h("h3", { className: "card-title" }, "What happens on match"),
            h(
              "p",
              { className: "badword-action-summary" },
              !badwordRule.effective.enabled
                ? h("span", { className: "badge badge-soft badge-sm badge-error" }, "Off")
                : badwordRule.effective.dry_run
                  ? h("span", { className: "badge badge-soft badge-sm badge-warning" }, "Dry-run")
                  : h("span", { className: "badge badge-soft badge-sm badge-success" }, "On"),
              " ",
              !badwordRule.effective.enabled
                ? "Matches are ignored."
                : badwordRule.effective.dry_run
                  ? `Only logs matches. When on: ${describeRuleActions(badwordRule.effective)}`
                  : describeRuleActions(badwordRule.effective),
            ),
            h(
              "div",
              { className: "card-actions" },
              h(
                ActionButton,
                { className: "btn btn-sm", onClick: () => editRule(badwordRule) },
                "Edit actions & exemptions",
              ),
            ),
          ),
        )
      : null,
    h(
      DraftCard,
      { group: "badwords", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h(
          "div",
          { className: "row badword-entry-row" },
          field(
            "Entry type",
            h(
              Select,
              { draftKey: "badwords:mode", defaultValue: "words", className: "select" },
              h("option", { value: "words" }, "Words or phrases"),
              h("option", { value: "regex" }, "Advanced regex"),
            ),
            h(
              "p",
              { className: "field-help" },
              mode === "regex"
                ? "Checked as written and as a lowercase copy with disguises removed. Add \\b for whole words."
                : "Matches whole words and catches common disguises.",
            ),
          ),
          h(
            "div",
            { className: "badword-entry" },
            pattern,
            h(
              "p",
              { className: "field-help", "aria-live": "polite" },
              mode === "regex"
                ? "One pattern, up to 512 bytes."
                : `${entryCount}/${BADWORD_MAX_ENTRIES} entries`,
            ),
          ),
          note,
          scope,
          h(
            ActionButton,
            {
              className: "btn btn-primary",
              onClick: async () => {
                if (!patternRef.current.value.trim()) return toast("Enter words, phrases or an advanced regex", "error");
                await reloadAfter(
                  () => api(`/api/guilds/${state.guildId}/badwords`, "POST", {
                    scope: scopeRef.current.value,
                    [mode === "regex" ? "pattern" : "words"]: patternRef.current.value.trim(),
                    note: noteRef.current.value || null,
                  }),
                  () => toast("Badwords saved", "success"),
                );
              },
            },
            "Add",
          ),
        ),
      ),
    ),
    h(
      DraftCard,
      { group: "badwords-test", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Live tester"),
        h("p", { className: "field-help" }, "Checks text against your list and the draft above. It doesn't take any action."),
        h(
          "div",
          { className: "row" },
          sample,
          h(
            ActionButton,
            {
              className: "btn",
              onClick: async () => {
                if (!sampleRef.current.value) return toast("Enter sample text", "error");
                const draft = patternRef.current.value.trim();
                const res = await api(`/api/guilds/${state.guildId}/testbadwords`, "POST", {
                  sample: sampleRef.current.value,
                  ...(draft ? { [mode === "regex" ? "pattern" : "words"]: draft } : {}),
                });
                const summary = res.matched
                  ? `Matched ${res.matches.map((match) => `${match.label} (${match.source})`).join(", ")}`
                  : "No match in the saved list or draft.";
                setResult(summary + (res.invalid_patterns ? `\nSkipped ${res.invalid_patterns} invalid saved patterns.` : ""));
              },
            },
            "Test",
          ),
        ),
        result ? h("pre", { className: "code" }, result) : null,
      ),
    ),
    h(
      "div",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, `Words and patterns (${words.length})`),
        words.length === 0
          ? h("p", { className: "muted" }, "No words yet. Add some above, for example: scam, free nitro")
          : h(
              DataTable,
              { headings: ["Scope", "Word, phrase or advanced regex", "Note", "Status", ""] },
              words.map((w) => {
                const isGlobal = String(w.guild_id) === "0";
                const canDelete = !isGlobal || isBotOwner();
                const invalid = w.valid === false;
                return h(
                  "tr",
                  { key: w.id },
                  h("td", {}, isGlobal ? "global" : "server"),
                  h("td", { className: "mono" }, w.word || `Regex · ${w.pattern}`),
                  h("td", { className: "muted" }, w.note || ""),
                  h(
                    "td",
                    {},
                    invalid
                      ? h(
                          "span",
                          {
                            className: "badge badge-soft badge-sm badge-error",
                            title: w.error || "failed compile limits",
                          },
                          "not enforced",
                        )
                      : h("span", { className: "badge badge-soft badge-sm badge-success" }, "valid"),
                  ),
                  h(
                    "td",
                    {},
                    canDelete
                      ? h(IconButton, {
                          action: "remove",
                          label: "Remove",
                          onClick: async () => {
                            await reloadAfter(
                              () => api(`/api/guilds/${state.guildId}/badwords/${w.id}`, "DELETE"),
                              () => toast("Removed", "success"),
                            );
                          },
                        })
                      : h("span", { className: "muted" }, "owner only"),
                  ),
                );
              }),
            ),
      ),
    ),
  );
}

function tabCommands() {
  const cmds = state.config.commands;
  return h(
    "div",
    {},
    h(
      "div",
      { className: "row title-row" },
      h("h2", {}, "Commands"),
      canManageServer() &&
        h(
          ActionButton,
          { className: "btn btn-primary", onClick: () => editCommand(null) },
          "+ New custom command",
        ),
    ),
    !canManageServer() &&
      h("p", { className: "muted" }, "Editing commands needs Manage Server: command plans can grant roles and run moderation actions."),
    h("p", { className: "muted" }, "Rename any command, change its replies, who can use it, and its cooldown."),
    h(GoodToKnow, {
      items: [
        "Custom commands can run several steps: reply, DM, log, change roles, timeout, kick or ban.",
        "Enabled commands also show up in Discord as slash commands.",
      ],
    }),
    h(
      DataTable,
      { headings: ["Command", "Category", "Description", "Permission", "State", ""], className: "commands-table table" },
      cmds.map((c) => {
        const d = c.effective;
        return h(
          "tr",
          { key: c.name },
          h(
            "td",
            {},
            h(
              "div",
              { className: "command-identity" },
              h("strong", { className: "mono" }, d.name),
              h(
                "div",
                { className: "tag-group" },
                c.guild_override
                  ? h(
                      "span",
                      { className: "badge badge-soft badge-sm badge-neutral override-tag" },
                      "server override",
                    )
                  : null,
                c.scope === "global"
                  ? h("span", { className: "badge badge-soft badge-sm badge-primary scope-tag" }, "global")
                  : null,
              ),
            ),
          ),
          h("td", {}, d.category),
          h("td", { className: "muted command-description" }, d.description),
          h("td", {}, d.permission),
          h(
            "td",
            {},
            h(
              "span",
              { className: `badge badge-soft badge-sm ${d.enabled ? "badge-success" : "badge-error"}` },
              d.enabled ? "enabled" : "disabled",
            ),
          ),
          h(
            "td",
            {},
            canManageServer() && h(IconButton, { action: "edit", label: "Edit", onClick: () => editCommand(c) }),
          ),
        );
      }),
    ),
  );
}

function parsePlanParams(value) {
  const params = JSON.parse(value || "{}");
  if (!params || typeof params !== "object" || Array.isArray(params))
    throw new Error("plan parameters must be an object");
  if (params.steps !== undefined && !Array.isArray(params.steps))
    throw new Error("plan steps must be an array");
  params.steps = params.steps || [];
  if (params.steps.some((step) => !step || typeof step !== "object" || Array.isArray(step)))
    throw new Error("each plan step must be an object");
  return params;
}

function CommandEditor({ record: c }) {
  const isNew = c === null;
  const [data] = useState(() =>
    isNew
      ? {
          name: "",
          aliases: [],
          category: "Custom",
          description: "Custom command.",
          usage: "",
          enabled: true,
          permission: "everyone",
          roles: [],
          channels: [],
          cooldown_secs: 0,
          behavior: { kind: "plan", params: { steps: [{ type: "reply", template: "Hello {user}!" }] } },
          response: "",
          log_event: "",
        }
      : JSON.parse(JSON.stringify(c.effective)),
  );

  const nameInRef = useRef(null);
  const nameIn = h(Input, {
    ref: nameInRef,
    type: "text",
    defaultValue: data.name,
    placeholder: "command name",
    className: "input",
  });
  const descInRef = useRef(null);
  const descIn = h(Input, {
    ref: descInRef,
    type: "text",
    defaultValue: data.description,
    placeholder: "description",
    className: "input",
  });
  const usageInRef = useRef(null);
  const usageIn = h(Input, {
    ref: usageInRef,
    type: "text",
    defaultValue: data.usage,
    placeholder: "usage",
    className: "input",
  });
  const aliasesInRef = useRef(null);
  const aliasesIn = h(Input, {
    ref: aliasesInRef,
    type: "text",
    defaultValue: (data.aliases || []).join(","),
    placeholder: "aliases, comma separated",
    className: "input",
  });
  const respInRef = useRef(null);
  const respIn = h(Textarea, {
    ref: respInRef,
    defaultValue: data.response || "",
    placeholder: "response template, e.g. Banned {target} | {reason}",
    className: "textarea",
  });
  const permInRef = useRef(null);
  const permIn = h(
    Select,
    { ref: permInRef, defaultValue: data.permission, className: "select" },
    ["everyone", "mod", "admin", "owner"].map((p) => h("option", { value: p }, p)),
  );
  const cdInRef = useRef(null);
  const cdIn = h(Input, {
    ref: cdInRef,
    type: "number",
    min: 0,
    defaultValue: data.cooldown_secs || 0,
    className: "input",
  });
  const roleInRef = useRef(null);
  const roleIn = h(Input, {
    ref: roleInRef,
    type: "text",
    defaultValue: (data.roles || []).join(","),
    placeholder: "role ids",
    className: "input",
  });
  const chanInRef = useRef(null);
  const chanIn = h(Input, {
    ref: chanInRef,
    type: "text",
    defaultValue: (data.channels || []).join(","),
    placeholder: "channel ids",
    className: "input",
  });
  const logInRef = useRef(null);
  const logIn = h(Input, {
    ref: logInRef,
    type: "text",
    defaultValue: data.log_event || "",
    placeholder: "mod_action",
    className: "input",
  });
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef);
  const isPlan = data.behavior && data.behavior.kind === "plan";
  const [behaviorJson, setBehaviorJson] = useState(() =>
    JSON.stringify(isPlan ? data.behavior.params || {} : data.behavior || {}, null, 2),
  );
  const [, setStepsRevision] = useState(0);
  const renderSteps = () => setStepsRevision((value) => value + 1);
  const syncPlanJson = () => {
    if (isPlan) setBehaviorJson(JSON.stringify(data.behavior.params || {}, null, 2));
  };
  const behaviorAreaRef = useRef(null);
  const behaviorArea = h(Textarea, {
    ref: behaviorAreaRef,
    className: "mono textarea",
    style: { minHeight: "70px" },
    value: behaviorJson,
    onChange: (event) => setBehaviorJson(event.target.value),
    onBlur: () => {
      if (!isPlan) return;
      try {
        data.behavior.params = parsePlanParams(behaviorJson);
        renderSteps();
      } catch (error) {
        toast("Invalid behavior JSON: " + error.message, "error");
      }
    },
  });
  const typePlaceholder = (t) =>
    ({
      reply: "message template, e.g. Hi {user}!",
      dm: "DM template",
      log: "log template",
      add_role: "role id",
      remove_role: "role id",
      timeout: "minutes",
      ban: "delete_days",
      delete_command_message: "…",
      react: "emoji",
      wait: "seconds",
    })[t] || "value";
  const stepsBox = h(
    "div",
    null,
    isPlan &&
      data.behavior.params.steps.map((step, i) =>
        h(CommandStep, {
          key: itemKey(step),
          step,
          placeholder: typePlaceholder(step.type),
          onChange: () => {
            syncPlanJson();
            renderSteps();
          },
          remove: () => {
            data.behavior.params.steps.splice(i, 1);
            markDirty();
            syncPlanJson();
            renderSteps();
          },
        }),
      ),
  );
  return h(
    DialogPanel,
    { className: "modal-box", onInput: trackDraft },
    h("h3", { className: "card-title" }, isNew ? "New custom command" : `Edit command: ${c.name}`),
    h(
      "div",
      { className: "row form-row" },
      field("Name", nameIn),
      field("Aliases", aliasesIn),
      field("Permission", permIn),
    ),
    field("Description", descIn),
    field("Usage", usageIn),
    isPlan
      ? h(
          "fieldset",
          { className: "fieldset field plan-steps" },
          h("legend", { className: "fieldset-legend" }, "Action plan steps"),
          stepsBox,
          h(
            ActionButton,
            {
              className: "btn btn-sm",
              onClick: () => {
                data.behavior.params.steps.push({ type: "reply", template: "" });
                markDirty();
                syncPlanJson();
                renderSteps();
              },
            },
            "+ add step",
          ),
        )
      : null,
    field("Behavior (advanced JSON)", behaviorArea),
    field("Response template (in-chat reply; {target} {reason} {response} …)", respIn),
    h(
      "div",
      { className: "row form-row" },
      field("Cooldown (secs)", cdIn),
      field("Role restriction (ids)", roleIn),
      field("Channel restriction (ids)", chanIn),
      field("Log event", logIn),
    ),
    h(
      "div",
      { className: "modal-action" },
      field("Applies to", scopeSel),
      h(
        "label",
        { className: "checkbox-label" },
        h(Input, {
          type: "checkbox",
          defaultChecked: data.enabled,
          onChange: (e) => (data.enabled = e.target.checked),
          className: "checkbox",
        }),
        "Enabled",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-primary",
          onClick: async () => {
            try {
              if (isPlan) {
                data.behavior.params = parsePlanParams(behaviorAreaRef.current.value);
              } else {
                data.behavior = JSON.parse(behaviorAreaRef.current.value || "{}");
              }
            } catch (e) {
              toast("Invalid behavior JSON: " + e.message, "error");
              return;
            }
            data.name = nameInRef.current.value.trim().toLowerCase();
            if (!data.name) return toast("Name required", "error");
            data.aliases = aliasesInRef.current.value
              .split(",")
              .map((s) => s.trim())
              .filter(Boolean);
            data.description = descInRef.current.value;
            data.usage = usageInRef.current.value;
            data.response = respInRef.current.value;
            data.permission = permInRef.current.value;
            data.cooldown_secs = Number(cdInRef.current.value) || 0;
            data.roles = splitIds(roleInRef.current.value);
            data.channels = splitIds(chanInRef.current.value);
            data.log_event = logInRef.current.value;
            if (isPlan) {
              const steps = (data.behavior.params && data.behavior.params.steps) || [];
              for (const s of steps) {
                if (s.type === "add_role" || s.type === "remove_role") {
                  applyPlanStepInput(s, s.type, s.role_id);
                  if (!s.role_id) return toast("Role steps need a role id", "error");
                }
              }
            }
            await reloadAfter(
              () =>
                isNew
                  ? api(`/api/guilds/${state.guildId}/commands`, "POST", {
                      scope: scopeSelRef.current.value,
                      data,
                    })
                  : api(`/api/guilds/${state.guildId}/commands/${encodeURIComponent(c.name)}`, "PUT", {
                      scope: scopeSelRef.current.value,
                      data,
                    }),
              () => {
                toast("Command saved", "success");
                closeOverlay();
              },
            );
          },
        },
        "Save",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-error",
          hidden: isNew,
          onClick: async () => {
            if (!confirm(`Delete command ${c.name}?`)) return;
            // Prefer clearing a guild override (reset to global). Only delete the
            // global row when there is no override and the caller is the bot owner.
            const delScope = c.guild_override
              ? "guild"
              : c.scope === "global" && isBotOwner()
                ? "global"
                : "guild";
            await reloadAfter(
              () =>
                api(`/api/guilds/${state.guildId}/commands/${encodeURIComponent(c.name)}`, "DELETE", {
                  scope: delScope,
                }),
              () => {
                toast(c.guild_override ? "Override cleared" : "Deleted", "success");
                closeOverlay();
              },
            );
          },
        },
        "Delete",
      ),
      h(
        ActionButton,
        {
          className: "btn btn-outline",
          onClick: () => {
            if (canLeave()) closeOverlay();
          },
        },
        "Cancel",
      ),
    ),
  );
}

const TEMPLATE_VARIABLE_GROUPS = [
  ["common", "Who, where and when"],
  ["message", "Messages"],
  ["moderation", "Moderation"],
  ["role", "Roles"],
  ["thread", "Threads"],
  ["member", "Member details"],
];

// One row per variable: [name, group, meaning, example]. The examples are also the
// Templates preview's sample data, so the table and the preview always agree.
const TEMPLATE_VARIABLES = [
  ["user", "common", "The member's display name", "Alice"],
  ["mention", "common", "The member as an @mention (log posts never ping anyone)", "@alice"],
  ["user_id", "common", "The member's Discord ID", "123456789012345678"],
  ["username", "common", "The member's username", "alice"],
  ["guild", "common", "This server's ID", "987654321098765432"],
  ["channel", "common", "The channel it happened in", "#general"],
  ["time", "common", "When it happened, in UTC", "Nov 09, 2026 • 22:35:54 UTC"],
  ["emoji", "common", "The event's emoji, for your own wording", "an inbox tray for joins"],
  ["message_id", "message", "The message's ID", "123456789012345681"],
  ["message_url", "message", "A link to the message", "https://discord.com/channels/1/2/3"],
  ["content", "message", "The message text, automod evidence or other details", "FREE NITRO https://example.test"],
  ["before", "message", "The old value: earlier message text, nickname, username, thread name or voice channel", "Previous text"],
  ["after", "message", "The new value", "Updated text"],
  ["attachments", "message", "Files that were on the message", "none"],
  ["coverage", "message", "How complete the bot's copy of the message is", "retained snapshot"],
  ["count", "message", "How many messages were deleted", "3"],
  ["retained", "message", "How many of those the bot had a copy of", "2"],
  ["unavailable", "message", "How many the bot knows it had no copy of", "1"],
  ["unknown", "message", "How many the bot couldn't check", "0"],
  ["filtered", "message", "How many were left out by your ignore list", "0"],
  ["omitted", "message", "How many were left out of the attached transcript", "0"],
  ["details", "message", "Where to find the full list of deleted messages", "transcript attached"],
  ["actor", "moderation", "The moderator or member who did it", "Bob (mod)"],
  ["actor_id", "moderation", "That moderator's Discord ID", "123456789012345682"],
  ["reason", "moderation", "The reason given", "spam"],
  ["action", "moderation", "What was done", "ban"],
  ["rule", "moderation", "The automod rule that acted", "mention_spam"],
  ["rule_id", "moderation", "That rule's ID", "mention_spam"],
  ["duration", "moderation", "How long: a timeout's length, or time spent in the server", "1d"],
  ["command", "moderation", "The custom command that ran", "example"],
  ["added", "role", "Roles that were added", "@Member"],
  ["removed", "role", "Roles that were removed", "none"],
  ["role", "role", "The role, as a mention", "@Member"],
  ["role_id", "role", "The role's ID", "123456789012345679"],
  ["role_name", "role", "The role's name", "Member"],
  ["detail", "role", "What changed about the role", "Permissions updated"],
  ["thread", "thread", "The thread, as a link", "#discussion"],
  ["thread_id", "thread", "The thread's ID", "123456789012345680"],
  ["thread_name", "thread", "The thread's name", "Discussion"],
  ["notice", "thread", "A short note: whether a thread is new, or about a returning member's cases", "created"],
  ["archived", "thread", "Whether the thread is archived", "false"],
  ["locked", "thread", "Whether the thread is locked", "false"],
  ["auto_archive", "thread", "How long the thread stays open after going quiet", "1d"],
  ["invitable", "thread", "Whether members can invite others to the private thread", "true"],
  ["account_created", "member", "When the account was created", "Nov 01, 2026 • 10:00:00 UTC"],
  ["account_age", "member", "How old the account is", "1w 1d"],
  ["joined", "member", "When they joined the server", "Nov 08, 2026 • 22:35:54 UTC"],
  ["cases", "member", "Their earlier moderation cases", "Case #1: warning"],
  ["before_global", "member", "Their display name before the change", "Alice"],
  ["after_global", "member", "Their display name after the change", "Alice Example"],
].map(([name, group, meaning, example]) => ({ name, group, meaning, example }));

// Variables each event supplies besides {guild}, {time} and {emoji}. Every variable
// in a shipped template is listed here.
const TEMPLATE_EVENT_VARIABLES = {
  member_join: "user mention user_id username account_created account_age joined",
  member_leave: "user mention user_id username account_created account_age joined duration",
  member_ban: "user mention user_id username account_created account_age actor actor_id reason",
  member_unban: "user mention user_id username account_created account_age actor actor_id reason",
  member_kick: "user mention user_id actor reason",
  message_edit: "channel mention message_id message_url before after attachments coverage",
  message_delete: "channel mention message_id message_url content attachments coverage",
  message_bulk_delete: "channel count retained unavailable unknown filtered omitted details",
  automod: "mention channel content rule action reason",
  mod_action: "user mention actor reason action",
  timeout_add: "user mention user_id actor reason duration",
  timeout_remove: "user mention user_id actor",
  plan_executed: "content actor command",
  system: "content",
  member_role_update: "user mention user_id added removed",
  role_change: "action role role_id role_name detail",
  voice_join: "user mention user_id channel",
  voice_leave: "user mention user_id channel",
  voice_move: "user mention user_id channel before",
  nickname_change: "user mention user_id before after",
  username_change: "user mention user_id before after before_global after_global",
  thread_create: "mention channel thread thread_id thread_name notice",
  thread_update: "channel before thread thread_id thread_name archived locked auto_archive invitable",
  thread_delete: "channel thread thread_id thread_name",
  automod_delete: "mention channel message_id message_url content attachments coverage reason rule rule_id",
  member_rejoin: "user mention user_id notice cases",
};

// What a variable shows when the bot can't fill it in, per event: the same name can
// come from different places. These variables get the "?" mark.
const NOT_SEEN = "Shows “unavailable” if the bot didn't see the original message.";
const JOIN_UNKNOWN = "Shows “Unavailable” if the bot doesn't know when they joined.";
const BAN_AUDIT = "Shows “unavailable” if the bot can't match this in Discord's audit log. The bot needs View Audit Log.";
const NOT_IN_UPDATE = "Shows “not supplied in this update” if Discord's update left it out and the bot has no copy.";
const THREAD_PARENT = "Shows “unavailable” if Discord didn't say which channel the thread is in.";
const THREAD_STATE = "Shows “unavailable” if Discord didn't send the thread's settings.";
const TEMPLATE_VARIABLE_GAPS = {
  member_join: { joined: JOIN_UNKNOWN },
  member_leave: { joined: JOIN_UNKNOWN, duration: JOIN_UNKNOWN },
  member_ban: {
    actor: BAN_AUDIT,
    actor_id: BAN_AUDIT,
    reason: `${BAN_AUDIT} Shows “No reason provided” if the moderator gave none.`,
  },
  member_unban: {
    actor: BAN_AUDIT,
    actor_id: BAN_AUDIT,
    reason: `${BAN_AUDIT} Shows “No reason provided” if the moderator gave none.`,
  },
  member_kick: { reason: "Stays as typed if the command that logged the kick has no reason." },
  message_edit: {
    mention: "Shows “unavailable” if the bot doesn't know who wrote the message.",
    before: "Shows “unavailable” if the bot didn't see the message before the edit.",
    after: NOT_IN_UPDATE,
    attachments: NOT_IN_UPDATE,
  },
  message_delete: { mention: NOT_SEEN, content: NOT_SEEN, attachments: NOT_SEEN },
  automod: {
    channel: "Only filled in by the exact-image, repeated-text and attachment-burst rules. Other rules leave it as typed.",
    content: "Empty for the honeypot role rule, which acts on a role change rather than a message.",
  },
  timeout_add: { actor: "Only filled in when a bot command gave the timeout. For a timeout given in Discord, it stays as typed." },
  timeout_remove: { actor: "Only filled in when a bot command removed the timeout. For a timeout removed in Discord, it stays as typed." },
  plan_executed: { content: "Empty when none of the command's steps sent a reply." },
  role_change: { role_name: "Shows “unavailable” if the bot didn't know a deleted role's name." },
  thread_create: {
    mention: "Shows “unavailable” if Discord didn't say who owns the thread.",
    channel: THREAD_PARENT,
  },
  thread_update: {
    channel: THREAD_PARENT,
    before: "Shows “unavailable” if the bot didn't know the thread's name before the change.",
    archived: THREAD_STATE,
    locked: THREAD_STATE,
    auto_archive: THREAD_STATE,
    invitable: THREAD_STATE,
  },
  thread_delete: {
    channel: THREAD_PARENT,
    thread_name: "Shows “unavailable” if the bot didn't know the deleted thread's name.",
  },
  automod_delete: { mention: NOT_SEEN, content: NOT_SEEN, attachments: NOT_SEEN },
  member_rejoin: {
    cases: "Empty when the bot can't list their cases yet. {notice} then says why.",
    notice: "Empty when every case is listed.",
  },
};

function templateEventVariables(event) {
  const supplied = new Set([...(TEMPLATE_EVENT_VARIABLES[event] || "").split(" ").filter(Boolean), "guild", "time", "emoji"]);
  const gaps = TEMPLATE_VARIABLE_GAPS[event] || {};
  return TEMPLATE_VARIABLES.filter((v) => supplied.has(v.name)).map((v) => ({ ...v, gap: gaps[v.name] ?? null }));
}

const TEMPLATE_RENDERING_NOTES = [
  "Built-in templates show up in Discord as a card with one field per line. Your own text replaces those fields and becomes the card's main text.",
  "Edits and deletes only include the old message text if the bot saw the original message. The bot doesn't keep message history or look old messages up in Discord.",
  "Role, voice, nickname and thread events don't include message text.",
  "Times are shown in UTC, to the second.",
  "If an event doesn't have a variable, it stays in the post as typed, like {reason}, so mistakes are easy to spot.",
  "Each post's colored edge matches the event. Add {emoji} to your own text to show the event's emoji.",
  "The preview uses sample data and Discord's length limits.",
  "When the built-in wording improves, templates still using the old built-in text update once. Your own text is never changed.",
];

function tabTemplates() {
  return h(
    "div",
    {},
    h("h2", {}, "Event templates"),
    h(
      "p",
      { className: "muted" },
      "Customize the messages sent to your log channels. Edit a template, review the sample, then save it for the selected scope.",
    ),
    h(
      "details",
      { className: "reference-panel" },
      h("summary", {}, "Template variables"),
      h(
        "p",
        { className: "muted" },
        "Variables are swapped for real details when the bot posts. Each template lists the ones its event has; click one to add it. A variable marked ? isn't always filled in; point at it to see what shows instead.",
      ),
      ...TEMPLATE_VARIABLE_GROUPS.map(([group, label]) =>
        h(
          "div",
          { key: group, className: "template-variable-group" },
          h("h3", {}, label),
          h(
            "table",
            { className: "table table-sm" },
            h("thead", {}, h("tr", {}, h("th", {}, "Variable"), h("th", {}, "What it is"), h("th", {}, "Example"))),
            h(
              "tbody",
              {},
              ...TEMPLATE_VARIABLES.filter((v) => v.group === group).map((v) =>
                h(
                  "tr",
                  { key: v.name },
                  h("td", {}, h("code", {}, `{${v.name}}`)),
                  h("td", {}, v.meaning),
                  h("td", {}, h("code", {}, v.example)),
                ),
              ),
            ),
          ),
        ),
      ),
      h(
        "details",
        { className: "template-rendering-notes" },
        h("summary", {}, "How rendering works"),
        h("ul", {}, ...TEMPLATE_RENDERING_NOTES.map((note) => h("li", { key: note }, note))),
      ),
    ),
    ...state.config.templates.map((t) => h(TemplateCard, { key: t.event, record: t })),
  );
}

// Adds a variable at the editor's cursor through the same draft path as typing.
function insertTemplateVariable(textarea, draftKey, group, defaultValue, name) {
  const { drafts, draftBaselines } = ui.getSnapshot();
  const current = textarea.value;
  const start = textarea.selectionStart ?? current.length;
  const end = textarea.selectionEnd ?? start;
  const token = `{${name}}`;
  const value = current.slice(0, start) + token + current.slice(end);
  const baseline = String(draftBaselines[draftKey] ?? defaultValue ?? "");
  const next = { ...drafts };
  if (value === baseline) delete next[draftKey];
  else next[draftKey] = { value, group };
  // Render the new text first: React writing the controlled value moves the caret to the end.
  flushSync(() => ui.set({ drafts: next, edits: Object.keys(next).length > 0 ? "dirty" : "clean" }));
  textarea.focus();
  textarea.setSelectionRange(start + token.length, start + token.length);
}

function TemplateVariableChips({ event, textRef, draftKey, group, defaultValue }) {
  const working = useContext(BusyContext);
  return h(
    "div",
    { className: "template-variables", role: "group", "aria-label": "Insert a variable" },
    h("span", { className: "muted" }, "Insert:"),
    ...templateEventVariables(event).map((v) =>
      h(
        "button",
        {
          key: v.name,
          type: "button",
          className: `btn btn-xs template-var-chip${v.gap ? " template-var-optional" : ""}`,
          disabled: working,
          title: v.gap ? `${v.meaning}. Not always filled in: ${v.gap}` : v.meaning,
          onClick: () => insertTemplateVariable(textRef.current, draftKey, group, defaultValue, v.name),
        },
        `{${v.name}}`,
        v.gap ? h("span", { className: "template-var-mark", "aria-hidden": "true" }, "?") : null,
      ),
    ),
  );
}

// Joiners, presentation selectors and modifiers that continue an emoji sequence.
function continuesEmojiSequence(ch) {
  const cp = ch.codePointAt(0);
  return cp === 0x200d || cp === 0xfe0e || cp === 0xfe0f || cp === 0x20e3
    || (cp >= 0x1f3fb && cp <= 0x1f3ff) || (cp >= 0xe0020 && cp <= 0xe007f);
}

// Same UTF-16 budget and substitution order as the shared log renderer. Like
// the bot, a cut never splits an emoji sequence.
function clipEmbedText(text, limit) {
  if (text.length <= limit) return text;
  if (!limit) return "";
  const chars = Array.from(text);
  let end = 0;
  let units = 0;
  while (units + chars[end].length < limit) units += chars[end++].length;
  while (end > 0 && (continuesEmojiSequence(chars[end]) || chars[end - 1] === "\u200d")) end--;
  return chars.slice(0, end).join("") + "…";
}

function renderEmbedText(template, vars, limit) {
  let out = "";
  for (const chunk of template.match(/[^}]*}|[^}]+$/g) || []) {
    const start = chunk.endsWith("}") ? chunk.lastIndexOf("{") : -1;
    const key = start >= 0 ? chunk.slice(start + 1, -1) : "";
    const value = Object.hasOwn(vars, key) ? vars[key] : undefined;
    for (const text of value === undefined ? [chunk] : [chunk.slice(0, start), value]) {
      for (const ch of text) {
        if (out.length + ch.length > limit) {
          if (!limit) return "";
          // Rust pops the last scalar only when the budget is exhausted.
          if (out.length === limit) out = Array.from(out).slice(0, -1).join("");
          return out + "…";
        }
        out += ch;
      }
    }
  }
  return out;
}

// Discord preview: the bot's map emoji arrive from the API in record.preview_emoji.
function templateEmbedPreview(record, template, vars) {
  const style = record.embed_style || { title: "Log event", color: 0xffb347 };
  const emoji = record.preview_emoji || {};
  const action = (vars.action || "").trim().toLowerCase();
  const color = style.removal_color && vars.removed && vars.removed !== "none"
    ? style.removal_color : style.action_colors?.[action] ?? style.color;
  const titleEmoji = (Object.hasOwn(emoji.actions || {}, action) ? emoji.actions[action] : null) || emoji.title;
  const shipped = template === record.shipped;
  const title = clipEmbedText(shipped && titleEmoji ? `${titleEmoji} ${style.title}` : style.title, 256);
  let left = 6000 - title.length;
  const fields = [];
  let description = "";
  if (shipped) {
    for (const line of template.split("\n").slice(1)) {
      const match = line.match(/^\*\*(.*?):\*\* (.*)$/);
      if (!match || fields.length === 25 || left < 2) continue;
      const labelEmoji = Object.hasOwn(emoji.labels || {}, match[1]) ? emoji.labels[match[1]] : null;
      const name = clipEmbedText(labelEmoji ? `${labelEmoji} ${match[1]}` : match[1], Math.min(256, left - 1));
      left -= name.length;
      const value = clipEmbedText(renderEmbedText(match[2], vars, 1024) || "—", Math.min(1024, left));
      left -= value.length;
      fields.push({ name, value });
    }
  } else {
    // Custom wording opts in with {emoji}; values supplied by the caller win.
    const opted = template.includes("{emoji}") && titleEmoji && !Object.hasOwn(vars, "emoji")
      ? { ...vars, emoji: titleEmoji } : vars;
    description = renderEmbedText(template, opted, Math.min(4096, left));
  }
  return { title, color, description, fields };
}

function logPreviewText(text) {
  return previewMarkdown(text);
}

function LogEmbedPreview({ embed }) {
  return h("div", {
    className: "log-embed",
    style: { borderLeftColor: `#${embed.color.toString(16).padStart(6, "0")}` },
  },
  h("strong", { className: "log-embed-title" }, embed.title),
  embed.description ? h("div", { className: "log-embed-description discord-markdown" }, logPreviewText(embed.description)) : null,
  ...embed.fields.map((field, key) => h("div", { className: "log-embed-field", key },
    h("strong", {}, field.name), h("div", { className: "discord-markdown" }, logPreviewText(field.value)),
  )));
}

function TemplateCard({ record: t }) {
  const textRef = useRef(null);
  const text = h(Textarea, {
    ref: textRef,
    draftKey: `template:${t.event}:text`,
    className: "mono template-input textarea",
    rows: 8,
    spellCheck: false,
    defaultValue: t.effective,
  });
  const template = ui.getSnapshot().drafts[`template:${t.event}:text`]?.value ?? t.effective;
  // {emoji} stays with the preview's own emoji rule.
  const samples = Object.fromEntries(TEMPLATE_VARIABLES.filter((v) => v.name !== "emoji").map((v) => [v.name, v.example]));
  const preview = templateEmbedPreview(t, template, {
    ...samples,
    action: t.event === "role_change" ? "updated" : "ban",
    notice: t.event === "thread_create" ? "created" : "Prior moderation",
  });
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef, "This server", "Global", true, `template:${t.event}:scope`);
  return h(
    DraftCard,
    { group: `template:${t.event}`, className: "card bg-base-100 card-border template-card" },
    h(
      "div",
      { className: "card-body" },
      eventHeading(t.event),
      h(
        "div",
        { className: "template-workspace" },
        h(
          "div",
          { className: "template-editor" },
          field("Message template", text),
          h(TemplateVariableChips, {
            event: t.event,
            textRef,
            draftKey: `template:${t.event}:text`,
            group: `template:${t.event}`,
            defaultValue: t.effective,
          }),
        ),
        h(
          "div",
          { className: "template-preview" },
          h("strong", {}, "Preview"),
          h(LogEmbedPreview, { embed: preview }),
        ),
      ),
      h(
        "div",
        { className: "editor-footer" },
        field("Save to", scopeSel),
        h(
          "div",
          { className: "row" },
          h(
            ActionButton,
            {
              className: "btn btn-primary btn-sm",
              onClick: async () => {
                await reloadAfter(
                  () =>
                    api(`/api/guilds/${state.guildId}/templates/${t.event}`, "PUT", {
                      scope: scopeSelRef.current.value,
                      template: textRef.current.value,
                    }),
                  () => toast("Template saved", "success"),
                );
              },
            },
            "Save",
          ),
          h(
            ActionButton,
            {
              className: "btn btn-sm",
              onClick: async () => {
                await reloadAfter(
                  () =>
                    api(`/api/guilds/${state.guildId}/templates/${t.event}`, "DELETE", { scope: "guild" }),
                  () => toast("Reset", "success"),
                );
              },
            },
            "Reset to global",
          ),
        ),
      ),
    ),
  );
}

function tabLogChannels() {
  useEffect(() => {
    const guildId = state.guildId, enrollmentId = state.config?.enrollment_id;
    const visibility = () => {
      pauseLogChannelPolls(document.visibilityState !== "visible");
      resumeLogChannelPolls();
    };
    document.addEventListener("visibilitychange", visibility);
    resumeLogChannelPolls();
    return () => {
      document.removeEventListener("visibilitychange", visibility);
      pauseLogChannelPolls(state.tab !== "logchannels" || state.guildId !== guildId ||
        state.config?.enrollment_id !== enrollmentId);
    };
  }, [state.guildId, state.config?.enrollment_id]);
  return h(
    "div",
    {},
    h(
      "div",
      { className: "row title-row" },
      h("h2", {}, "Log channels"),
      h(
        ActionButton,
        {
          className: "btn",
          onClick: async () => {
            if (!canLeave()) return;
            if (await loadConfig()) renderTab();
          },
        },
        "Refresh configuration",
      ),
    ),
    h("p", { className: "muted" }, "Pick a channel in this server for each kind of event report."),
    h(GoodToKnow, {
      items: [
        "Paste a channel ID, #mention or channel link.",
        "A change shows as queued until the bot handles it or it expires. It then updates by itself.",
        "Bot actions shows the full result of each change.",
      ],
    }),
    ...state.config.log_channels.map((lc) => h(LogChannelCard, { key: lc.event, lc })),
  );
}

function LogChannelCard({ lc }) {
  const guildId = state.guildId;
  const enrollmentId = state.config.enrollment_id;
  const inputRef = useRef(null);
  const input = h(Input, {
    ref: inputRef,
    draftKey: `log:${lc.event}:channel`,
    type: "text",
    defaultValue: lc.effective && String(lc.effective) !== "0" ? String(lc.effective) : "",
    placeholder: "Channel ID, #mention or link",
    className: "input",
  });
  const ignoredUsersRef = useRef(null);
  const ignoredUsers = h(Textarea, {
    ref: ignoredUsersRef,
    draftKey: `log:${lc.event}:users`,
    placeholder: "One Discord user or bot ID per line",
    rows: 2,
    className: "textarea",
    defaultValue: (lc.ignore?.users || []).join("\n"),
  });
  const ignoredChannelsRef = useRef(null);
  const ignoredChannels = h(Textarea, {
    ref: ignoredChannelsRef,
    draftKey: `log:${lc.event}:channels`,
    placeholder: "One source channel ID per line",
    rows: 2,
    className: "textarea",
    defaultValue: (lc.ignore?.channels || []).join("\n"),
  });
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef, "This server", "Global", false);
  return h(
    DraftCard,
    { group: `log:${lc.event}`, className: "card bg-base-100 card-border log-channel-card" },
    h(
      "div",
      { className: "card-body" },
      h(
        "div",
        { className: "row title-row" },
        eventHeading(lc.event),
        lc.pending_action
          ? h(
              "span",
              { className: "badge badge-soft badge-sm badge-warning" },
              ["pending", "claimed", "running"].includes(lc.pending_action.state)
                ? "queued"
                : lc.pending_action.state,
            )
          : null,
      ),
      h(
        "div",
        { className: "routing-fields" },
        field("Destination channel", input),
        field("Applies to", scopeSel),
        h(
          "div",
          { className: "row routing-actions" },
          h(
            ActionButton,
            {
              className: "btn btn-sm btn-primary",
              savedFields: [`log:${lc.event}:channel`],
              onClick: async () => {
                const v = parseSnowflake(inputRef.current.value);
                if (!v) return toast("Enter a Discord channel ID, <#id> mention, or channel URL", "error");
                const job = await submitBotAction(`/api/guilds/${guildId}/logchannels/${lc.event}`, "PUT", {
                  scope: scopeSelRef.current.value,
                  channel_id: v,
                });
                if (applyQueuedLogChannel(job)) {
                  renderTab();
                  if (["pending", "claimed", "running"].includes(job.state)) scheduleLogChannelReconcile(job);
                  else if (job.state !== "succeeded") await reconcileQueuedLogChannel(job);
                }
              },
            },
            "Set",
          ),
          h(
            ActionButton,
            {
              className: "btn btn-sm",
              savedFields: [`log:${lc.event}:channel`],
              onClick: async () => {
                const job = await submitBotAction(
                  `/api/guilds/${guildId}/logchannels/${lc.event}`,
                  "DELETE",
                  {
                    scope: scopeSelRef.current.value,
                  },
                );
                if (applyQueuedLogChannel(job)) {
                  renderTab();
                  if (["pending", "claimed", "running"].includes(job.state)) scheduleLogChannelReconcile(job);
                  else if (job.state !== "succeeded") await reconcileQueuedLogChannel(job);
                }
              },
            },
            "Clear",
          ),
        ),
      ),
      h(
        "div",
        { className: "log-ignore-section" },
        h("h4", {}, "Event ignores"),
        h(
          "p",
          { className: "field-help" },
          "Skip reports involving these users or source channels. Leave empty to report all.",
        ),
        h(
          "div",
          { className: "ignore-fields" },
          field("Ignore user or bot IDs", ignoredUsers),
          field("Ignore source channel IDs", ignoredChannels),
        ),
        h(
          ActionButton,
          {
            className: "btn btn-sm",
            savedFields: [`log:${lc.event}:users`, `log:${lc.event}:channels`],
            onClick: async () => {
              const users = ignoredUsersRef.current.value
                .split(/\r?\n|,/)
                .map((id) => id.trim())
                .filter(Boolean)
                .map((id) => parseLogFilterId(id, "user"));
              if (users.some((id) => !id)) return toast("Enter valid Discord user or bot IDs", "error");
              const channels = ignoredChannelsRef.current.value
                .split(/\r?\n|,/)
                .map((id) => id.trim())
                .filter(Boolean)
                .map((id) => parseLogFilterId(id, "channel"));
              if (channels.some((id) => !id))
                return toast("Enter a valid source channel ID, <#id> mention, or channel URL", "error");
              const saved = await api(`/api/guilds/${guildId}/logfilters/${lc.event}`, "PUT", {
                users,
                channels,
              }, undefined, async () => {
                const config = await apiRead(`/api/guilds/${guildId}/config`);
                if (config.enrollment_id !== enrollmentId) {
                  throw new Error("Bot membership changed. Reload and review this server before saving again.");
                }
                return { ignore: config.log_channels.find(row => row.event === lc.event).ignore };
              });
              lc.ignore = saved.ignore;
              toast("Ignore list saved", "success");
              renderTab();
            },
          },
          "Save ignores",
        ),
      ),
    ),
  );
}

function actionIsLogChannel(job) {
  return job && job.action && job.action.kind === "set_log_channel";
}

function actionCanCancel(job) {
  if (!job || (actionIsLogChannel(job) && !canEditSettings())) return false;
  if (job.state === "pending" || job.state === "claimed") return true;
  return actionIsLogChannel(job) && (job.state === "running" || job.state === "uncertain");
}

function actionCancelLabel(job) {
  return job && (job.state === "running" || job.state === "uncertain") ? "Force cancel" : "Cancel";
}

function actionCancelNeedsConfirm(job) {
  return actionIsLogChannel(job) && (job.state === "running" || job.state === "uncertain");
}

// Gap reasons are stored by the bot. Connection reasons get a label; anything
// else is a member or ban change the bot could not save.
const MEMBER_CONNECTION_GAPS = {
  "bot restarted": "planned bot restart",
  "bot stopped unexpectedly": "the bot stopped unexpectedly",
  "gateway continuity interrupted": "Discord connection was interrupted",
  "guild gateway unavailable": "Discord reported the server as unavailable",
};

function memberGapCause(reason, when) {
  const label = Object.hasOwn(MEMBER_CONNECTION_GAPS, reason || "") ? MEMBER_CONNECTION_GAPS[reason] : null;
  return label
    ? `The bot was offline or reconnected${when} (${label})`
    : `The bot could not save a member or ban change${when}`;
}

function memberInstant(value) {
  if (value === null || value === undefined || value === "") return null;
  const date = new Date(typeof value === "string" && !/^\d+$/.test(value) ? value : Number(value));
  return Number.isNaN(date.getTime()) ? null : date;
}

// One zone label across the Members view: "PT" rather than PDT beside PST.
function memberTime(value) {
  const date = memberInstant(value);
  if (!date) return "Unknown";
  const options = { year: "numeric", month: "short", day: "numeric", hour: "numeric", minute: "2-digit" };
  try {
    return date.toLocaleString(undefined, { ...options, timeZoneName: "shortGeneric" });
  } catch {
    return date.toLocaleString(undefined, options);
  }
}

function memberDuration(ms) {
  const minutes = Math.round(ms / 60000);
  if (minutes < 1) return "under 1 min";
  if (minutes < 60) return `${minutes} min`;
  const rest = minutes % 60;
  return `${Math.floor(minutes / 60)} h${rest ? ` ${rest} min` : ""}`;
}

// Cursor pages have no total; the last page reveals it.
function memberPageLabel(number, offset, rows, last) {
  if (!rows) return number === 1 ? "No results" : `Page ${number} · no results`;
  const end = (offset + rows).toLocaleString();
  return `Page ${number} · ${(offset + 1).toLocaleString()}–${end}${last ? ` of ${end}` : ""}`;
}

const memberSyncStatusLabels = {
  queued: "Queued",
  running: "Running",
  waiting: "Waiting",
  paused: "Paused",
  needs_attention: "Needs attention",
  complete: "Complete",
};

function tabMembers() {
  const guildId = state.guildId;
  const enrollmentId = state.config?.enrollment_id;
  const handlers = useRef({});
  const [syncView, setSyncView] = useState({
    coverage: null,
    summary: [h("p", { key: "loading" }, "Loading import status…")],
    notes: [],
    details: null,
    error: null,
  });
  const [memberRows, setMemberRows] = useState([]);
  const [detailParts, setDetailParts] = useState([]);
  const [pagination, setPagination] = useState({ first: true, next: true, label: "" });
  const searchRef = useRef(null);
  const search = h(Input, {
    ref: searchRef,
    onInput: () => handlers.current.queueFilteredList?.(),
    onBlur: () => handlers.current.loadChangedFilters?.(),
    type: "search",
    placeholder: "User ID, mention, display name or username",
    className: "input",
  });
  const statusFilterRef = useRef(null);
  const statusFilter = h(
    Select,
    {
      ref: statusFilterRef,
      onChange: () => handlers.current.loadChangedFilters?.(),
      className: "select",
    },
    ...[
      ["current", "Current members"],
      ["banned", "Currently banned"],
      ["previously_banned", "Previously banned"],
      ["all", "All retained"],
    ].map(([value, label]) => h("option", { value: value }, label)),
  );
  const joinedFromRef = useRef(null);
  const joinedFrom = h(Input, {
    ref: joinedFromRef,
    onInput: () => handlers.current.queueFilteredList?.(),
    onBlur: () => handlers.current.loadChangedFilters?.(),
    type: "date",
    className: "input",
  });
  const joinedToRef = useRef(null);
  const joinedTo = h(Input, {
    ref: joinedToRef,
    onInput: () => handlers.current.queueFilteredList?.(),
    onBlur: () => handlers.current.loadChangedFilters?.(),
    type: "date",
    className: "input",
  });
  const sortRef = useRef(null);
  const sort = h(
    Select,
    {
      ref: sortRef,
      onChange: () => handlers.current.loadChangedFilters?.(),
      className: "select",
    },
    h("option", { value: "joined_newest" }, "Newest join first"),
    h("option", { value: "joined_oldest" }, "Oldest join first"),
  );
  useEffect(() => {
    const alive = () =>
      state.guildId === guildId && state.config?.enrollment_id === enrollmentId && state.tab === "members";
    let listRequest = 0,
      statusRequest = 0,
      detailRequest = 0;
    let listAbort = null,
      statusAbort = null,
      detailAbort = null;
    let timer = null,
      countdownTimer = null,
      debounce = null,
      lifecycle = "active",
      after = "",
      nextAfter = null;
    // The page a pending list read will show, and the page currently shown.
    let requestedPage = { number: 1, offset: 0 },
      shownPage = { number: 1, offset: 0, rows: 0 };
    let latestStatus = null,
      latestStatusRevision = -1,
      statusRead = "current",
      lastStatusError = null;
    let latestStatusReceivedAt = 0;
    let pollAttempt = 0, pollDueAt = 0;
    let syncControl = { kind: "idle" };
    let syncActionOutcome = null;
    const controlPending = () => syncControl.kind !== "idle";
    const observeControl = (job, operation) => {
      syncControl = {
        kind: job.state === "succeeded" ? "awaiting-status" : "awaiting-receipt",
        job: { id: job.id, operation, state: job.state },
      };
    };
    let emptyMembersCell = null;
    const emptyMembersText = () =>
      latestStatus?.status === "complete"
        ? "No members match these filters."
        : "No saved members match yet. Import may still be incomplete.";
    const aliveHere = () => lifecycle === "active" && alive();
    const clearTimer = () => {
      if (timer !== null) clearTimeout(timer);
      timer = null;
    };
    const clearCountdown = () => {
      if (countdownTimer !== null) clearTimeout(countdownTimer);
      countdownTimer = null;
    };
    const auditPaginationStalled = (sync) =>
      sync?.audit_recovery_status === "needs_attention" &&
      sync?.audit_recovery_reason === "pagination_stalled";
    const memberSyncRetryEnabled = (sync) =>
      ["needs_attention", "waiting"].includes(sync?.status) ||
      (sync?.status === "complete" && auditPaginationStalled(sync));
    const schedulePoll = () => {
      clearTimer();
      if (!aliveHere()) return;
      const active =
        statusRead === "retry-needed" ||
        controlPending() ||
        (latestStatus &&
          (["queued", "running", "waiting"].includes(latestStatus.status) ||
            ["queued", "running", "waiting"].includes(latestStatus.audit_recovery_status)));
      if (!active) {
        pollDueAt = 0;
        return;
      }
      if (!pollDueAt) {
        let delay = Math.min(10000 * 2 ** Math.min(pollAttempt, 5), 300000);
        const statusNow = Number(latestStatus?.as_of) || latestStatusReceivedAt;
        const elapsed = Date.now() - latestStatusReceivedAt;
        const attempts = [];
        if (["queued", "running", "waiting"].includes(latestStatus?.status))
          attempts.push(Number(latestStatus.next_attempt_at) || 0);
        if (["queued", "running", "waiting"].includes(latestStatus?.audit_recovery_status))
          attempts.push(Number(latestStatus.audit_recovery_next_attempt_at) || 0);
        // A queued control can change the schedule before the worker's next page.
        if (attempts.length && !controlPending())
          delay = Math.max(delay, Math.min(...attempts) - statusNow - elapsed);
        delay = Math.max(delay, lastStatusError?.retryAfterMs || 0);
        pollDueAt = Date.now() + delay;
      }
      if (document.visibilityState !== "visible") return;
      timer = setTimeout(() => {
        timer = null;
        if (Date.now() < pollDueAt) return schedulePoll();
        pollDueAt = 0;
        ++pollAttempt;
        loadStatus();
      }, Math.min(Math.max(0, pollDueAt - Date.now()), 2147483647));
    };
    const asDate = (value) => memberTime(value);
    const renderStatus = (sync) => {
      clearCountdown();
      const complete = sync.status === "complete";
      // Before the bot admits the first job, or after its metadata is lost, there is no run to describe.
      const noJob = !sync.run_id && ["queued", "needs_attention"].includes(sync.status);
      const summary = [];
      const notes = [];
      const title = sync.run_kind === "reconcile" ? "Reconcile from Discord" : "Initial member import";
      const phases = ["members", "bans", "verify", "audit", "local_history"];
      const phaseLabels = {
        members: "Members",
        bans: "Current bans",
        verify: "Departure checks",
        audit: "Audit log entries",
        local_history: "Local case history",
      };
      const currentPhase =
        sync.phase === "complete" && sync.status !== "complete" && sync.local_history_complete === false
          ? phases.indexOf("local_history")
          : phases.indexOf(String(sync.phase || ""));
      const statusLabel = memberSyncStatusLabels[sync.status || "needs_attention"] || String(sync.status);
      const statusTone =
        { complete: "badge-success", needs_attention: "badge-error", paused: "badge-warning", waiting: "badge-warning" }[
          sync.status
        ] || "badge-info";
      const phaseCounts = {
        members: sync.member_processed,
        bans: sync.ban_processed,
        verify: sync.verify_processed,
        audit: sync.audit_processed,
      };
      // Local case history records only completion, never a committed count.
      const phaseNotCounted = (phase) => phase === "local_history";
      const processed = Number(sync.processed ?? sync.committed_count ?? sync.committed ?? 0);
      const currentPhaseKey = phases[currentPhase];
      const phaseProcessed = phaseCounts[currentPhaseKey] == null ? null : Number(phaseCounts[currentPhaseKey]);
      const total = sync.total_estimate == null ? null : Number(sync.total_estimate);
      const percent =
        sync.status === "complete"
          ? 100
          : sync.percent == null
            ? null
            : Math.min(99, Math.max(0, Number(sync.percent)));
      const totalKind = sync.total_is_estimated === false ? "counted" : "estimated";
      const memberPhase = sync.phase === "members";
      let totalDescription = "current phase total unknown";
      if (total !== null && sync.total_is_estimated === false)
        totalDescription = `${total.toLocaleString()} members counted`;
      else if (total !== null && memberPhase)
        totalDescription = `approximately ${total.toLocaleString()} members estimated`;
      const ranFor = !!sync.started_at && Number(sync.last_success_at) >= Number(sync.started_at);
      if (complete) {
        // A finished run is history: one line with its counts, date and duration.
        const counts = [];
        const members = sync.member_processed ?? (sync.total_is_estimated === false ? sync.total_estimate : null);
        if (members != null) counts.push(`${Number(members).toLocaleString()} members`);
        if (sync.ban_processed != null) counts.push(`${Number(sync.ban_processed).toLocaleString()} bans`);
        const verb = sync.run_kind === "reconcile" ? "Reconciled" : "Imported";
        const finishedAt = memberInstant(
          sync.last_success_at ??
            (sync.run_kind === "reconcile" ? null : (sync.initial_completed_at_ms ?? sync.initial_completed_at)),
        );
        summary.push(
          h(
            "p",
            { role: "status" },
            counts.length ? `${verb} ${counts.join(" and ")}` : `${title} complete`,
            finishedAt ? " on " : null,
            finishedAt
              ? h(
                  "time",
                  { dateTime: finishedAt.toISOString() },
                  finishedAt.toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" }),
                )
              : null,
            ranFor ? ` (${memberDuration(Number(sync.last_success_at) - Number(sync.started_at))})` : null,
          ),
        );
      } else if (noJob && sync.status === "queued") {
        summary.push(
          h(
            "p",
            { role: "status", className: "muted" },
            "The bot will create the first import job when it is online. Import controls become available after that job exists.",
          ),
        );
      } else {
        const progressText =
          percent === null ? "" : ` · ${Math.round(percent)}% of ${totalKind} member phase`;
        summary.push(
          h(
            "p",
            { role: "status", className: "row" },
            h("span", { className: `badge badge-soft ${statusTone}` }, statusLabel),
            h(
              "span",
              null,
              `${title}${currentPhase >= 0 ? ` · Stage: ${phaseLabels[currentPhaseKey]}` : ""}${progressText}`,
            ),
            sync.status_reason ? h("span", { className: "muted" }, sync.status_reason) : null,
          ),
        );
      }
      if (noJob && sync.status === "needs_attention") {
        notes.push(
          h(
            "p",
            { className: "alert alert-soft alert-warning" },
            "Import metadata is unavailable. Controls are disabled until the saved job is restored or repaired.",
          ),
        );
      }
      const serverReadAt = Number(sync.as_of ?? sync.as_of_ms);
      const statusNow =
        Number.isFinite(serverReadAt) && serverReadAt > 0
          ? serverReadAt + Math.max(0, Date.now() - latestStatusReceivedAt)
          : Date.now();
      if (!complete && !noJob) {
        notes.push(
          h("progress", {
            max: "100",
            value: percent,
            "aria-label": "Import progress",
            "aria-valuetext":
              percent === null
                ? phaseNotCounted(currentPhaseKey)
                  ? `${phaseLabels[currentPhaseKey]} stage; records not counted`
                  : `${phaseProcessed == null ? "Unknown number of" : phaseProcessed} records committed in current phase; total unknown`
                : `${Math.round(percent)} percent of ${totalKind} member stage`,
          }),
        );
        const elapsed =
          sync.elapsed_seconds ??
          (sync.started_at ? Math.max(0, (statusNow - Number(sync.started_at)) / 1000) : null);
        const workerUnconfirmed = ["worker_unconfirmed", "worker_offline"].includes(sync.estimate_reason);
        const forecastUnknown =
          workerUnconfirmed ||
          ["paused", "needs_attention"].includes(sync.status) ||
          (sync.status === "waiting" && sync.estimate_reason !== "rate_limited") ||
          sync.estimate_reason === "discord_unavailable";
        const phaseFinish = Number(sync.phase_finish_at);
        const forecastAvailable =
          !forecastUnknown &&
          sync.phase_remaining_seconds != null &&
          sync.phase_finish_at != null &&
          Number.isFinite(phaseFinish) &&
          phaseFinish > statusNow;
        const remaining = workerUnconfirmed
          ? "Unknown until worker activity is confirmed"
          : !forecastAvailable
            ? "Unknown"
            : `about ${Math.ceil((phaseFinish - statusNow) / 60000)} minutes for this phase`;
        const overallFinish = Number(sync.overall_finish_at);
        const finish = forecastAvailable
          ? asDate(
              sync.overall_finish_at != null && Number.isFinite(overallFinish) && overallFinish > statusNow
                ? sync.overall_finish_at
                : sync.phase_finish_at,
            )
          : "Unknown";
        notes.push(
          h(
            "p",
            { className: "muted" },
            `Elapsed: ${elapsed == null ? "Unknown" : `${Math.round(Number(elapsed) / 60)} minutes`} · Remaining: ${remaining} · Expected finish: ${finish}`,
          ),
        );
      }
      if (
        sync.phase === "members" &&
        sync.phase_finish_at != null &&
        sync.estimate_reason?.startsWith("preliminary member-stage estimate")
      ) {
        notes.push(
          h(
            "p",
            { className: "muted" },
            "Preliminary member-stage estimate; later stages have unknown totals.",
          ),
        );
      }
      if (sync.estimate_reason === "worker_unconfirmed")
        notes.push(
          h(
            "p",
            { className: "muted" },
            "Worker activity is not recently confirmed. The bot may be between paced pages; timing will resume when activity is confirmed.",
          ),
        );
      if (sync.estimate_reason === "worker_offline")
        notes.push(
          h("p", { className: "muted" }, "The bot worker is offline. Timing will resume when it is online."),
        );
      if (sync.next_attempt_at) {
        const nextAttempt = Number(sync.next_attempt_at);
        const rateLimited =
          sync.status === "waiting" &&
          (sync.status_reason === "rate_limited" || sync.estimate_reason === "rate_limited");
        let retryText = `Next attempt: ${asDate(sync.next_attempt_at)}`;
        if (rateLimited && Number.isFinite(nextAttempt)) {
          const seconds = Math.max(0, Math.ceil((nextAttempt - statusNow) / 1000));
          retryText = `${seconds ? `Rate limit: retry in ${Math.floor(seconds / 60)}m ${seconds % 60}s` : "Rate limit: retry due now"} · ${retryText}`;
          if (
            seconds &&
            aliveHere() &&
            document.visibilityState === "visible" &&
            statusRead !== "retry-needed" &&
            latestStatus === sync
          ) {
            countdownTimer = setTimeout(() => {
              countdownTimer = null;
              if (aliveHere() && latestStatus === sync) renderStatus(sync);
            }, 1000);
          }
        }
        notes.push(h("p", { className: "muted" }, retryText));
      }
      function syncControlButton(operation, label) {
        // Mirror the API's validate_member_sync_operation: omit controls this state can never accept.
        const valid = {
          pause: ["queued", "running", "waiting"].includes(sync.status),
          resume: sync.status === "paused",
          retry: memberSyncRetryEnabled(sync),
          reconcile: sync.status === "complete" && !!sync.initial_completed_at,
        }[operation];
        if (!valid) return null;
        const disabled = !sync.run_id || controlPending();
        return h(
          ActionButton,
          {
            className: "btn btn-sm",
            disabled: disabled,
            onClick: async () => {
              if (!latestStatus?.run_id) return;
              if (
                operation === "reconcile" &&
                !confirm(
                  "Request a full current-member and current-ban reconciliation from Discord? This can take a while.",
                )
              )
                return;
              let job;
              try {
                job = await submitBotAction(`/api/guilds/${guildId}/member-sync/actions`, "POST", {
                  operation,
                });
              } catch (error) {
                if (
                  error.status !== 409 ||
                  !["member_sync_missing_job", "member_sync_invalid_operation"].includes(error.code)
                )
                  throw error;
                if (!aliveHere()) return;
                syncActionOutcome = error.message;
                renderStatus(latestStatus);
                loadStatus();
                return;
              }
              if (!aliveHere()) return;
              syncActionOutcome = null;
              if (["pending", "claimed", "running", "succeeded"].includes(job.state)) {
                observeControl(job, operation);
              } else {
                syncActionOutcome = `${label} action ${job.state}. Check Bot actions for details.`;
              }
              renderStatus(latestStatus);
              schedulePoll();
              loadStatus();
            },
          },
          label,
        );
      }
      // Live coverage is its own status line or banner, separate from the import summary.
      // A short planned restart clears itself when audit recovery finishes, unless
      // that recovery stopped for attention. Anything else needs Reconcile.
      const autoClearing = sync.gap_auto_clear && sync.audit_recovery_status !== "needs_attention";
      const reconcileInNotice = sync.reconciliation_needed && !autoClearing && !isPreview() && canManageServer();
      const reconcileButton = reconcileInNotice && syncControlButton("reconcile", "Reconcile from Discord");
      let coverage;
      if (sync.reconciliation_needed) {
        const when = sync.gap_at ? ` at ${asDate(sync.gap_at)}` : "";
        coverage = autoClearing
          ? h(
              "p",
              { id: "member-coverage-notice", className: "muted", role: "status" },
              `The bot restarted${when}. Ban and unban actions from the restart are being caught up automatically; no action is needed.`,
            )
          : h(
              "div",
              { id: "member-coverage-notice", className: "alert alert-soft alert-warning alert-wrap", role: "status" },
              h(
                "div",
                {},
                h("strong", {}, "Member list may be out of date"),
                h(
                  "p",
                  {},
                  `${memberGapCause(sync.reconciliation_reason, when)}, so it may have missed some joins, leaves, or bans. Select Reconcile from Discord to re-check the full member and ban lists. This can take a while on large servers.`,
                ),
              ),
              reconcileButton && h("div", { className: "row" }, reconcileButton),
            );
      } else {
        coverage = h(
          "p",
          { id: "member-coverage-notice", className: "muted", role: "status" },
          complete
            ? "Member list: up to date"
            : [
                "Member list: ",
                h("span", { key: "partial", className: "badge badge-soft badge-sm badge-warning" }, "partial"),
                " ",
                // Not live: opening the explanation must not re-announce the status line.
                h(
                  "span",
                  { key: "help", "aria-live": "off" },
                  h(
                    InfoTip,
                    { id: "member-coverage-help", label: "About partial results" },
                    "Results stay partial until the import finishes. Older join or ban dates may be missing.",
                  ),
                ),
              ],
        );
      }
      if (sync.audit_recovery_status && sync.audit_recovery_status !== "complete") {
        const phase =
          sync.audit_recovery_phase === "ban"
            ? "Ban actions"
            : sync.audit_recovery_phase === "unban"
              ? "Unban actions"
              : "Ban and unban actions";
        notes.push(
          h(
            "p",
            { className: "muted" },
            `Bounded audit recovery: ${phase} · ${sync.audit_recovery_status} · ${Number(sync.audit_recovery_processed || 0).toLocaleString()} entries checked${sync.audit_recovery_reason ? ` · ${sync.audit_recovery_reason}` : ""}`,
          ),
        );
        if (sync.audit_recovery_next_attempt_at)
          notes.push(
            h(
              "p",
              { className: "muted" },
              `Audit recovery next attempt: ${asDate(sync.audit_recovery_next_attempt_at)}`,
            ),
          );
        if (sync.audit_recovery_last_progress_at)
          notes.push(
            h(
              "p",
              { className: "muted" },
              `Audit recovery last progressed: ${asDate(sync.audit_recovery_last_progress_at)}`,
            ),
          );
      }
      if (controlPending())
        notes.push(
          h(
            "p",
            { role: "status" },
            `${syncControl.job.operation[0].toUpperCase()}${syncControl.job.operation.slice(1)} requested · bot action ${syncControl.job.state}. Waiting for the import status to reflect it.`,
          ),
        );
      if (syncActionOutcome) notes.push(h("p", { className: "err", role: "status" }, syncActionOutcome));
      if (!isPreview() && canManageServer()) {
        for (const [operation, label] of [
          ["pause", "Pause"],
          ["resume", "Resume"],
          ["retry", "Retry"],
          ["reconcile", "Reconcile from Discord"],
        ]) {
          const control = operation === "reconcile" && reconcileInNotice ? null : syncControlButton(operation, label);
          if (control) summary.push(control);
        }
      }
      // Counters and secondary times stay one click away, collapsed by default.
      let details = null;
      if (!noJob) {
        const facts = [];
        const initialCompletedAt = sync.initial_completed_at_ms ?? sync.initial_completed_at;
        if (sync.run_kind === "reconcile" && initialCompletedAt)
          facts.push(["Initial import finished", asDate(initialCompletedAt)]);
        const runCounts = [
          ["processed", "processed"],
          ["inserted", "inserted"],
          ["updated", "updated"],
          ["unchanged", "unchanged"],
        ]
          .filter(([field]) => sync[`last_success_${field}`] != null)
          .map(([field, label]) => `${Number(sync[`last_success_${field}`]).toLocaleString()} ${label}`);
        if (complete) {
          if (runCounts.length) facts.push(["Run results", runCounts.join(" · ")]);
        } else {
          if (sync.last_success_at)
            facts.push([
              "Last successful run",
              `${asDate(sync.last_success_at)}${runCounts.length ? ` · ${runCounts.join(" · ")}` : ""}`,
            ]);
          facts.push(["Last progress", asDate(sync.last_progress_at)]);
        }
        if (Number(sync.unseen_candidate_count || 0) > 0)
          facts.push(["Departure checks waiting", Number(sync.unseen_candidate_count).toLocaleString()]);
        if (sync.as_of_ms || sync.as_of) facts.push(["Status checked", asDate(sync.as_of_ms ?? sync.as_of)]);
        details = h(
          "details",
          null,
          h("summary", null, "Details"),
          h(
            DataTable,
            { headings: ["Stage", "State", "Saved"], className: "table table-sm" },
            phases.map((phase, index) =>
              h(
                "tr",
                { key: phase },
                h("td", null, phaseLabels[phase]),
                h(
                  "td",
                  null,
                  complete || (currentPhase >= 0 && index < currentPhase)
                    ? "Done"
                    : index === currentPhase
                      ? statusLabel
                      : "Pending",
                ),
                h(
                  "td",
                  null,
                  phaseNotCounted(phase)
                    ? "Not counted"
                    : phaseCounts[phase] == null
                      ? "Unknown"
                      : Number(phaseCounts[phase]).toLocaleString(),
                ),
              ),
            ),
            h(
              "tr",
              { key: "summary" },
              h("th", { scope: "row" }, "Total"),
              h(
                "td",
                { colSpan: 2 },
                `${Number.isFinite(processed) ? processed.toLocaleString() : "0"} records saved in this run · ${totalDescription}`,
              ),
            ),
          ),
          h(
            "dl",
            { className: "member-sync-details" },
            ...facts.flatMap(([label, value]) => [
              h("dt", { key: `${label}-t` }, label),
              h("dd", { key: `${label}-d` }, value),
            ]),
          ),
        );
      }
      setSyncView({ coverage, summary, notes, details, error: null });
    };
    const loadStatus = async () => {
      if (!aliveHere()) return;
      clearTimer();
      pollDueAt = 0;
      // A succeeded receipt is durable before this primary status read starts.
      // Its effect (or a later state) must therefore be visible in the result.
      const succeededId = syncControl.kind === "awaiting-status" ? syncControl.job.id : null;
      const request = ++statusRequest;
      statusAbort?.abort();
      statusAbort = new AbortController();
      try {
        const sync = await apiRead(`/api/guilds/${guildId}/member-sync`, statusAbort.signal);
        if (!aliveHere() || request !== statusRequest) return;
        const revision = Number(sync.progress_revision ?? 0);
        if (latestStatus && revision < latestStatusRevision && controlPending()) {
          syncActionOutcome =
            "Import status changed after a restore. Check Bot actions before repeating the requested control.";
          syncControl = { kind: "idle" };
        }
        if (latestStatus && (revision !== latestStatusRevision ||
          sync.status !== latestStatus.status ||
          sync.audit_recovery_status !== latestStatus.audit_recovery_status ||
          sync.audit_recovery_processed !== latestStatus.audit_recovery_processed)) pollAttempt = 0;
        statusRead = "current";
        lastStatusError = null;
        if (succeededId && syncControl.job?.id === succeededId) syncControl = { kind: "idle" };
        latestStatus = sync;
        latestStatusRevision = revision;
        latestStatusReceivedAt = Date.now();
        if (emptyMembersCell)
          setMemberRows([h("tr", null, h("td", { colSpan: 5, className: "muted" }, emptyMembersText()))]);
        renderStatus(sync);
        schedulePoll();
        if (controlPending()) {
          const pendingId = syncControl.job.id;
          try {
            const jobs = await apiRead(`/api/guilds/${guildId}/actions`, statusAbort.signal);
            if (!aliveHere() || request !== statusRequest || syncControl.job?.id !== pendingId) return;
            const job = jobs.find((item) => item.id === pendingId);
            if (job && ["failed", "uncertain", "cancelled", "expired"].includes(job.state)) {
              syncActionOutcome = `${syncControl.job.operation} action ${job.state}. Check Bot actions for details.`;
              syncControl = { kind: "idle" };
            } else if (job) {
              observeControl(job, syncControl.job.operation);
              if (job.state === "succeeded") {
                // This status was read before the receipt. Read again in causal order.
                loadStatus();
                return;
              }
            }
            renderStatus(sync);
            schedulePoll();
          } catch (error) {
            // Keep the acknowledged request visible and retry its receipt on the next poll.
          }
        }
      } catch (error) {
        if (!aliveHere() || request !== statusRequest) return;
        statusRead = "retry-needed";
        lastStatusError = error;
        if (latestStatus) renderStatus(latestStatus);
        else setSyncView({ coverage: null, summary: [], notes: [], details: null, error: null });
        setSyncView((view) => ({
          ...view,
          error: h(
            "p",
            { className: "err", role: "status" },
            latestStatus
              ? "Could not refresh import status. Showing the last saved status; retrying shortly while visible."
              : "Could not load import status. Retrying shortly while visible.",
          ),
        }));
        schedulePoll();
      } finally {
        if (request === statusRequest) statusAbort = null;
      }
    };
    const loadMembers = async () => {
      const request = ++listRequest;
      listAbort?.abort();
      listAbort = new AbortController();
      emptyMembersCell = null;
      const target = requestedPage;
      setPagination((current) => ({ ...current, label: "" }));
      setMemberRows([h("tr", {}, h("td", { colSpan: 5 }, "Loading members…"))]);
      try {
        const q = new URLSearchParams({
          limit: "50",
          status: statusFilterRef.current.value,
          sort: sortRef.current.value,
        });
        const rawSearch = String(searchRef.current.value || "").trim();
        const mention = rawSearch.match(/^<@!?(\d+)>$/);
        if (rawSearch) q.set("q", mention ? mention[1] : rawSearch);
        if (joinedFromRef.current.value) q.set("joined_from", joinedFromRef.current.value);
        if (joinedToRef.current.value) q.set("joined_to", joinedToRef.current.value);
        if (after) q.set("after", after);
        const page = await apiRead(`/api/guilds/${guildId}/members?${q}`, listAbort.signal);
        if (!aliveHere() || request !== listRequest) return;
        nextAfter = page.next_cursor || null;
        shownPage = { ...target, rows: (page.members || []).length };
        setPagination({
          first: !after,
          next: !nextAfter,
          label: memberPageLabel(shownPage.number, shownPage.offset, shownPage.rows, !nextAfter),
        });
        const rows = (page.members || []).map((member) =>
          h(
            "tr",
            {},
            h(
              "td",
              {},
              h("strong", {}, member.display_name || member.username || member.user_id),
              member.username ? h("small", { className: "muted" }, ` @${member.username}`) : null,
            ),
            h(
              "td",
              {},
              h("code", {}, member.user_id),
              " ",
              h(IconButton, {
                action: "copy",
                label: `Copy user ID ${member.user_id}`,
                onClick: () => navigator.clipboard.writeText(member.user_id),
              }),
            ),
            h("td", {}, asDate(member.joined_at_ms)),
            h(
              "td",
              {},
              member.is_banned
                ? "Currently banned"
                : member.is_current
                  ? "Current"
                  : member.ever_banned
                    ? "Previously banned"
                    : "Departed",
              member.is_banned && member.current_ban_reason
                ? h("small", { className: "muted" }, ` · Current reason: ${member.current_ban_reason}`)
                : null,
            ),
            h(
              "td",
              {},
              h(IconButton, {
                action: "view",
                label: "View history",
                onClick: () => openMember(member.user_id, member.display_name),
              }),
            ),
          ),
        );
        emptyMembersCell = rows.length
          ? null
          : h("td", { colSpan: 5, className: "muted" }, emptyMembersText());
        setMemberRows(rows.length ? rows : [h("tr", {}, emptyMembersCell)]);
      } catch (error) {
        if (!aliveHere() || request !== listRequest || error.message === "Failed to fetch") return;
        setMemberRows([
          h("tr", {}, h("td", { colSpan: 5, className: "err" }, "Could not load members. Refresh to retry.")),
        ]);
      }
    };
    const openMember = async (userId, displayName) => {
      const request = ++detailRequest;
      detailAbort?.abort();
      detailAbort = new AbortController();
      let cursor = null;
      setDetailParts([h("p", {}, "Loading member history…")]);
      const parts = [];
      const appendPage = async () => {
        const q = new URLSearchParams({ limit: "50" });
        if (cursor) q.set("after", cursor);
        const page = await apiRead(
          `/api/guilds/${guildId}/members/${userId}/history?${q}`,
          detailAbort.signal,
        );
        if (!aliveHere() || request !== detailRequest) return;
        if (!cursor)
          parts.push(
            h(
              "h3",
              { className: "card-title" },
              displayName ? `History for ${displayName} (${userId})` : `History for ${userId}`,
            ),
          );
        for (const event of page.entries || []) {
          const sourceLabel =
            {
              discord_audit: "Discord audit action",
              discord_audit_unresolved: "Confirmed Discord audit action; local case association unresolved",
              application_audit_linked: "Bot action linked to Discord audit",
              gateway_observation_unlinked: "Gateway observation; action time unconfirmed",
              state_reconciliation: "Verified from current Discord state; transition time unknown",
              application: "Bot action",
            }[event.source_kind] || "";
          const eventLabel =
            event.source_kind === "gateway_observation_unlinked"
              ? `${event.kind || "Ban state"} observation`
              : event.source_kind === "discord_audit_unresolved"
                ? `${event.kind === "ban" ? "Ban" : event.kind === "unban" ? "Unban" : "Discord"} audit entry`
                : event.kind || "Observation";
          const visibleLabel =
            event.kind === "member_state_observation"
              ? "Membership absence"
              : event.kind === "membership_state_observation"
                ? "Membership state check"
                : event.kind === "ban_present_state_observation"
                  ? "Ban present (time unknown)"
                  : event.kind === "ban_absent_state_observation" || event.kind === "ban_state_observation"
                    ? "Ban absent (time unknown)"
                    : eventLabel;
          parts.push(
            h(
              "p",
              {},
              h("strong", {}, `${visibleLabel} · ${asDate(event.occurred_at_ms)}`),
              event.case_id
                ? h(
                    React.Fragment,
                    null,
                    ` · Case ${event.case_id} `,
                    h(IconButton, {
                      action: "view",
                      label: `Open case ${event.case_id}`,
                      onClick: () => {
                        if (!aliveHere()) return;
                        pendingCase = { guildId, caseId: event.case_id };
                        if (!navigate("cases")) pendingCase = null;
                      },
                    }),
                  )
                : null,
              h(
                "span",
                { className: "muted" },
                `${sourceLabel ? ` ${sourceLabel}` : ""}${event.source_kind === "discord_audit_unresolved" && event.observation_id ? ` · Audit entry ${event.observation_id}` : ""}${event.observed_at_ms && event.source_kind === "state_reconciliation" ? ` · Checked ${asDate(event.observed_at_ms)}` : ""}${event.outcome ? ` · ${event.outcome}` : ""}${event.moderator_id ? ` · moderator ${event.moderator_id}` : ""}${event.reason ? ` · ${event.reason}` : ""}`,
              ),
            ),
          );
        }
        cursor = page.next_cursor || null;
        if (cursor) {
          setDetailParts([
            ...parts,
            h(
              ActionButton,
              {
                className: "btn btn-sm",
                onClick: async () => {
                  setDetailParts([...parts]);
                  await appendPage();
                },
              },
              "More history",
            ),
          ]);
        } else {
          if (!(page.entries || []).length)
            parts.push(h("p", { className: "muted" }, "No known events. Older history may be unavailable."));
          setDetailParts([...parts]);
        }
      };
      try {
        await appendPage();
      } catch (error) {
        if (aliveHere() && request === detailRequest && error.message !== "Failed to fetch")
          setDetailParts([h("p", { className: "err" }, "Could not load history.")]);
      }
    };
    const invalidateResults = () => {
      ++listRequest;
      listAbort?.abort();
      emptyMembersCell = null;
      ++detailRequest;
      detailAbort?.abort();
      after = "";
      nextAfter = null;
      requestedPage = { number: 1, offset: 0 };
      shownPage = { number: 1, offset: 0, rows: 0 };
      setPagination({ first: true, next: true, label: "" });
      setMemberRows([h("tr", {}, h("td", { colSpan: 5 }, "Loading members…"))]);
      setDetailParts([]);
    };
    const queueFilteredList = () => {
      if (!aliveHere()) return;
      invalidateResults();
      clearTimeout(debounce);
      debounce = setTimeout(() => {
        debounce = null;
        if (aliveHere()) loadMembers();
      }, 300);
    };
    const loadChangedFilters = () => {
      if (!aliveHere()) return;
      clearTimeout(debounce);
      debounce = null;
      invalidateResults();
      loadMembers();
    };
    handlers.current = {
      queueFilteredList,
      loadChangedFilters,
      first: () => {
        after = "";
        requestedPage = { number: 1, offset: 0 };
        loadMembers();
      },
      next: () => {
        if (nextAfter) {
          after = nextAfter;
          requestedPage = { number: shownPage.number + 1, offset: shownPage.offset + shownPage.rows };
          loadMembers();
        }
      },
      refresh: () => {
        loadStatus();
        loadMembers();
      },
    };
    const visibility = () => {
      if (document.visibilityState === "visible" && aliveHere()) {
        if (latestStatus) renderStatus(latestStatus);
        if (!pollDueAt && !statusAbort) loadStatus();
        else schedulePoll();
      } else {
        clearTimer();
        clearCountdown();
      }
    };
    document.addEventListener("visibilitychange", visibility);
    const dispose = () => {
      authLossCleanups.delete(dispose);
      lifecycle = "disposed";
      clearTimer();
      clearCountdown();
      clearTimeout(debounce);
      listAbort?.abort();
      statusAbort?.abort();
      detailAbort?.abort();
      document.removeEventListener("visibilitychange", visibility);
    };
    authLossCleanups.add(dispose);
    loadStatus();
    loadMembers();
    return dispose;
  }, [guildId, enrollmentId]);
  return h(
    "div",
    null,
    h("h2", null, "Members"),
    h(
      "p",
      { className: "muted" },
      "Search saved member identities and known join dates.",
    ),
    syncView.coverage,
    h(
      "section",
      { "aria-label": "Member import status" },
      h("div", { className: "row" }, ...syncView.summary),
      // Fixed slots keep an opened Details disclosure open across status refreshes.
      h("div", null, ...syncView.notes),
      syncView.details,
      syncView.error,
    ),
    h(
      "section",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h(
          "div",
          { className: "row form-row" },
          field("Search", search),
          field("Status", statusFilter),
          field("Joined from (UTC)", joinedFrom),
          field("Joined to (UTC)", joinedTo),
          field("Sort", sort),
        ),
        h(
          "div",
          { className: "row" },
          h(ActionButton, { className: "btn", onClick: () => handlers.current.refresh?.() }, "Refresh"),
          h(
            ActionButton,
            { className: "btn", disabled: pagination.first, onClick: () => handlers.current.first?.() },
            "First page",
          ),
          h(
            ActionButton,
            { className: "btn", disabled: pagination.next, onClick: () => handlers.current.next?.() },
            "Next page",
          ),
          h("span", { className: "muted", "aria-live": "polite" }, pagination.label),
        ),
        h(DataTable, { headings: ["Member", "User ID", "Joined", "Status", "History"] }, memberRows),
      ),
    ),
    detailParts.length
      ? h(
          "section",
          { className: "card bg-base-100 card-border", "aria-label": "Member history" },
          h("div", { className: "card-body" }, ...detailParts),
        )
      : null,
  );
}

function caseCleanupState(cleanupPending) {
  return cleanupPending ? "pending" : "complete";
}

function tabCases() {
  const guildId = state.guildId;
  const handlers = useRef({});
  const [rows, setRows] = useState([]);
  const [loading, setLoading] = useState(true);
  const [detailParts, setDetailParts] = useState([]);
  const [pagination, setPagination] = useState({ first: true, next: true });
  const [canDelete, setCanDelete] = useState(false);
  const [removedView, setRemovedView] = useState(false);
  const [handoff] = useState(() => {
    const next = pendingCase;
    pendingCase = null;
    return next;
  });
  useEffect(() => {
    let lifecycle = "active";
    let request = 0;
    let listController;
    let detailRequest = 0;
    let after = "";
    let listMode = "live";
    let nextAfter = "";
    const pageSize = 50;
    const openCase = async (id) => {
      const currentDetail = ++detailRequest;
      try {
        const payload = await apiRead(`/api/guilds/${guildId}/cases/${id}`);
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
        const evidence = await apiRead(`/api/guilds/${guildId}/cases/${id}/evidence`);
        if (lifecycle === "disposed" || currentDetail !== detailRequest || state.guildId !== guildId) return;
        const c = payload.case || {};
        let noteIntent = { kind: "empty" };
        let noteActivity = "idle";
        const noteRef = React.createRef();
        const note = h(Textarea, {
          key: `${id}:${currentDetail}`,
          ref: noteRef,
          rows: "3",
          placeholder: "Add a note. This does not change Discord.",
          className: "textarea",
        });
        setDetailParts([
          h("h3", { className: "card-title" }, "Case " + id),
          h("p", {}, `${c.action || "unknown"} · ${c.outcome || "unknown"} · subject ${c.subject_id || ""}`),
          h("p", { className: "muted" }, c.reason || "No reason recorded."),
          h(
            "p",
            { className: "muted" },
            "Removing a case deletes retained private records. It does not reverse a Discord ban, kick, timeout, or warning.",
          ),
          payload.can_delete
            ? h(
                IconButton,
                {
                  action: "remove",
                  label: "Remove case record",
                  className: "btn btn-sm btn-square standalone-action",
                  onClick: async () => {
                    if (!confirm("Remove this case record? This does not undo the Discord action.")) return;
                    const result = await api(`/api/guilds/${guildId}/cases/${id}`, "DELETE");
                    toast(
                      caseCleanupState(result?.cleanup_pending) === "pending"
                        ? "Case record removed. Cleanup is pending. Use Removed records / retry cleanup."
                        : "Case record removed",
                      "success",
                    );
                    renderTab();
                  },
                },
              )
            : null,
          h("h3", { className: "card-title" }, "History"),
          ...(history.history || []).map((entry) =>
            h("p", { className: "muted" }, JSON.stringify(entry.entry.body)),
          ),
          canManageServer() &&
          field(
            "Note",
            note,
            h(
              ActionButton,
              {
                className: "btn btn-sm",
                type: "button",
                onClick: async () => {
                  if (noteActivity === "submitting") return;
                  const text = noteRef.current.value.trim();
                  if (noteIntent.kind === "empty" || noteIntent.body.text !== text) {
                    noteIntent = { kind: "retained", body: { entry_id: crypto.randomUUID().replaceAll("-", ""), text } };
                  }
                  noteActivity = "submitting";
                  try {
                    await api(`/api/guilds/${guildId}/cases/${id}/history`, "POST", noteIntent.body);
                    noteIntent = { kind: "empty" };
                    toast("Note added", "success");
                    await openCase(id);
                    ui.set({ edits: "clean" });
                  } finally {
                    noteActivity = "idle";
                  }
                },
              },
              "Add note",
            ),
          ),
          h("h3", { className: "card-title" }, "Private evidence"),
          ...(Array.isArray(c.evidence?.text) ? c.evidence.text : []).map((item) =>
            h(
              "div",
              {},
              h(
                "p",
                { className: "muted" },
                `Message ${item.message_id} · channel ${item.channel_id} · author ${item.author_id}`,
              ),
              h("pre", {}, item.text),
            ),
          ),
          ...(evidence.evidence || []).map((item) =>
            h(
              "p",
              {},
              h("span", { className: "mono" }, item.id),
              " ",
              h(IconButton, {
                action: "download",
                label: `Download ${item.id}`,
                type: "button",
                onClick: async () => {
                  await downloadPrivate(`/api/guilds/${guildId}/cases/${id}/evidence/${item.id}`, item.id);
                },
              }),
            ),
          ),
        ]);
      } catch (e) {
        if (lifecycle === "disposed" || currentDetail !== detailRequest || state.guildId !== guildId) return;
        setDetailParts([h("p", { className: "err" }, e.message || String(e))]);
      }
    };
    const run = async () => {
      const current = ++request;
      listController?.abort();
      const controller = new AbortController();
      listController = controller;
      const signal = controller.signal;
      const timeout = setTimeout(() => controller.abort(), 30000);
      setLoading(true);
      ++detailRequest;
      setDetailParts([]);
      setPagination((value) => ({ ...value, next: true }));
      setRows([]);
      try {
        const query = new URLSearchParams({ limit: String(pageSize), removed: String(listMode === "removed") });
        if (after) query.set("after", after);
        const payload = await apiRead(`/api/guilds/${guildId}/cases?${query}`, signal);
        if (lifecycle === "disposed" || current !== request || state.guildId !== guildId) return;
        setCanDelete(!!payload.can_delete);
        const ids = payload.cases || [];
        nextAfter = ids.length === pageSize ? ids[ids.length - 1] : "";
        setPagination({ next: !nextAfter, first: !after });
        if (!ids.length) {
          setRows([
            h(
              "tr",
              {},
              h(
                "td",
                { colSpan: 5, className: "muted" },
                listMode === "removed" ? "No more removed records." : "No selected cases on this page.",
              ),
            ),
          ]);
          return;
        }
        const rows = [];
        for (const id of ids) {
          if (listMode === "removed") {
            rows.push(
              h(
                "tr",
                {},
                h("td", {}, id),
                h(
                  "td",
                  { colSpan: 4 },
                  h(
                    IconButton,
                    {
                      action: "retry",
                      label: "Retry cleanup",
                      onClick: async () => {
                        const result = await api(`/api/guilds/${guildId}/cases/${id}`, "DELETE");
                        toast(
                          caseCleanupState(result.cleanup_pending) === "pending"
                            ? "Cleanup is pending. Retry when storage is available."
                            : "Live record cleanup complete. This does not delete backup copies.",
                          "success",
                        );
                      },
                    },
                  ),
                ),
              ),
            );
            continue;
          }
          const c = payload.summaries?.[id];
          rows.push(
            h(
              "tr",
              {},
              h(
                "td",
                {},
                h("span", { className: "mono" }, id.slice(0, 8)),
                " ",
                h(IconButton, { action: "view", label: `Open case ${id}`, onClick: () => openCase(id) }),
              ),
              h("td", {}, c?.action || "Summary unavailable"),
              h("td", {}, c?.outcome || ""),
              h("td", {}, String(c?.subject_id || "")),
              h(
                "td",
                { className: "muted" },
                c?.occurred_at_ms ? new Date(Number(c.occurred_at_ms)).toLocaleString() : "",
              ),
            ),
          );
        }
        setRows(rows);
      } catch (e) {
        if (lifecycle === "disposed" || current !== request || state.guildId !== guildId) return;
        setRows([
          h(
            "tr",
            {},
            h(
              "td",
              { colSpan: 5, className: "err", role: "alert" },
              "Could not load cases: " + (signal.aborted
                ? "Request timed out. Refresh to verify saved state before trying again."
                : e.message || String(e)),
            ),
          ),
        ]);
      } finally {
        clearTimeout(timeout);
        if (lifecycle === "active" && current === request && state.guildId === guildId) setLoading(false);
      }
    };
    handlers.current = {
      run,
      first: () => {
        after = "";
        run();
      },
      next: () => {
        if (nextAfter) {
          after = nextAfter;
          run();
        }
      },
      recovery: () => {
        listMode = listMode === "removed" ? "live" : "removed";
        setRemovedView(listMode === "removed");
        after = "";
        run();
      },
    };
    run();
    if (handoff?.guildId === guildId) openCase(handoff.caseId);
    return () => {
      lifecycle = "disposed";
      listController?.abort();
      ++request;
      ++detailRequest;
    };
  }, [guildId, handoff]);
  return h(
    "div",
    null,
    h("h2", null, "Cases"),
    h("p", { className: "muted" }, "Moderation cases for this server."),
    h(GoodToKnow, {
      items: [
        "Only moderation details are kept, never ordinary messages.",
        "Removing a record doesn't undo the ban, kick, timeout or warning in Discord.",
        "Removed records can retry cleanup. Their private content stays deleted.",
      ],
    }),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      h(ActionButton, { className: "btn", onClick: () => handlers.current.run?.() }, "Refresh"),
      h(
        ActionButton,
        { className: "btn", disabled: pagination.first, onClick: () => handlers.current.first?.() },
        "First page",
      ),
      h(
        ActionButton,
        { className: "btn", disabled: pagination.next, onClick: () => handlers.current.next?.() },
        "Next page",
      ),
      h(
        ActionButton,
        {
          className: "btn",
          style: { display: canDelete ? "" : "none" },
          onClick: () => handlers.current.recovery?.(),
        },
        removedView ? "Back to cases" : "Removed records / retry cleanup",
      ),
    ),
    loading
      ? h(
        "div",
        { className: "card bg-base-100 card-border", role: "status", "aria-live": "polite" },
        h("div", { className: "card-body" }, "Loading cases…"),
      )
      : h(DataTable, { headings: ["Case", "Action", "Outcome", "Subject", "When"] }, rows),
    h(
      "div",
      { className: "card bg-base-100 card-border", style: { marginTop: "16px" } },
      ...detailParts,
    ),
  );
}

function tabSettings() {
  const cfg = state.config;
  const ov = cfg.settings_override || {};
  const prefixRef = useRef(null);
  const prefix = h(Input, {
    ref: prefixRef,
    draftKey: "settings:prefix",
    type: "text",
    defaultValue: cfg.settings.prefix,
    className: "input",
  });
  const botNameRef = useRef(null);
  const botName = h(Input, {
    ref: botNameRef,
    draftKey: "settings:bot_name",
    type: "text",
    defaultValue: cfg.settings.bot_name,
    className: "input",
  });

  const srcSelRef = useRef(null);
  const srcSel = h(
    Select,
    {
      ref: srcSelRef,
      draftKey: "copy:source",
      defaultValue: "",
      className: "select",
      onChange: (event) =>
        handleEvent(
          async () => {
            ui.set({ copySource: null });
            const guildId = srcSelRef.current.value;
            if (!guildId) return;
            const source = await apiRead(`/api/guilds/${guildId}/config`);
            if (srcSelRef.current?.value === guildId && source.enrollment_id) {
              ui.set({ copySource: { guildId, id: source.enrollment_id } });
            }
          },
          event,
        ),
    },
    h("option", { value: "" }, "Select a source server"),
    ...state.guilds
      .filter((g) => g.id !== state.guildId && (g.access || "manage") === "manage")
      .map((g) => h("option", { value: g.id }, g.name)),
  );
  const sectionSelRef = useRef(null);
  const sectionSel = h(
    Select,
    { ref: sectionSelRef, className: "select", draftKey: "copy:section", defaultValue: "all" },
    ["all", "rules", "badwords", "commands", "templates", "settings"].map((s) =>
      h("option", { value: s }, s === "all" ? "everything" : s),
    ),
  );
  const globalPrefixRef = useRef(null);
  const globalPrefix = h(Input, {
    ref: globalPrefixRef,
    draftKey: "global:prefix",
    type: "text",
    defaultValue: cfg.global_settings.prefix,
    className: "input",
  });

  /** Clear one override field via API null (inherit parent). */
  async function inheritField(field) {
    await reloadAfter(
      () => api(`/api/guilds/${state.guildId}/settings`, "PUT", { [field]: null }),
      () => toast(`Using global ${field.replaceAll("_", " ")}`, "success"),
    );
  }

  function settingsField(labelText, input, overrideKey, hint, allowInheritance = true) {
    const hasOv = ov[overrideKey] != null;
    return field(
      labelText,
      input,
      hint ? h("p", { className: "muted", style: { margin: "4px 0 0", fontSize: "12px" } }, hint) : null,
      hasOv && allowInheritance
        ? h(
            ActionButton,
            {
              className: "btn btn-sm",
              type: "button",
              savedFields: [input.props.draftKey],
              style: { marginTop: "6px" },
              onClick: () => inheritField(overrideKey),
            },
            "Use global",
          )
        : null,
    );
  }

  const cards = [
    h(
      DraftCard,
      { group: "settings", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "This server"),
        cfg.nickname_reconcile?.state === "terminal"
          ? h(
              "p",
              { className: "error" },
              `Discord nickname needs operator action: ${cfg.nickname_reconcile.last_error || cfg.nickname_reconcile.last_error_code || "permanent Discord rejection"}`,
            )
          : cfg.nickname_reconcile?.state === "dormant"
            ? h(
                "p",
                { className: "muted" },
                "Discord nickname updates are paused. Once the bot is in this server, select Re-sync Discord nickname.",
              )
            : null,
        h(
          "div",
          { className: "row form-row" },
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
        h(
          "div",
          { className: "row" },
          h(
            ActionButton,
            {
              className: "btn btn-primary",
              onClick: async () => {
                // Tri-state body: omit unchanged, null clears override, value sets.
                // Only send fields the operator actually changed — always sending
                // bot_name re-applied a stale input when global nick changed elsewhere.
                // bot_name empty is special (AGENTS): persist "" to clear Discord nick — not inherit.
                const body = {};
                const rawPrefix = String(prefixRef.current.value ?? "").trim();
                if (rawPrefix === "") {
                  if (ov.prefix != null) body.prefix = null;
                  else {
                    toast("Prefix cannot be empty (Use global to inherit)", "error");
                    return;
                  }
                } else if (rawPrefix !== cfg.settings.prefix) {
                  body.prefix = rawPrefix;
                }
                if (botNameRef.current.value !== cfg.settings.bot_name) {
                  body.bot_name = botNameRef.current.value;
                }
                if (body.prefix === undefined && body.bot_name === undefined) {
                  toast("No settings changes", "success");
                  return;
                }
                await reloadAfter(
                  () => api(`/api/guilds/${state.guildId}/settings`, "PUT", body),
                  (res) =>
                    toast(
                      res.nickname_queued ? "Settings saved — Discord nickname queued" : "Settings saved",
                      "success",
                    ),
                );
              },
            },
            "Save",
          ),
          h(
            ActionButton,
            {
              className: "btn",
              savedFields: [],
              onClick: async () => {
                await reloadAfter(
                  () => api(`/api/guilds/${state.guildId}/settings`, "PUT", { sync_nickname: true }),
                  (res) =>
                    toast(
                      res.nickname_queued
                        ? "Discord nickname reconciliation queued"
                        : "No nickname work queued",
                      "success",
                    ),
                );
              },
            },
            "Re-sync Discord nickname",
          ),
        ),
      ),
    ),
  ];

  const [roleBaseline, setRoleBaseline] = useState("");
  const roleInputRef = useRef(null);
  const roleInput = h(Input, {
    ref: roleInputRef,
    draftKey: "settings:roles",
    defaultValue: roleBaseline,
    type: "text",
    placeholder: "role id, <@&id> mention, comma-separated",
    className: "input",
  });
  const roleRevision = useRef("");
  const roleWriteGeneration = useRef(0);
  useEffect(() => {
    if (!canManageServer()) return undefined;
    let active = true;
    const initialRoleGeneration = roleWriteGeneration.current;
    apiRead(`/api/guilds/${state.guildId}/case-delete-roles`)
      .then((roles) => {
        if (!active || initialRoleGeneration !== roleWriteGeneration.current) return;
        const loadedValue = (roles.roles || []).join(", ");
        setRoleBaseline(loadedValue);
        roleRevision.current = roles.revision || "";
      })
      .catch((e) => {
        if (active && !isUnauthenticated(e)) reportApiError(e);
      });
    return () => {
      active = false;
    };
  }, []);
  if (canManageServer()) cards.push(
    h(AccessRolesCard, { key: "access-roles", revision: roleRevision, generation: roleWriteGeneration }),
    h(
      DraftCard,
      { group: "roles", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Case removal"),
        h(
          "p",
          { className: "muted" },
          "Server owners and Administrators can remove case records. Removal never undoes a Discord action.",
        ),
        field(
          "Delegated case-removal role IDs",
          roleInput,
          h("p", { className: "field-help" }, "Saved for later. These roles can't remove cases yet."),
        ),
        h(
          "div",
          { className: "card-actions" },
          h(
            ActionButton,
            {
              className: "btn",
              onClick: async () => {
                const saved = await api(`/api/guilds/${state.guildId}/case-delete-roles`, "PUT", {
                  roles: splitIds(roleInputRef.current.value),
                  revision: roleRevision.current,
                }, undefined, () => apiRead(`/api/guilds/${state.guildId}/case-delete-roles`));
                roleWriteGeneration.current++;
                roleRevision.current = saved.revision;
                setRoleBaseline(saved.roles ? saved.roles.join(", ") : roleInputRef.current.value);
                const drafts = { ...currentDrafts() };
                delete drafts["settings:roles"];
                ui.set({ drafts, edits: Object.keys(drafts).length > 0 ? "dirty" : "clean" });

                toast("Case-removal roles saved", "success");
              },
            },
            "Save case-removal roles",
          ),
        ),
      ),
    ),
  );

  if (isBotOwner()) {
    cards.push(
      h(
        DraftCard,
        { group: "global", className: "card bg-base-100 card-border" },
        h(
          "div",
          { className: "card-body" },
          h("h3", { className: "card-title" }, "Global defaults (applies to all servers)"),
          h(
            "p",
            { className: "muted", style: { marginBottom: "8px" } },
            "Every server uses these until it sets its own value.",
          ),
          h(
            "p",
            { className: "field-help" },
            "Rules, templates and commands have their own Global option in each editor.",
          ),
          h(
            "div",
            { className: "row form-actions" },
            field("Global prefix", globalPrefix),
            h(
              ActionButton,
              {
                className: "btn",
                onClick: async () => {
                  const raw = String(globalPrefixRef.current.value ?? "").trim();
                  if (raw === "") {
                    toast("Global prefix cannot be empty", "error");
                    return;
                  }
                  await reloadAfter(
                    () => api(`/api/guilds/0/settings`, "PUT", { prefix: raw }),
                    () => toast("Global prefix saved", "success"),
                  );
                },
              },
              "Save global prefix",
            ),
          ),
        ),
      ),
    );
  }

  if (canManageServer()) cards.push(
    h(
      DraftCard,
      { group: "copy", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Copy configuration from another server"),
        h(
          "p",
          { className: "muted", style: { marginBottom: "8px" } },
          "Replace part of this server's setup, or all of it, with another server you manage.",
        ),
        h("p", { className: "field-help" }, "Log channels are never copied."),
        h(
          "div",
          { className: "row" },
          srcSel,
          sectionSel,
          h(
            ActionButton,
            {
              className: "btn btn-primary",
              onClick: async () => {
                if (!ui.getSnapshot().copySource || ui.getSnapshot().copySource.guildId !== srcSelRef.current.value)
                  return toast("Select a source server to load its current configuration", "error");
                if (
                  !confirm(
                    `Copy ${sectionSelRef.current.value} from ${srcSelRef.current.selectedOptions[0].textContent}?`,
                  )
                )
                  return;
                await reloadAfter(
                  () =>
                    api(`/api/guilds/${state.guildId}/copy`, "POST", {
                      from_guild_id: String(srcSelRef.current.value),
                      from_enrollment_id: ui.getSnapshot().copySource.id,
                      sections: [sectionSelRef.current.value],
                    }),
                  (res) => {
                    let msg = `Copied: ${(res.copied || []).join(", ") || "(nothing)"}`;
                    if (res.skipped_badwords > 0) {
                      msg += ` (${res.skipped_badwords} invalid badword(s) skipped — not enforced)`;
                    }
                    if (res.skipped_logchannels) {
                      msg += " (log channels skipped — channel IDs are server-specific; set them here)";
                    }
                    if (res.nickname_queued) msg += " (Discord nickname queued)";
                    toast(msg, res.skipped_badwords > 0 || res.skipped_logchannels ? "info" : "success");
                  },
                );
              },
            },
            "Copy",
          ),
        ),
      ),
    ),
  );

  return h("div", {}, h("h2", {}, "Settings"), ...cards);
}

const ACCESS_LEVELS = [
  ["dashboard", "Dashboard (read-only)"],
  ["settings", "Dashboard and settings"],
];

// Role -> access mappings for this server. Only Manage Server sees this card;
// the API refuses the read and the save for everyone else.
// Access and case-removal roles share one server revision, so both cards use
// the same revision and write-generation refs from the Settings tab.
function AccessRolesCard({ revision, generation }) {
  const guildId = state.guildId;
  const [baselineRows, setBaselineRows] = useState(null);
  const [ttl, setTtl] = useState(30);
  const { drafts } = useStore(ui);
  const draftKey = "settings:access-roles";
  const rows = baselineRows === null ? null : drafts[draftKey]?.value ?? baselineRows;
  const withKeys = (roles) => roles.map((role) => ({ ...role, key: crypto.randomUUID() }));
  const changeRows = (next) => {
    const values = (items) => JSON.stringify(items.map(({ role_id, level }) => ({ role_id, level })));
    const nextDrafts = { ...currentDrafts() };
    if (baselineRows !== null && values(next) === values(baselineRows)) delete nextDrafts[draftKey];
    else nextDrafts[draftKey] = { value: next, group: "access-roles" };
    ui.set({ drafts: nextDrafts, edits: Object.keys(nextDrafts).length > 0 ? "dirty" : "clean" });
  };
  useEffect(() => {
    let active = true;
    const initialGeneration = generation.current;
    apiRead(`/api/guilds/${guildId}/access-roles`)
      .then((loaded) => {
        if (!active) return;
        if (initialGeneration === generation.current) revision.current = loaded.revision || "";
        setTtl(loaded.ttl_secs || 30);
        setBaselineRows(withKeys(loaded.roles || []));
      })
      .catch((e) => {
        if (active && !isUnauthenticated(e)) reportApiError(e);
      });
    return () => {
      active = false;
    };
  }, []);
  const update = (key, patch) => {
    changeRows(rows.map((row) => (row.key === key ? { ...row, ...patch } : row)));
  };
  return h(
    DraftCard,
    { group: "access-roles", className: "card bg-base-100 card-border" },
    h(
      "div",
      { className: "card-body" },
      h("h3", { className: "card-title" }, "Web UI access by role"),
      h("p", { className: "muted" }, "Members with a listed role can sign in and open this server only."),
      h(GoodToKnow, {
        items: [
          "Dashboard access is view-only.",
          "Settings access can also edit rules, badwords, known spam and images, templates, log channels and bot settings.",
          "Commands, copying configuration, member import controls, case notes, case removal and this list need Manage Server.",
          `A removed role stops working within ${ttl} seconds.`,
        ],
      }),
      rows === null
        ? h("p", { className: "muted", role: "status" }, "Loading roles…")
        : h(
            React.Fragment,
            null,
            rows.length === 0 && h("p", { className: "muted" }, "No roles have Web UI access."),
            ...rows.map((row) =>
              h(
                "div",
                { key: row.key, className: "row form-row access-role-row" },
                field(
                  "Role ID",
                  h(Input, {
                    type: "text",
                    className: "input",
                    value: row.role_id,
                    placeholder: "Discord role (ID or <@&id> mention)",
                    onChange: (event) => update(row.key, { role_id: event.target.value }),
                  }),
                ),
                field(
                  "Access",
                  h(
                    Select,
                    {
                      className: "select",
                      value: row.level,
                      onChange: (event) => update(row.key, { level: event.target.value }),
                    },
                    ACCESS_LEVELS.map(([value, label]) => h("option", { key: value, value }, label)),
                  ),
                ),
                h(IconButton, {
                  action: "remove",
                  label: "Remove role",
                  type: "button",
                  onClick: () => {
                    changeRows(rows.filter((item) => item.key !== row.key));
                  },
                }),
              ),
            ),
            h(
              "div",
              { className: "card-actions" },
              h(
                ActionButton,
                {
                  className: "btn",
                  type: "button",
                  disabled: rows.length >= 32,
                  onClick: () => changeRows([...rows, ...withKeys([{ role_id: "", level: "dashboard" }])]),
                },
                "Add role",
              ),
              h(
                ActionButton,
                {
                  className: "btn btn-primary",
                  onClick: async () => {
                    const roles = rows.map((row) => {
                      const id = parseSnowflake(String(row.role_id).trim());
                      if (!id) throw new Error("Enter each role as a Discord role ID or <@&id> mention");
                      return { role_id: id, level: row.level };
                    });
                    const saved = await api(`/api/guilds/${guildId}/access-roles`, "PUT", {
                      roles,
                      revision: revision.current,
                    }, undefined, () => apiRead(`/api/guilds/${guildId}/access-roles`));
                    generation.current++;
                    revision.current = saved.revision;
                    setBaselineRows(withKeys(saved.roles || roles));
                    const nextDrafts = { ...currentDrafts() };
                    delete nextDrafts[draftKey];
                    ui.set({ drafts: nextDrafts, edits: Object.keys(nextDrafts).length > 0 ? "dirty" : "clean" });
                    toast("Web UI access roles saved", "success");
                  },
                },
                "Save access roles",
              ),
            ),
          ),
    ),
  );
}

function scopeSelect(
  ref,
  guildLabel = "This server only",
  globalLabel = "Global (all servers)",
  allowGlobal = true,
  draftKey,
) {
  return h(
    Select,
    { ref, className: "select", draftKey, defaultValue: "guild" },
    h("option", { value: "guild" }, guildLabel),
    allowGlobal && isBotOwner() && h("option", { value: "global" }, globalLabel),
  );
}
function BanOptions({ action }) {
  const legacyDays = action.delete_days || 0;
  const selected = action.purge_last_hour ? "purge" : legacyDays ? "legacy" : "ban";
  return field(
    "Ban behavior",
    h(
      Select,
      {
        className: "select",
        defaultValue: selected,
        onChange: (event) => {
          const mode = event.target.value;
          action.purge_last_hour = mode === "purge";
          action.delete_days = mode === "legacy" ? legacyDays : 0;
        },
      },
      h("option", { value: "ban" }, "Ban"),
      h("option", { value: "purge" }, "Ban and purge (previous hour)"),
      legacyDays > 0 && h("option", { value: "legacy" }, `Keep existing: delete ${legacyDays} day(s)`),
    ),
    h("p", { className: "field-help" }, "Ban keeps earlier messages. Ban and purge asks Discord to delete this member’s messages across the server from the previous hour. Applies only to ban actions; dry-run never bans or purges."),
  );
}

function RuleAction({ action, index, remove }) {
  return h(
    "div",
    { className: "card card-border escalation-step" },
    h(
      "div",
      { className: "row title-row" },
      h("strong", null, `Action ${index + 1}`),
      h(
        IconButton,
        { action: "remove", label: `Remove action ${index + 1}`, onClick: remove },
      ),
    ),
    h(
      "div",
      { className: "escalation-fields" },
      field(
        "Strike count",
        h(Input, {
          className: "input",
          type: "number",
          min: 1,
          defaultValue: action.at,
          onChange: (event) => {
            action.at = Math.max(1, parseInt(event.target.value) || 1);
          },
        }),
      ),
      field(
        "Action",
        h(
          Select,
          {
            className: "select",
            defaultValue: action.action,
            onChange: (event) => {
              action.action = event.target.value;
            },
          },
          ...["log", "delete", "warn", "timeout", "kick", "ban"].map((value) =>
            h("option", { key: value, value }, value),
          ),
        ),
      ),
      field(
        "Timeout (minutes)",
        h(Input, {
          className: "input",
          type: "number",
          min: 0,
          defaultValue: action.duration_minutes ?? 0,
          title: "minutes (timeout)",
          onChange: (event) => {
            action.duration_minutes = parseInt(event.target.value) || 0;
          },
        }),
      ),
      h(BanOptions, { action }),
    ),
    field(
      "Reason template",
      h(Input, {
        className: "input",
        type: "text",
        defaultValue: action.reason || "",
        placeholder: "reason template",
        onChange: (event) => {
          action.reason = event.target.value;
        },
      }),
    ),
  );
}

const itemKeys = new WeakMap();
let nextItemKey = 0;
function itemKey(item) {
  if (!itemKeys.has(item)) itemKeys.set(item, ++nextItemKey);
  return itemKeys.get(item);
}
function CommandStep({ step, placeholder, onChange, remove }) {
  const input = useRef(null);
  return h(
    "div",
    { className: "card card-border step-card" },
    h(
      "div",
      { className: "row control-row" },
      h(
        Select,
        {
          className: "select",
          value: step.type,
          "aria-label": "Step action",
          onChange: (event) => {
            const type = event.target.value,
              updated = { type };
            applyPlanStepInput(updated, type, input.current.value);
            if (type === "log") updated.event = "plan_executed";
            Object.keys(step).forEach((key) => delete step[key]);
            Object.assign(step, updated);
            input.current.value =
              type === "delete_command_message"
                ? ""
                : String(
                    step.template ??
                      step.role_id ??
                      step.minutes ??
                      step.delete_days ??
                      step.emoji ??
                      step.secs ??
                      "",
                  );
            onChange();
          },
        },
        ...[
          "reply",
          "dm",
          "log",
          "add_role",
          "remove_role",
          "timeout",
          "kick",
          "ban",
          "delete_command_message",
          "react",
          "wait",
        ].map((value) => h("option", { key: value, value }, value)),
      ),
      h(Input, {
        ref: input,
        "aria-label": "Step value",
        className: "input",
        type: "text",
        defaultValue:
          step.template ?? step.role_id ?? step.minutes ?? step.delete_days ?? step.emoji ?? step.secs ?? "",
        placeholder,
        style: { flex: 1 },
        onChange: (event) => {
          applyPlanStepInput(step, step.type, event.target.value);
          onChange();
        },
      }),
      h(IconButton, { action: "remove", label: "Remove step", onClick: remove }),
    ),
  );
}

function useApiRead(path, { retainData = false } = {}) {
  const [revision, setRevision] = useState(0);
  const [result, setResult] = useState({ kind: "loading" });
  useEffect(() => {
    const controller = new AbortController();
    let publication = "current";
    const timeout = setTimeout(() => controller.abort(), 30000);
    setResult((current) => {
      const retained = retainData && current.path === path && current.data != null;
      // A pending retry does not make the retained snapshot fresh again.
      return { kind: "loading", path, data: retained ? current.data : null, error: retained ? current.error : null };
    });
    apiRead(path, controller.signal)
      .then((data) => {
        if (publication === "current") setResult({ kind: "loaded", path, data });
      })
      .catch((error) => {
        if (publication === "current")
          setResult((current) => ({
            ...current,
            kind: "failed",
            error: controller.signal.aborted
              ? apiError("Request timed out. Refresh to verify saved state before trying again.")
              : error,
          }));
      })
      .finally(() => clearTimeout(timeout));
    return () => {
      publication = "disposed";
      clearTimeout(timeout);
      controller.abort();
    };
  }, [path, revision, retainData]);
  return {
    loading: result.kind === "loading",
    data: retainData ? (result.path === path ? result.data ?? null : null) : result.kind === "loaded" ? result.data : null,
    error: retainData ? (result.path === path ? result.error ?? null : null) : result.kind === "failed" ? result.error : null,
    refresh: () => setRevision((value) => value + 1),
  };
}
function DataTable({ headings, children, className = "table table-zebra" }) {
  return h(
    "div",
    {
      className: "table-scroll rounded-box bg-base-100",
      tabIndex: 0,
      role: "region",
      "aria-label": "Scrollable data table",
    },
    h(
      "table",
      { className },
      h("thead", null, h("tr", null, ...headings.map((text) => h("th", { key: text }, text)))),
      h("tbody", null, ...React.Children.toArray(children)),
    ),
  );
}
function tabEvents() {
  const [type, setType] = useState("");
  const [user, setUser] = useState("");
  const input = useRef(null);
  const result = useApiRead(
    `/api/guilds/${state.guildId}/events?limit=100` +
      (type ? `&event=${encodeURIComponent(type)}` : "") +
      (user ? `&user=${encodeURIComponent(user)}` : ""),
  );
  const refresh = () => {
    setUser(parseSnowflake(input.current.value));
    result.refresh();
  };
  return h(
    "div",
    null,
    h("h2", null, "Audit log"),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      h(
        Select,
        {
          className: "select",
          "aria-label": "Event type",
          value: type,
          onChange: (event) => {
            setType(event.target.value);
            setUser(parseSnowflake(input.current.value));
          },
        },
        h("option", { value: "" }, "all types"),
        ...state.config.event_types.map((value) => h("option", { key: value, value }, value)),
      ),
      h(Input, {
        ref: input,
        type: "text",
        className: "input",
        placeholder: "user id",
        onKeyDown: (event) => {
          if (event.key === "Enter") refresh();
        },
      }),
      h(ActionButton, { className: "btn", onClick: refresh }, "Refresh"),
    ),
    h(
      DataTable,
      { headings: ["Time", "Event", "User ID", "Actor ID", "Details"] },
      result.loading || result.error || !result.data?.length
        ? h(
            "tr",
            null,
            h(
              "td",
              { colSpan: 5, className: result.error ? "err" : result.loading ? "" : "muted" },
              result.loading
                ? "Loading…"
                : result.error
                  ? "Could not load records: " + result.error.message
                  : "No matching records.",
            ),
          )
        : result.data.map((record, i) => {
            let data;
            try {
              data = JSON.stringify(JSON.parse(record.data || "{}"), null, 1);
            } catch (_) {
              data = record.data || "";
            }
            return h(
              "tr",
              { key: record.id ?? i },
              h("td", { className: "muted" }, new Date(record.created_at).toLocaleString()),
              h("td", null, record.event_type),
              h("td", { className: "mono" }, record.user_id || "-"),
              h("td", { className: "mono" }, record.actor_id || "-"),
              h("td", { className: "mono", style: { fontSize: "11px" } }, data),
            );
          }),
    ),
  );
}
const STOCKS_SPAM_EXAMPLE = "📈 **Looking for a community focused on stocks and trading?**\n\nJoin our growing community to learn from experienced investors, share market insights, discuss stock ideas, and exchange trading strategies.\nNo spam or aggressive promotion—just practical market discussions.\nWe welcome active investors and value your thoughts and feedback.\n👇 Interested? Join us here:";
const AI_SPAM_EXAMPLE = "I used to work as Full Stack AI developer at several software companies but I’m currently available and working remotely because my last contract ended. Right now I am resting at home. If you have any AI projects or need support, I’d be happy to help, just reach out.";

// A failed add's reason belongs to its retained draft: it survives remounts and
// the other entry's add, and goes away when the draft is edited or cleared.
function setKnownSpamAddError(key, error) {
  const drafts = ui.getSnapshot().drafts;
  if (!drafts[key]) return;
  const draft = { value: drafts[key].value, group: drafts[key].group };
  ui.set({ drafts: { ...drafts, [key]: error ? { ...draft, error } : draft } });
}

function tabKnownSpam() {
  const { drafts } = useStore(ui);
  const scopeRef = useRef(null);
  const guildId = state.guildId;
  const rows = state.config.known_spam || [];
  const rule = (state.config.rules || []).find((r) => r.rule_id === "known_spam");
  const add = async (kind) => {
    const key = kind === "sample" ? "known-spam:sample" : "known-spam:url";
    const value = drafts[key]?.value || "";
    if (!value.trim()) return toast("Enter a full sample or URL pattern", "error");
    const scope = scopeRef.current.value;
    setKnownSpamAddError(key, null);
    try {
      await reloadAfter(async () => {
        const res = await api(`/api/guilds/${guildId}/known-spam`, "POST", {
          scope, kind, value, note: drafts["known-spam:note"]?.value || null,
        });
        // Acknowledged: keep this scope selected, without a draft, even if the refresh fails.
        ui.set({ draftBaselines: { ...ui.getSnapshot().draftBaselines, "known-spam:scope": scope } });
        return res;
      }, () => toast("Known spam entry added", "success"));
    } catch (error) {
      // The browser cannot tell a rejected save from one whose outcome is unknown
      // or pending, so state only that the add was not confirmed.
      const label = kind === "sample" ? "Sample" : "URL pattern";
      const lookup = error?.saveLookup ? " Retrying the same entry, note and scope only looks up the earlier request; no new save was sent." : "";
      setKnownSpamAddError(key, `${label} not confirmed as added: ${error?.message || error}${lookup}`);
      throw error;
    }
  };
  const addErrorFor = (key) => drafts[key]?.error ? h("p", { className: "error", role: "alert" }, drafts[key].error) : null;
  return h("div", null,
    h("h2", null, "Known spam"),
    h("p", { className: "muted" }, "Ban members who post spam you've already confirmed."),
    h(GoodToKnow, {
      items: [
        "The first match bans the member and deletes their messages from the last hour.",
        "In dry-run, matches are only logged.",
        "The rule below turns the list on or off and sets who is skipped.",
        "This server and global each hold up to 1024 entries, samples and URL patterns combined.",
        isBotOwner()
          ? "Global entries apply to every server."
          : "Global entries apply to every server. Only the bot owner can change them.",
      ],
    }),
    rule ? ruleCard(rule) : null,
    h(DraftCard, { group: "known-spam", className: "card bg-base-100 card-border" },
      h("div", { className: "card-body" },
        field("Applies to", scopeSelect(scopeRef, "This server", "Global", true, "known-spam:scope")),
        field("Optional note (up to 512 bytes)", h(Input, { className: "input w-full", draftKey: "known-spam:note", "data-note": true, maxLength: 512 })),
        field("Full sample message (up to 4096 bytes)", h(Textarea, { className: "textarea w-full", draftKey: "known-spam:sample", rows: 7, maxLength: 4096, placeholder: "Paste the entire confirmed spam message. No regex needed." }),
          h("p", { className: "field-help" }, "Matches the same whole message, even with case, accent, spacing or leetspeak changes. A reworded copy may not match.")),
        h(ActionButton, { className: "btn btn-primary", savedFields: ["known-spam:sample", "known-spam:note", "known-spam:scope"], onClick: () => add("sample") }, "Add sample"),
        addErrorFor("known-spam:sample"),
        field("URL wildcard (up to 512 bytes)", h(Input, { className: "input w-full", draftKey: "known-spam:url", "data-url": true, maxLength: 512, placeholder: "tinyurl.com/*" })),
        h("p", { className: "field-help" }, "Use * as a wildcard, for example tinyurl.com/* or *.example.com/*."),
        h(GoodToKnow, {
          summary: "How URL patterns match",
          items: [
            "http:// or https:// is optional.",
            "tinyurl.com/* matches only that site. *.example.com/* matches its subdomains, not example.com itself.",
            "No path means the whole site. Paths are case-sensitive.",
            "* matches anything, including /.",
            "Links without http:// also match.",
            "Anything after ? or # in a link is ignored.",
            "Patterns can't contain regex, ports, logins, ? or #.",
          ],
        }),
        h(ActionButton, { className: "btn btn-primary", savedFields: ["known-spam:url", "known-spam:note", "known-spam:scope"], onClick: () => add("url") }, "Add URL pattern"),
        addErrorFor("known-spam:url"),
      ),
    ),
    h("details", { className: "card bg-base-100 card-border" },
      h("summary", null, "Paste-as-sample examples (not active until saved)"),
      h("p", null, "Copy the full owner sample you want to save into the sample field. These examples are not preloaded entries."),
      h("pre", { style: { whiteSpace: "pre-wrap" } }, STOCKS_SPAM_EXAMPLE),
      h("pre", { style: { whiteSpace: "pre-wrap" } }, AI_SPAM_EXAMPLE),
    ),
    h(DataTable, { headings: ["Type", "Sample or URL pattern", "Note", "Scope", "Actions"] },
      rows.length ? rows.map((row) => h("tr", { key: row.id },
        h("td", null, row.kind === "url" ? "URL" : "Sample"),
        h("td", { style: { whiteSpace: "pre-wrap", overflowWrap: "anywhere" } }, row.value),
        h("td", null, row.note || "—"), h("td", null, row.guild_id === "0" ? "Global" : "This server"),
        h("td", null, row.guild_id !== "0" || isBotOwner() ? h(IconButton, { action: "remove", label: "Remove",
          onClick: async () => {
            if (!confirm("Remove this known spam entry?")) return;
            await api(`/api/guilds/${guildId}/known-spam/${row.id}`, "DELETE");
            if (state.guildId === guildId && await loadConfig()) renderTab();
          },
        }) : "Owner only"),
      )) : h("tr", null, h("td", { colSpan: 5, className: "muted" }, "No known spam entries.")),
    ),
  );
}

function tabKnownBadImages() {
  const [query, setQuery] = useState("");
  const { drafts } = useStore(ui);
  const hashDraft = drafts["known-bad:hash"]?.value ?? "";
  const guildId = state.guildId;
  const rows = (state.config.known_bad_images || []).filter((row) =>
    `${row.hash} ${row.note || ""} ${row.added_via} ${row.actor}`.toLowerCase().includes(query.toLowerCase()),
  );
  if (guildId === "0") return h("p", { className: "muted" }, "Known-bad images are managed per server. Select a server.");
  return h("div", null,
    h("h2", null, "Known-bad images"),
    h("p", { className: "muted" }, "Ban images you've already confirmed as spam."),
    h(GoodToKnow, {
      items: [
        "A listed image matches the first time it's posted.",
        "Uses the Exact image spam rule: in dry-run it only logs, otherwise it deletes the message and bans the member.",
        "Up to 1024 images per server.",
        "Only the image's fingerprint (hash) and your note are kept, never the image.",
      ],
    }),
    field("Search hashes or notes", h(Input, {
      className: "input w-full", "data-search": true, value: query, onChange: (event) => setQuery(event.target.value),
    })),
    h(
      DraftCard,
      { group: "known-bad-images", className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        field("SHA-256 digest", h(Input, {
          className: "input w-full", draftKey: "known-bad:hash", maxLength: 64,
        }), h("p", { className: "field-help" }, "Copy it from an exact-image notice in your log channel.")),
        field("Optional safe note", h(Input, {
          className: "input w-full", draftKey: "known-bad:note", maxLength: 256,
        })),
        h(ActionButton, {
          className: "btn btn-sm",
          disabled: !/^[a-fA-F0-9]{64}$/.test(hashDraft),
          savedFields: ["known-bad:hash", "known-bad:note"],
          onClick: async () => {
            const hash = drafts["known-bad:hash"]?.value ?? "";
            const note = drafts["known-bad:note"]?.value ?? "";
            await reloadAfter(
              () => api(`/api/guilds/${guildId}/known-bad-images`, "POST", { hash, note: note || null }),
              (result) => toast(result.changed ? "Hash added." : "Hash already listed.", "success"),
            );
          },
        }, "Add hash"),
      ),
    ),
    h(DataTable, { headings: ["SHA-256", "Added", "Source", "Actor", "Note", "Actions"] },
      rows.length ? rows.map((row) => h("tr", { key: row.hash },
        h("td", { style: { overflowWrap: "anywhere" } }, row.hash),
        h("td", null, row.added_at), h("td", null, row.added_via),
        h("td", null, row.actor), h("td", null, row.note || "—"),
        h("td", null, h(IconButton, { action: "remove", label: "Remove",
          onClick: async () => {
            if (!confirm(`Remove SHA-256 ${row.hash}?`)) return;
            await api(`/api/guilds/${guildId}/known-bad-images/${row.hash}`, "DELETE");
            if (state.guildId === guildId && await loadConfig()) renderTab();
          },
        })),
      )) : h("tr", null, h("td", { colSpan: 6, className: "muted" }, "No matching hashes.")),
    ),
  );
}

function tabBans() {
  const guildId = state.guildId;
  const result = useApiRead(`/api/guilds/${guildId}/bans?limit=200`);
  return h(
    "div",
    null,
    h("h2", null, "Bans"),
    h(
      "p",
      { className: "muted", style: { marginBottom: "12px" } },
      "Bans the bot made or saw in Discord.",
    ),
    h(GoodToKnow, {
      items: ["You can unban here or in Discord.", "Unbans you request here are queued. Bot actions shows the result."],
    }),
    h(
      DataTable,
      { headings: ["Banned user", "Status", "Moderator", "Reason", "Time", "Actions"] },
      result.loading || result.error || !result.data?.length
        ? h(
            "tr",
            null,
            h(
              "td",
              { colSpan: 6, className: result.error ? "err" : result.loading ? "" : "muted" },
              result.loading
                ? "Loading…"
                : result.error
                  ? "Could not load records: " + result.error.message
                  : "No matching records.",
            ),
          )
        : result.data.map((record, i) =>
            h(
              "tr",
              { key: record.id ?? i },
              h(
                "td",
                null,
                record.user_display_name
                  ? `${record.user_display_name} (${record.user_name ? `${record.user_name}, ` : ""}${record.user_id})`
                  : record.user_name
                    ? `${record.user_name} (${record.user_id})`
                    : record.user_id,
              ),
              h("td", null, record.active ? "banned" : "unbanned"),
              h(
                "td",
                null,
                record.moderator_id
                  ? record.moderator_name
                    ? `${record.moderator_name} (${record.moderator_id})`
                    : record.moderator_id
                  : "Unavailable",
              ),
              h("td", null, record.reason || "Reason unavailable"),
              h("td", { className: "muted" }, new Date(record.created_at).toLocaleString()),
              h(
                "td",
                null,
                record.active &&
                  h(
                    IconButton,
                    {
                      action: "unban",
                      label: "Unban",
                      onClick: async () => {
                        if (confirm(`Unban ${record.user_id}?`))
                          await submitBotAction(`/api/guilds/${guildId}/unban`, "POST", {
                            user_id: String(record.user_id),
                            reason: "unbanned via WebUI",
                          });
                      },
                    },
                  ),
              ),
            ),
          ),
    ),
  );
}
function tabActions() {
  const guildId = state.guildId;
  const result = useApiRead(`/api/guilds/${guildId}/actions`);
  return h(
    "div",
    null,
    h("h2", null, "Bot actions"),
    h(
      "p",
      { className: "muted" },
      "The latest 50 requests sent to the bot.",
    ),
    h(GoodToKnow, {
      items: [
        "Queued requests need the bot to be online.",
        "A request with an unknown result is not retried. Check Discord to see what happened.",
        "Cancel stops requests that haven't started.",
        "Force-cancel clears a stuck log-channel request so you can save again. It doesn't undo anything in Discord.",
      ],
    }),
    h(ActionButton, { className: "btn standalone-action", onClick: result.refresh }, "Refresh"),
    h(
      DataTable,
      { headings: ["Requested", "Action", "Requested by", "Status", "Result", "Controls"] },
      result.loading || result.error || !result.data?.length
        ? h(
            "tr",
            null,
            h(
              "td",
              { colSpan: 6 },
              result.loading
                ? "Loading…"
                : result.error
                  ? "Could not load actions: " + result.error.message
                  : "No bot actions yet.",
            ),
          )
        : result.data.map((job) =>
            h(
              "tr",
              { key: job.id },
              h("td", null, new Date(job.created_at * 1000).toLocaleString()),
              h(
                "td",
                null,
                job.action.kind === "unban"
                  ? `Unban ${job.action.user_id}`
                  : job.action.kind === "member_sync"
                    ? `Member sync: ${job.action.operation}`
                    : `${job.action.event}: ${job.action.channel_id || "clear log channel"}`,
              ),
              h("td", null, job.actor_id),
              h("td", null, job.state === "uncertain" ? "Outcome unknown" : job.state),
              h("td", null, job.outcome || "—"),
              h(
                "td",
                null,
                actionCanCancel(job) &&
                  h(
                    IconButton,
                    {
                      action: "cancel",
                      label: actionCancelLabel(job),
                      onClick: async () => {
                        if (
                          actionCancelNeedsConfirm(job) &&
                          !confirm(
                            "Clear this stuck log-channel request? This does not change Discord. A new save can then run.",
                          )
                        )
                          return;
                        await api(`/api/guilds/${guildId}/actions/${encodeURIComponent(job.id)}`, "DELETE");
                        result.refresh();
                      },
                    },
                  ),
              ),
            ),
          ),
    ),
  );
}

// Render only fixed React elements and text children: React escapes HTML before
// it reaches the DOM. Never parse user text as HTML or use innerHTML here.
function previewInline(text, depth = 0) {
  if (depth >= 20) return [text];
  const out = [];
  // A code opener runs to the end of its backtick run, and its closer is the
  // next run with at least that many ticks. Index every run once, right to
  // left, so unmatched ticks do not rescan the rest of the text.
  let codeSpans;
  const codeSpan = start => {
    if (!codeSpans) {
      codeSpans = new Map();
      const next = [];
      for (let end = text.length; end > 0;) {
        if (text[end - 1] !== "`") { end--; continue; }
        let run = end - 1;
        while (run > 0 && text[run - 1] === "`") run--;
        for (let i = run; i < end; i++) codeSpans.set(i, [end - i, next[end - i] ?? -1]);
        for (let ticks = 1; ticks <= end - run; ticks++) next[ticks] = run;
        end = run;
      }
    }
    return codeSpans.get(start);
  };
  // Like Discord, italics skip a nested bold or underline pair, and an emphasis
  // closer must end its delimiter run, so `*a **b** c*` and `**a *b***` nest.
  const closing = (marker, start, skipCode = false) => {
    const emphasis = skipCode && ["*", "_", "**", "__"].includes(marker);
    for (let i = start; i < text.length; i++) {
      if (text[i] === "\\") { i++; continue; }
      if (skipCode && text[i] === "`") {
        const [ticks, end] = codeSpan(i);
        if (end >= 0) { i = end + ticks - 1; continue; }
      }
      if (i > start && emphasis && marker.length === 1 && text.startsWith(marker + marker, i)) { i++; continue; }
      if (text.startsWith(marker, i) && (i === start || !emphasis || text[i + marker.length] !== marker[0])) return i;
    }
    return -1;
  };
  // Bracket queries advance monotonically. Reuse the next label closer,
  // including a failed search, until the caller passes it.
  let labelEnd;
  let linkEnds;
  const linkEnd = start => {
    if (!linkEnds) {
      linkEnds = new Map();
      const open = [];
      // Match destination parentheses once, with the same escape and balance
      // rules as the forward scan. An unmatched outer '(' can contain a
      // matched inner link, so a failed outer search cannot skip the suffix.
      for (let i = start; i < text.length; i++) {
        if (text[i] === "\\") { i++; continue; }
        if (text[i] === "(") open.push(i);
        if (text[i] === ")" && open.length) linkEnds.set(open.pop(), i);
      }
    }
    return linkEnds.get(start) ?? -1;
  };
  for (let i = 0; i < text.length;) {
    const key = i;
    if (text[i] === "\\" && /[\\`*_{}[\]()#+\-.!>~|]/.test(text[i + 1] || "")) {
      out.push(text[i + 1]); i += 2; continue;
    }
    if (text[i] === "`") {
      const [ticks, end] = codeSpan(i);
      if (end >= 0) {
        out.push(h("code", { key }, text.slice(i + ticks, end)));
        i = end + ticks; continue;
      }
    }
    if (text[i] === "[") {
      if (labelEnd === undefined || (labelEnd >= 0 && labelEnd < i + 1)) labelEnd = closing("](", i + 1);
      if (labelEnd >= 0) {
        const end = linkEnd(labelEnd + 1);
        if (end >= 0) {
          const url = text.slice(labelEnd + 2, end).replace(/\\([()])/g, "$1");
          if (/^https?:\/\/[^\s<>\\]+$/i.test(url)) {
            out.push(h("a", { key, href: url, target: "_blank", rel: "noreferrer noopener" },
              previewInline(text.slice(i + 1, labelEnd), depth + 1)));
            i = end + 1; continue;
          }
          // An unsafe target stays literal, including any Markdown within it.
          out.push(text.slice(i, end + 1)); i = end + 1; continue;
        }
      }
    }
    let matched = false;
    for (const [marker, tag] of [["***", "strong"], ["**", "strong"], ["__", "u"], ["~~", "s"], ["||", "details"], ["*", "em"], ["_", "em"]]) {
      if (!text.startsWith(marker, i)) continue;
      const end = closing(marker, i + marker.length, true);
      if (end <= i + marker.length) continue;
      let children = previewInline(text.slice(i + marker.length, end), depth + 1);
      if (marker === "***") children = h("em", {}, children);
      out.push(marker === "||"
        ? h(tag, { key, className: "preview-spoiler" }, h("summary", {}, "Spoiler"), children)
        : h(tag, { key }, children));
      i = end + marker.length; matched = true; break;
    }
    if (!matched) out.push(text[i++]);
  }
  return out;
}

function previewMarkdown(text, depth = 0) {
  if (depth >= 20) return [text];
  const lines = text.replace(/\r\n?/g, "\n").split("\n");
  const out = [];
  const listItem = line => line.match(/^( *)([-*]|\d+\.) (.*)$/);
  for (let i = 0; i < lines.length;) {
    const key = i, line = lines[i];
    const fence = line.match(/^(`{3,})([^`]*)$/);
    if (fence) {
      const code = [];
      i++;
      while (i < lines.length && !new RegExp("^`{" + fence[1].length + ",}\\s*$").test(lines[i])) code.push(lines[i++]);
      if (i < lines.length) i++;
      out.push(h("pre", { key }, h("code", {}, code.join("\n")))); continue;
    }
    if (line.startsWith(">>> ")) {
      const quoted = [line.slice(4), ...lines.slice(i + 1)].join("\n");
      out.push(h("blockquote", { key }, previewMarkdown(quoted, depth + 1))); break;
    }
    if (line.startsWith("> ")) {
      const quoted = [];
      while (i < lines.length && lines[i].startsWith("> ")) quoted.push(lines[i++].slice(2));
      out.push(h("blockquote", { key }, previewMarkdown(quoted.join("\n"), depth + 1))); continue;
    }
    const item = listItem(line);
    if (item) {
      const ordered = item[2].endsWith("."), indent = item[1].length, items = [];
      while (i < lines.length) {
        const next = listItem(lines[i]);
        if (!next || next[1].length !== indent || next[2].endsWith(".") !== ordered) break;
        const body = [next[3]], itemKey = i++;
        while (i < lines.length && lines[i].startsWith(" ".repeat(indent + 2))) body.push(lines[i++].slice(indent + 2));
        items.push(h("li", { key: itemKey }, previewMarkdown(body.join("\n"), depth + 1)));
      }
      out.push(h(ordered ? "ol" : "ul", { key, ...(ordered ? { start: Number.parseInt(item[2], 10) } : {}) }, items)); continue;
    }
    const heading = line.match(/^(#{1,3}|-#) (.*)$/);
    out.push(h(heading ? heading[1] === "-#" ? "small" : "h" + heading[1].length : "div", { key },
      line === "" ? h("br") : previewInline(heading ? heading[2] : line)));
    i++;
  }
  return out;
}

capturePreviewToken();
root.render(h(App));
init();
