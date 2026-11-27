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
  RESOURCE_RANGES,
  ResourceChart,
  ResourceRangeSelector,
  ShareBar,
  formatCount,
  formatDelta,
  formatExact,
  sumSeries,
} from "./charts.js";
import {
  clearListLocation, ListControls, ListEmptyRow, ListLoadProgress, ListSortHeading, readListLocation,
  flushPendingListSearch, listLoadProgress, openFilteredList, readListPage, rememberListLocation, restoreListLocation,
  SEARCH_DELAY_MS, setCaseListLocation, useListControls,
} from "./list-controls.js";
import { ACTION_ICONS, DASHBOARD_ICONS, Icon, NAV_ICONS } from "./icons.js";
import { noAutofill } from "./no-autofill.js";
import React, { useState, useEffect, useLayoutEffect, useRef, useSyncExternalStore, createContext, useContext } from "react";
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
const subscribeWithoutDraft = () => () => {};
function draftBaseline(snapshot, key, fallback, multiline) {
  const baseline = String(snapshot.draftBaselines[key] ?? fallback ?? "");
  // The DOM exposes textarea values with LF line endings.
  return multiline ? baseline.replace(/\r\n?/g, "\n") : baseline;
}
function useDraftControl({ draftKey, defaultValue, onChange, ...props }, multiline = false) {
  const group = useContext(DraftGroup);
  const working = useContext(BusyContext);
  const value = useSyncExternalStore(draftKey ? ui.subscribe : subscribeWithoutDraft, () => {
    if (!draftKey) return undefined;
    const snapshot = ui.getSnapshot();
    return snapshot.drafts[draftKey]?.value ?? draftBaseline(snapshot, draftKey, defaultValue, multiline);
  });
  if (!draftKey) return { ...props, defaultValue, onChange, disabled: working || props.disabled };
  return {
    ...props,
    "data-no-draft": true,
    value,
    disabled: working || props.disabled,
    onChange: event => {
      const value = event.target.value;
      const snapshot = ui.getSnapshot();
      const next = { ...snapshot.drafts };
      if (value === draftBaseline(snapshot, draftKey, defaultValue, multiline)) delete next[draftKey];
      else next[draftKey] = { value, group };
      ui.set({ drafts: next, edits: Object.keys(next).length > 0 ? "dirty" : "clean" });
      onChange?.(event);
    },
  };
}
function Input(props) { return h(HeadlessInput, noAutofill(useDraftControl(props))); }
function Textarea(props) { return h(HeadlessTextarea, noAutofill(useDraftControl(props, true))); }
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
    canManageServer() && h(
      ActionButton,
      {
        className: "btn btn-sm",
        onClick: async () => {
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
    canManageServer() && h(
      Select,
      { ref: profile, className: "select", "aria-label": "Preview profile" },
      h("option", { value: "administrator" }, "Administrator"),
      h("option", { value: "moderator" }, "Moderator (read-only)"),
      h("option", { value: "settings" }, "Settings"),
      h("option", { value: "member" }, "Member"),
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
function ServerPicker({ guilds, selectedId, onSelect }) {
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
        selected ? selected.name : "Select a server",
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
  );
}

async function switchGuild(guildId) {
  if (!canLeave()) return;
  ui.set({ pendingDrafts: null, drafts: {}, draftBaselines: {}, copySource: null, editor: null });
  pendingCase = null;
  state.guildId = guildId;
  state.tab = workspaceTab(state.tab);
  syncListLocation();
  resetGuildConfig();
  ui.set({ edits: "clean" });
  persistWorkspace();
  await loadWorkspaceConfig();
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
      setSessionItemWithReadCacheFallback(window.sessionStorage, key, requestId);
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
  const result = useCachedApiRead("/api/owner-servers", "servers");
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
      "Every server the bot is in or has left, who added it and when.",
    ),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      h(ActionButton, { className: "btn", onClick: () => result.refresh() }, "Refresh"),
      h(LastUpdated, { result }),
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

const USAGE_HELP = [
  "Each database has its own 1,000 MB limit.",
  "Cleanup speeds up at 700 MB, snapshot capture pauses at 800 MB and growth stops at 900 MB.",
  "Allocated size includes free pages the database can reuse.",
  "Unknown sizes never count as free space.",
  "Estimated categories are labeled as estimates.",
  "Details lists every diagnostic value.",
];

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
    { className: "contents chart-page database-usage-page" },
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

// Platform analytics (#618): historical activity is range-dependent, while
// current directory coverage stays with the mounted owner page.
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

const MEMBER_DIRECTORY_COVERAGE = {
  not_started: { label: "not imported", tone: "warn", title: "Open Members to check initial import availability." },
  importing: { label: "importing", tone: "warn", title: "The initial member import has yet to finish." },
  stalled: { label: "stalled", tone: "warn", title: "The initial member import has stopped progressing." },
  reconciling: { label: "reconciling", tone: "warn", title: "The bot is scheduled to recover member coverage." },
  uncertain: { label: "unverified", tone: "warn", title: "The member list may be out of date. Open Members for recovery options." },
  unavailable: { label: "unavailable", tone: "warn", title: "This server's member directory could not be read." },
};

function memberDirectoryBadge(directory, guildId, importingLabel = "importing") {
  const coverage = directory?.coverage;
  if (!Object.hasOwn(MEMBER_DIRECTORY_COVERAGE, coverage)) return null;
  const badge = MEMBER_DIRECTORY_COVERAGE[coverage];
  const details = [badge.title];
  if (directory.reconciliation_reason || directory.gap_at) {
    details.push(memberGapCause(directory.reconciliation_reason, memberGapWhen(directory.gap_at)) + ".");
  }
  if (directory.status_reason) details.push(directory.status_reason);
  if (directory.next_attempt_at) details.push(`Next attempt: ${memberTime(directory.next_attempt_at)}`);
  else if (coverage === "stalled") details.push("Next attempt: not scheduled");
  const canOpenImport = coverage === "not_started" && !isPreview() &&
    state.guilds.some((guild) => guild.id === guildId && guildAccessRank[guild.access] >= guildAccessRank.dashboard);
  return {
    ...badge,
    label: coverage === "importing" ? importingLabel : badge.label,
    title: details.join(" "),
    importLink: canOpenImport
      ? `?list=members&guild=${encodeURIComponent(guildId)}`
      : null,
  };
}

function memberImportLink(badge) {
  return badge?.importLink && h("a", { className: "link", href: badge.importLink, title: badge.title }, "Import");
}

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
    ? ` ${members.guilds_incomplete} counted ${members.guilds_incomplete === 1 ? "server has" : "servers have"} incomplete member-directory coverage.`
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
function platformRows(snapshot, members, membersLoading, snapshotGuildIds) {
  const byGuild = members?.by_guild ?? {};
  return (snapshot?.guilds ?? []).map((guild) => {
    const outsideSnapshot = snapshotGuildIds && !snapshotGuildIds.has(guild.guild_id);
    const directory = guild.active ? byGuild[guild.guild_id] : null;
    let coverage = null;
    if (guild.active && !membersLoading) {
      if (outsideSnapshot) coverage = "outside_snapshot";
      else if (!directory) coverage = "unavailable";
      else if (directory.coverage !== "complete") coverage = directory.coverage;
    }
    return {
      ...guild,
      actions: platformActions(guild),
      previousActions: platformActions(guild.previous),
      members: directory ? directory.current : null,
      directory,
      coverage,
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
      h("input", noAutofill({
        type: "search",
        className: "input input-sm",
        placeholder: "Filter by name or ID",
        "aria-label": "Filter servers",
        value: query,
        onChange: (event) => setQuery(event.target.value),
      })),
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
                const coverage = row.coverage === "outside_snapshot"
                  ? { label: "not in snapshot", tone: "warn", title: "This server was outside the saved member totals. Refresh to include it." }
                  : memberDirectoryBadge(row.directory || { coverage: row.coverage }, row.guild_id);
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
                    memberImportLink(coverage),
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
  const [legacy, setLegacy] = useState(false);
  const historyPath = `/api/platform/analytics?days=${days}`;
  const result = useApiRead(legacy ? historyPath : `${historyPath}&include_members=false`, { retainData: true });
  const directory = useApiRead(legacy ? null : "/api/platform/analytics/members", { retainData: true });
  const splitMissing = !legacy && directory.error?.status === 404;
  useEffect(() => {
    if (splitMissing) setLegacy(true);
  }, [splitMissing]);
  const snapshot = result.data;
  const checkingLegacy = splitMissing || (!legacy && directory.loading && result.error?.status === 400);
  const historyError = checkingLegacy ? null : result.error;
  const loading = !snapshot && !historyError;
  const failed = !snapshot && Boolean(historyError);
  const rangeLabel = `${days} days`;
  const series = snapshot?.series;
  const points = React.useMemo(() => toTrendPoints(series), [series]);
  const previous = snapshot?.previous ?? null;
  const members = legacy ? snapshot?.members : directory.error?.status === 403 ? null : directory.data;
  const membersLoading = !members && (legacy ? loading : splitMissing || (directory.loading && !directory.error));
  const snapshotGuildIds = Array.isArray(members?.snapshot_guild_ids) ? new Set(members.snapshot_guild_ids) : null;
  const rows = platformRows(snapshot, members, membersLoading, snapshotGuildIds);
  const activeRows = rows.filter(row => row.active);
  const fleetChanged = snapshot && snapshotGuildIds &&
    (activeRows.length !== snapshotGuildIds.size || activeRows.some(row => row.coverage === "outside_snapshot"));
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
  const membersUnavailable = !members || members.status === "unavailable";
  const fleetWarning = fleetChanged ? "Member totals and activity cover different sets of servers. Refresh to update both." : "";
  const memberBadge = fleetChanged && !membersUnavailable
    ? { label: "different server set", tone: "warn", title: fleetWarning }
    : members?.status === "partial" ? { label: "partial", tone: "warn", title: platformMemberStatus(members) } : null;
  const netPrevious = previous?.joins == null || previous?.leaves == null ? null : previous.joins - previous.leaves;
  const body = (render) =>
    snapshot ? render() : h("p", { className: "an-empty muted", "aria-busy": loading ? "true" : "false" }, failed ? "Unavailable" : "Loading…");
  const rank = (title, id, props) => analyticsPanel(title, id, body(() => h(RankList, props)), "an-span-4");
  const memberStatus = !membersLoading && members?.status !== "complete" ? platformMemberStatus(members) : "";
  const memberWarning = directory.error && !splitMissing
    ? members
      ? `Member totals refresh failed. ${directory.error.message}. Showing the last successful response (stale). ${memberStatus}`.trim()
      : `Member totals unavailable. ${directory.error.message}`
    : "";
  const memberNote = [memberWarning || memberStatus, fleetWarning].filter(Boolean).join(" ");
  return h(
    "div",
    { className: "contents chart-page analytics-page platform-page" },
    pageHeader(
      "Platform analytics",
      `All servers · last ${rangeLabel} (UTC)`,
      h(RangeSelector, { value: days, onChange: setDays }),
      h(ActionButton, { id: "refresh-platform-analytics", className: "btn btn-sm", onClick: () => {
        result.refresh();
        if (!legacy) directory.refresh();
      } }, "Refresh"),
      h(InfoTip, { id: "platform-analytics-help", label: "About these figures" }, PLATFORM_HELP),
    ),
    historyError && h("p", { id: "platform-analytics-status", className: "alert alert-soft alert-warning", role: "status" },
      snapshot
        ? `Platform analytics refresh failed: ${historyError.message}. Showing the last successful response (stale).`
        : `Platform analytics unavailable: ${historyError.message}`),
    memberNote && h("p", { id: "platform-members-status", className: memberWarning ? "alert alert-soft alert-warning" : "an-note muted", role: "status" }, memberNote),
    h(
      "div",
      { id: "platform-analytics", className: "contents" },
      h(
        "section",
        { className: "an-kpis", "aria-label": `Fleet totals for the last ${rangeLabel}` },
        h(KpiTile, { label: "Servers", value: snapshot?.servers ?? 0, loading, unavailable: failed, rangeLabel }),
        h(KpiTile, { label: "Members", value: members?.current ?? 0, loading: membersLoading, unavailable: membersUnavailable, badge: memberBadge, rangeLabel }),
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

// ---------------------------------------------------------------------------
// Bot Resources (#652): system status for the bot and API processes. One
// owner-only read per range or Refresh. Values the processes could not read
// say "Unavailable"; values older than about two sample intervals are marked
// stale. Nothing is drawn as 0 unless it was measured as 0.
// ---------------------------------------------------------------------------

const RESOURCES_HELP = [
  "CPU and memory readings and limits come from the cgroups sampled by the bot and API.",
  "The API is sampled every 5 seconds; the bot saves its status and both processes' history once a minute.",
  "1h and 24h use minute rollups kept 48 hours; 7d and 30d use hourly rollups kept 31 days.",
  "Activity counts cover this application. The database client does not report rows read.",
];

const RESOURCE_PROCESSES = [
  ["bot", "Bot"],
  ["api", "API"],
];

const UNAVAILABLE = "Unavailable";

function formatBytes(bytes) {
  if (bytes == null || !Number.isFinite(bytes)) return null;
  const units = [[1e9, "GB"], [1e6, "MB"], [1e3, "KB"]];
  for (const [size, unit] of units) if (Math.abs(bytes) >= size) return `${(bytes / size).toFixed(1)} ${unit}`;
  return `${Math.round(bytes)} B`;
}

function formatRate(value) {
  if (value == null || !Number.isFinite(value)) return null;
  if (value === 0) return "0";
  return value >= 100 ? formatExact(Math.round(value)) : value >= 10 ? value.toFixed(1) : value.toFixed(2);
}

function formatDuration(ms) {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return null;
  const minutes = Math.floor(ms / 60000);
  if (minutes < 1) return `${Math.floor(ms / 1000)} s`;
  if (minutes < 60) return `${minutes} min`;
  const hours = Math.floor(minutes / 60);
  if (hours < 48) return `${hours} h ${minutes % 60} min`;
  return `${Math.floor(hours / 24)} d ${hours % 24} h`;
}

function formatInstant(ms) {
  return ms ? new Date(ms).toLocaleString(undefined, { timeZoneName: "short" }) : null;
}

// CPU as a percentage of the container quota when one exists, otherwise of
// one CPU. Tenths of a percent of one CPU come from the API.
function cpuScale(limit) {
  const millicores = limit?.Millicores;
  return millicores > 0 ? { divisor: millicores / 1000, of: `of ${formatRate(millicores / 1000)} CPU limit`, max: 100 } : { divisor: 1, of: limit === "Unlimited" ? "of one CPU (no CPU limit)" : "of one CPU (limit unavailable)", max: null };
}

function cpuPercent(tenths, divisor) {
  return tenths == null ? null : tenths / 10 / divisor;
}

function formatResourcePercent(value) {
  return value == null ? "—" : `${formatRate(value)}%`;
}

function memoryLimitBytes(memory) {
  return memory?.limit?.Bytes ?? null;
}

// The snapshot is fresh only while the response itself is recent.
function resourceFreshness(current, generatedAtMs, nowMs = Date.now()) {
  if (!current || current.availability !== "available") return "unavailable";
  const drift = generatedAtMs ? Math.max(0, nowMs - generatedAtMs) : 0;
  return current.stale || (current.age_ms ?? Infinity) + drift > (current.stale_after_ms ?? 0) ? "stale" : "fresh";
}

function resourceValue(text) {
  return text == null ? h("span", { className: "br-unavailable" }, UNAVAILABLE) : text;
}

function resourceMetric(label, value, caption, chart) {
  return h(
    "div",
    { className: "du-metric br-metric" },
    h("span", { className: "du-metric-label" }, label),
    h("strong", { className: "br-value" }, resourceValue(value)),
    caption ? h("span", { className: "du-metric-caption" }, caption) : null,
    chart || null,
  );
}

function resourceFacts(rows) {
  const known = rows.filter(([, value]) => value != null && value !== "");
  const unknown = rows.filter(([, value]) => value == null || value === "").map(([label]) => label);
  return [
    h("dl", { key: "facts", className: "du-facts" }, ...known.map(([label, value]) => h("div", { key: label }, h("dt", null, label), h("dd", null, value)))),
    unknown.length ? h("p", { key: "unknown", className: "du-unknown" }, `Unavailable: ${unknown.join(", ")}.`) : null,
  ];
}

const BYTES_PER_MIB = 2 ** 20;
const ESTIMATE_HOURS_30_DAYS = 30 * 24;
const memoryMibFormat = new Intl.NumberFormat("en-US", { maximumFractionDigits: 2 });
const hourlyCostFormat = new Intl.NumberFormat("en-US", { maximumFractionDigits: 8 });

function formatMemoryMiB(bytes) {
  if (!Number.isFinite(bytes)) return null;
  const mib = bytes / BYTES_PER_MIB;
  if (mib > 0 && mib < 0.01) return "<0.01 MiB";
  return `${memoryMibFormat.format(mib)} MiB`;
}

function memoryBlocks(bytes, blockMib) {
  const blockBytes = blockMib * BYTES_PER_MIB;
  const count = Math.max(1, Math.ceil(bytes / blockBytes));
  const remaining = count * blockBytes - bytes;
  const headroom = Math.floor(remaining / BYTES_PER_MIB * 100) / 100;
  return {
    count,
    capacity: count * blockMib,
    headroom: remaining > 0 && headroom === 0 ? "<0.01 MiB" : `${memoryMibFormat.format(headroom)} MiB`,
  };
}

function hostingCost(hourly, hours) {
  const cost = hourly * hours;
  if (hourly == null || !Number.isFinite(cost)) return null;
  const minimum = hours === 1 ? 0.00000001 : 0.01;
  const formatted = hours === 1 ? hourlyCostFormat.format(cost) : cost.toFixed(2);
  return cost > 0 && cost < minimum ? `<$${minimum.toFixed(hours === 1 ? 8 : 2)}` : `$${formatted}`;
}

function MemoryBillingPanel({ snapshot, nowMs }) {
  const billing = snapshot.memory_billing;
  const blockMib = billing?.block_mib;
  const rates = billing?.rates;
  const memoryRelation = snapshot.cgroup_relation?.memory;
  const cpuRelation = snapshot.cgroup_relation?.cpu;
  const memoryDeclared = memoryRelation === "operator_declared_disjoint";
  const cpuDeclared = cpuRelation === "operator_declared_disjoint";
  const combineMemory = memoryRelation === "disjoint" || memoryDeclared;
  const combineCpu = cpuRelation === "disjoint" || cpuDeclared;
  const declaredMetrics = [memoryDeclared && "memory", rates && cpuDeclared && "CPU"].filter(Boolean);
  const readings = RESOURCE_PROCESSES.map(([id]) => {
    const current = snapshot.processes?.[id]?.current;
    return resourceFreshness(current, snapshot.generated_at_ms, nowMs) === "fresh" ? current : null;
  });
  const byteCounts = readings.map((reading) => {
    const bytes = reading?.memory?.program_bytes;
    return Number.isSafeInteger(bytes) && bytes >= 0 ? bytes : null;
  });
  const [botBytes, apiBytes] = byteCounts;
  const total = combineMemory && botBytes != null && apiBytes != null && Number.isSafeInteger(botBytes + apiBytes)
    ? botBytes + apiBytes : null;
  const blocks = total != null && blockMib > 0 ? memoryBlocks(total, blockMib) : null;
  const memoryHourly = blocks && rates ? blocks.count * rates.memory_usd_per_block_hour : null;
  const cpuTenths = readings.map((reading) => reading?.cpu_tenths);
  const cpuHourly = combineCpu && rates && cpuTenths.every((value) => value != null)
    ? cpuTenths.reduce((sum, value) => sum + value, 0) / 1000 * rates.cpu_usd_per_core_hour
    : null;
  const blockText = blocks && `${formatExact(blocks.count)} block${blocks.count === 1 ? "" : "s"} (${formatExact(blocks.capacity)} MiB) · ${blocks.headroom} headroom to the next block in this estimate`;
  const peakBytes = snapshot.combined_memory_peak?.peak_bytes;
  const pairedPeriods = snapshot.combined_memory_peak?.periods;
  const totalPeriods = snapshot.combined_memory_peak?.total_periods;
  const peakPeriod = snapshot.span_secs === 3600 ? "hour" : "minute";
  const coverage = Number.isSafeInteger(pairedPeriods) && pairedPeriods >= 0
    && Number.isSafeInteger(totalPeriods) && totalPeriods > 0 && pairedPeriods <= totalPeriods
    ? `${formatExact(pairedPeriods)} of ${formatExact(totalPeriods)} ${peakPeriod}${totalPeriods === 1 ? "" : "s"}`
    : null;
  const peakBlocks = pairedPeriods > 0 && Number.isSafeInteger(peakBytes) && peakBytes >= 0 && blockMib > 0
    ? memoryBlocks(peakBytes, blockMib) : null;
  const peakText = pairedPeriods === 0 ? "No paired memory readings"
    : peakBlocks && `${formatMemoryMiB(peakBytes)} · ${formatExact(peakBlocks.count)} block${peakBlocks.count === 1 ? "" : "s"} (${formatExact(peakBlocks.capacity)} MiB)`;
  const useApiMemory = apiBytes != null && (botBytes == null || readings[1].age_ms <= readings[0].age_ms);
  const memoryRows = combineMemory
    ? [
      ["Combined cgroup memory", formatMemoryMiB(total)],
      ["Bot cgroup memory", formatMemoryMiB(botBytes)],
      ["API cgroup memory", formatMemoryMiB(apiBytes)],
      ["Estimated blocks", blockText],
      ["Paired-period peak estimate", peakText],
      ["Paired memory coverage", coverage],
    ]
    : [[
      memoryRelation === "shared" ? "Shared cgroup memory" : useApiMemory ? "API cgroup memory" : botBytes != null ? "Bot cgroup memory" : "Cgroup memory",
      formatMemoryMiB(useApiMemory ? apiBytes : botBytes),
    ]];
  const overlapReason = (relation) => {
    if (relation === "shared") return "the samples share a cgroup";
    if (relation === "overlapping") return "the sampled cgroups overlap";
    if (relation === "unverified") return "the sampled cgroups have no common visible ancestor";
    return "the sampled cgroups could not be verified as separate";
  };
  let blockNote = "Estimated block size unavailable.";
  if (blocks) {
    blockNote = `The estimate rounds fresh cgroup readings up in ${blockMib} MiB blocks, with a one-block minimum.`;
  } else if (!combineMemory) {
    blockNote = `Combined memory and block estimates are unavailable because ${overlapReason(memoryRelation)}.`;
  } else if (total == null) {
    blockNote = "Combined block estimate unavailable. Both cgroups need fresh, usable memory readings.";
  }
  const estimateRow = (label, hourly) => h("tr", null,
    h("th", { scope: "row" }, label),
    h("td", null, resourceValue(hostingCost(hourly, 1))),
    h("td", null, resourceValue(hostingCost(hourly, ESTIMATE_HOURS_30_DAYS))),
  );
  return analyticsPanel("Local memory block estimate", "resources-memory-billing", h("div", null,
    h("dl", { className: "du-facts" },
      ...memoryRows.map(([label, value]) => h("div", { key: label }, h("dt", null, label), h("dd", null, resourceValue(value)))),
    ),
    h("p", { className: "an-note muted" }, blockNote),
    combineMemory && peakBlocks && h("p", { className: "an-note muted" },
      `This peak estimate covers only ${peakPeriod}s with readings for both cgroups. The sum of their separate maxima is an upper bound on sampled combined memory in each ${peakPeriod} because the cgroups can peak at different times.`,
    ),
    combineMemory && coverage && pairedPeriods > 0 && pairedPeriods < totalPeriods && h("p", { className: "an-note muted" },
      "Periods without both readings can hide higher peaks.",
    ),
    combineMemory && (blocks || peakBlocks) && h("p", { className: "an-note muted" },
      "These estimates exclude Bunny’s billed system overhead. Bunny’s pricing docs leave open whether hourly billing uses average or peak memory and whether blocks round per container or per pod.",
    ),
    rates
      ? h("div", null,
        h("h4", { id: "resources-hosting-estimate", className: "du-metric-label" }, "Hosting cost estimate (USD)"),
        (combineMemory || combineCpu) && h("table", { className: "table table-sm", "aria-labelledby": "resources-hosting-estimate" },
          h("thead", null, h("tr", null, h("th", { scope: "col" }, "Resource"), h("th", { scope: "col" }, "Hourly"), h("th", { scope: "col" }, "30 days"))),
          h("tbody", null, combineMemory && estimateRow("Memory blocks", memoryHourly), combineCpu && estimateRow("CPU time", cpuHourly)),
        ),
        !combineCpu && h("p", { className: "an-note muted" }, `Combined CPU cost is unavailable because ${overlapReason(cpuRelation)}.`),
        (combineMemory || combineCpu) && h("p", { className: "an-note muted" }, "Assumes current usage continues. Excludes host system overhead and egress."),
      )
      : h("p", { className: "an-note muted" }, billing ? "Hosting cost estimate is off." : "Hosting cost estimate configuration unavailable."),
    declaredMetrics.length > 0 && h("p", { className: "an-note muted" },
      `Using the operator’s cgroup separation declaration for ${declaredMetrics.join(" and ")}.`,
    ),
    (memoryRelation === "unverified" || (rates && cpuRelation === "unverified")) && h("p", { className: "an-note muted" },
      "After independently verifying that the sampled scopes are disjoint, enable ",
      h("code", null, "web.hosting_estimate.assume_disjoint_cgroups"),
      " on the API.",
    ),
  ));
}

function diskRows(disks) {
  const labels = { container_root: "Container filesystem", tmp: "Temporary files", logs: "Log files" };
  if (!disks?.length) return [["Container filesystem", null], ["Temporary files", null]];
  return disks.map((disk) => {
    const label = labels[disk.kind] || disk.kind;
    if (disk.kind === "container_root") return [label, "Not measured: its totals are the host's disk"];
    if (disk.bytes == null) return [label, null];
    const limit = disk.limit_bytes != null ? ` of ${formatBytes(disk.limit_bytes)}` : "";
    return [label, `${formatBytes(disk.bytes)}${limit}${disk.path ? ` in ${disk.path}` : ""}${disk.complete ? "" : " (at least; too many files to count)"}`];
  });
}

function ResourceCard({ id, title, process, generatedAtMs, nowMs, range, chartFrom, chartTo }) {
  const current = process?.current;
  const history = process?.history;
  const points = history?.points;
  const freshness = resourceFreshness(current, generatedAtMs, nowMs);
  const info = current?.info;
  const badge =
    freshness === "unavailable"
      ? { label: "Unavailable", tone: "neutral" }
      : freshness === "stale"
      ? { label: "Stale", tone: "warning" }
      : info?.ready
      ? { label: "Ready", tone: "success" }
      : { label: "Not ready", tone: "warning" };
  const scale = cpuScale(info?.cpu_limit);
  const { divisor, max } = scale;
  const memoryLimit = memoryLimitBytes(current?.memory);
  const memory = current?.memory?.program_bytes ?? null;
  const cpu = cpuPercent(current?.cpu_tenths, divisor);
  const sampleAgeMs = current?.age_ms != null && generatedAtMs != null
    ? current.age_ms + Math.max(0, nowMs - generatedAtMs)
    : null;
  const sampled = sampleAgeMs != null ? `Sampled ${formatDuration(sampleAgeMs)} ago` : "Not sampled";
  const headingId = `resources-${id}`;
  const cores = info?.schedulable_cpus != null ? `${info.schedulable_cpus} schedulable CPU${info.schedulable_cpus === 1 ? "" : "s"}` : "schedulable CPUs unavailable";
  const source = current?.memory?.source === "CgroupUsage" ? "cgroup usage (includes page cache)" : current?.memory ? "cgroup stat (excludes reclaimable cache)" : null;
  const cpuChart = React.useMemo(() => h(ResourceChart, {
    points: points ?? [], label: `${title} CPU`, from: chartFrom, to: chartTo, max, format: formatResourcePercent,
    series: [
      { key: "avg", label: "Average", tone: "br-tone-avg", value: (p) => cpuPercent(p.cpu_avg_tenths, divisor) },
      { key: "max", label: "Peak", tone: "br-tone-max", value: (p) => cpuPercent(p.cpu_max_tenths, divisor) },
    ],
  }), [points, title, chartFrom, chartTo, divisor, max]);
  const memoryChart = React.useMemo(() => h(ResourceChart, {
    points: points ?? [], label: `${title} memory`, from: chartFrom, to: chartTo, max: memoryLimit, format: (value) => formatMemoryMiB(value) ?? "—",
    series: [
      { key: "avg", label: "Average", tone: "br-tone-avg", value: (p) => p.memory_avg_bytes },
      { key: "max", label: "Peak", tone: "br-tone-max", value: (p) => p.memory_max_bytes },
    ],
  }), [points, title, chartFrom, chartTo, memoryLimit]);
  return h(
    "section",
    { className: "card bg-base-100 card-border du-card br-card", "aria-labelledby": headingId, "data-freshness": freshness },
    h(
      "div",
      { className: "card-body" },
      h(
        "header",
        { className: "du-head" },
        h("h3", { id: headingId, className: "du-title" }, title),
        h("span", { className: `badge badge-soft badge-sm badge-${badge.tone} du-health` }, badge.label),
        h("span", { className: "du-chip", title: formatInstant(current?.sampled_at_ms) ?? "Not sampled" }, sampled),
      ),
      freshness === "stale" && h("p", { className: "du-stale" }, `Stale: older than ${formatDuration(current?.stale_after_ms) ?? "two sample intervals"}. These are not current values.`),
      freshness === "unavailable" && h("p", { className: "du-stale" }, id === "bot" ? "The bot has not saved a status yet, or it could not be read." : "This process has not sampled itself yet."),
      h(
        "div",
        { className: "du-metrics br-metrics" },
        resourceMetric(
          "CPU",
          cpu == null ? null : `${formatRate(cpu)}% ${scale.of}`,
          [h("span", { key: "c" }, cores), history?.cpu_peak_tenths != null ? h("span", { key: "p" }, `peak ${formatResourcePercent(cpuPercent(history.cpu_peak_tenths, divisor))} in ${range}`) : null],
          cpuChart,
        ),
        resourceMetric(
          "Memory",
          memory == null ? null : `${formatMemoryMiB(memory)}${memoryLimit ? ` of ${formatMemoryMiB(memoryLimit)} (${formatRate((memory * 100) / memoryLimit)}%)` : current?.memory?.limit === "Unlimited" ? " (no memory limit)" : ""}`,
          [source ? h("span", { key: "s" }, `Source: ${source}`) : null, h("span", { key: "p" }, `peak ${formatMemoryMiB(history?.memory_peak_bytes) ?? "unavailable"} in ${range}`)],
          memoryChart,
        ),
      ),
      h(
        "div",
        { className: "br-section" },
        h("h4", { className: "du-metric-label" }, "Disk"),
        ...resourceFacts(diskRows(current?.disk)),
        h("p", { className: "du-metric-caption" }, "Database size: ", h("button", { type: "button", className: "btn btn-xs btn-link br-link", onClick: () => navigate("usage") }, "Database Usage")),
      ),
      h(
        "div",
        { className: "br-section" },
        h("h4", { className: "du-metric-label" }, "Process"),
        ...resourceFacts([
          ["Version", info?.version],
          ["Commit", info?.commit ? info.commit.slice(0, 12) : null],
          ["Uptime", formatDuration(current?.uptime_ms)],
          ["Last restart", formatInstant(info?.started_at_ms)],
          [`Restarts in ${range}`, history?.restarts == null ? null : formatExact(history.restarts)],
          ["Pod", info?.pod],
          ["Region", info?.region],
          ["Ready", info ? (info.ready ? "Ready" : "Not ready") : null],
        ]),
      ),
    ),
  );
}

// A counter from the range: measured zero when the process reported minutes
// without it, unavailable when there is no history at all.
function rangeRate(history, name) {
  if (!history || !history.minutes) return null;
  return history.per_minute?.[name] ?? 0;
}

function rangeTotal(history, name) {
  if (!history || !history.minutes) return null;
  return history.totals?.[name] ?? 0;
}

// `meaning` spells out a short label for the tooltip and accessible name.
function resourceTile(label, value, meaning) {
  const name = meaning ?? label;
  return h(
    "div",
    { key: label, className: "an-kpi br-kpi", role: "group", "aria-label": `${name}: ${value ?? "unavailable"}` },
    h("div", { className: "an-kpi-head" }, h("span", { className: "an-kpi-label", title: name }, label)),
    h("div", { className: "an-kpi-body" }, h("span", { className: "an-kpi-value", title: value ?? UNAVAILABLE }, value ?? "—")),
  );
}

function breakdownRows(history, family) {
  return (history?.breakdowns?.[family] ?? []).map(([label, count]) => ({ key: label, label, count }));
}

function rateChart(points, title, name, from, to) {
  return h(ResourceChart, {
    points, label: title, from, to, format: (value) => formatRate(value) ?? "—",
    series: [{ key: name, label: title, tone: "br-tone-avg", value: (p) => (p.minutes ? p.rates?.[name] ?? 0 : null) }],
  });
}

function gaugeChart(points, title, name, from, to, format) {
  return h(ResourceChart, {
    points, label: title, from, to, format,
    series: [{ key: name, label: title, tone: "br-tone-avg", value: (p) => (p.minutes ? p.gauges?.[name] ?? null : null) }],
  });
}

const BUNNY_COST_SERVICES = [
  ["magic_containers", "Magic Containers", "accent"],
  ["database", "Database", "ok"],
  ["storage", "Storage", "warn"],
  ["cdn", "CDN"],
];

function bunnyUsd(amount) {
  if (!Number.isFinite(amount) || amount < 0) return "Not available";
  if (amount > 0 && amount < 0.01) return "<$0.01";
  return `$${amount.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
}

function BunnyCostHistory({ history }) {
  const services = BUNNY_COST_SERVICES.filter(([, , tone]) => tone);
  const max = Math.max(0, ...history.map((month) =>
    services.reduce((total, [key]) => total + (month[key]?.usd ?? 0), 0)));
  return h("div", null,
    h("h4", { id: "resources-bunny-history", className: "du-metric-label" }, "Past months (UTC)"),
    history.length === 0
      ? h("p", { className: "an-note muted" }, "No saved months yet.")
      : h("div", { className: "table-scroll rounded-box", tabIndex: 0, role: "region", "aria-labelledby": "resources-bunny-history" },
        h("table", { className: "table table-sm" },
          h("thead", null, h("tr", null,
            ...["Month", "Estimated costs", ...services.map(([, label]) => label)].map((label) =>
              h("th", { key: label, scope: "col" }, label)),
          )),
          h("tbody", null, ...history.map((month) => {
            const parts = services.map(([key, label, tone]) => {
              const cost = month[key];
              return {
                key, label, tone, value: cost?.usd,
                partial: cost?.partial || (key === "magic_containers" && Number.isFinite(cost?.usd)),
                display: cost?.egress_rate_unknown ? "Unknown egress rate" : bunnyUsd(cost?.usd),
              };
            });
            return h("tr", { key: month.month },
              h("th", { scope: "row" }, month.month),
              h("td", null, h(ShareBar, {
                parts, total: max || 1,
                summary: parts.map((part) => `${part.label} ${part.display}${part.partial ? " (partial)" : ""}`).join(", "),
              })),
              ...parts.map((part) => h("td", { key: part.key },
                part.display, part.partial ? " (partial)" : "",
              )),
            );
          })),
        ),
      ),
  );
}

function BunnyCostPanel({ provider }) {
  const breakdown = provider.magic_breakdown;
  const cpu = provider.magic_cpu_hourly_averages;
  const cpuSampleTime = formatInstant(Date.parse(cpu?.sampled_at ?? ""));
  let cpuAverage = "Not available";
  if (cpu) {
    cpuAverage = cpu.min === cpu.max ? String(cpu.min) : `${cpu.min}–${cpu.max}`;
  }
  return analyticsPanel("Bunny cost estimates", "resources-bunny-costs", h("div", null,
    h("p", { className: "an-note muted" },
      `${provider.month} (UTC). Bunny’s monthly figure is the authoritative Magic Containers projection. Partial estimates cover CPU, RAM, egress, and allocated volumes when available.`,
    ),
    h("div", { className: "table-scroll rounded-box", tabIndex: 0, role: "region", "aria-label": "Current Bunny costs" },
      h("table", { className: "table table-sm" },
        h("thead", null, h("tr", null,
          ...["Service", "Projected month (USD)", "Month-to-date estimate (USD)", "Last updated"].map((label) =>
            h("th", { key: label, scope: "col" }, label)),
        )),
        h("tbody", null, ...BUNNY_COST_SERVICES.map(([key, label]) => {
          const service = provider[key];
          const magic = key === "magic_containers";
          const projection = bunnyUsd(service?.projected_usd);
          const partial = service?.partial || (magic && Number.isFinite(service?.month_to_date_usd));
          const status = key === "cdn" ? "Not available with scoped Bunny keys"
            : service?.status !== "available" && service?.last_updated_secs
            ? "Not available · last successful figures shown" : null;
          return h("tr", { key },
            h("th", { scope: "row" }, label,
              key === "database" && service?.configured_resources === 1 ? " (1 database)" : "",
            ),
            h("td", null, magic ? h("strong", null, projection) : projection),
            h("td", null, bunnyUsd(service?.month_to_date_usd), partial ? " (partial)" : ""),
            h("td", null, service?.last_updated_secs ? formatInstant(service.last_updated_secs * 1000) : "—",
              status && h("div", { className: "muted" }, status),
            ),
          );
        })),
      ),
    ),
    (breakdown || cpu) && h("dl", { className: "du-facts", "aria-label": "Magic Containers month-to-date estimate components" },
      h("div", null, h("dt", null, "CPU cost"), h("dd", null, bunnyUsd(breakdown?.cpu))),
      h("div", null, h("dt", null, "Estimated hourly average CPU cores per pod"),
        h("dd", null, cpuAverage,
          cpu && h("span", { className: "muted" }, " · ",
            cpuSampleTime ? "Sampled " : "Sample time unavailable",
            cpuSampleTime && h("time", { dateTime: cpu.sampled_at }, cpuSampleTime),
          ),
        ),
      ),
      ...(breakdown ? [["RAM", "ram"], ["Egress", "egress"], ["Volumes", "volume"]] : []).map(([label, key]) =>
        h("div", { key }, h("dt", null, label), h("dd", null, key === "egress" && breakdown.egress_rate_unknown ? "Unknown regional rate" : bunnyUsd(breakdown[key]))),
      ),
    ),
    h("p", { className: "an-note muted" }, "Database uses Bunny’s free public-preview rate. Storage assumes one Standard region. Magic Containers egress uses Bunny’s regional rates. An unknown egress rate leaves the usage total unavailable. Volume estimates use the reported allocation because volume chart units are unverified. Estimates exclude Bunny’s $1 monthly account minimum."),
    h(BunnyCostHistory, { history: provider.history ?? [] }),
  ));
}

function TabBotResources() {
  return isBotOwner() ? h(BotResources) : ownerAccessRequired();
}

function BotResources() {
  const [range, setRange] = useState("24h");
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = window.setInterval(() => setNowMs(Date.now()), 5000);
    return () => window.clearInterval(id);
  }, []);
  const result = useApiRead(`/api/owner/resources?range=${range}`, { retainData: true });
  const snapshot = result.data;
  const bot = snapshot?.processes?.bot;
  const api = snapshot?.processes?.api;
  const botHistory = bot?.history;
  const apiHistory = api?.history;
  const from = snapshot?.from_secs;
  const to = snapshot?.to_secs;
  const botPoints = botHistory?.points;
  const apiPoints = apiHistory?.points;
  // Freshness ticks leave the history props stable. The charts still own their layout state.
  const botCharts = React.useMemo(() => h("div", { key: "charts", className: "br-pair" },
    h("div", null, h("h4", { className: "du-metric-label" }, "Gateway events per minute"), rateChart(botPoints ?? [], "Gateway events per minute", "gateway_events", from, to)),
    h("div", null, h("h4", { className: "du-metric-label" }, "Shard latency (peak)"), gaugeChart(botPoints ?? [], "Shard latency", "shard_latency_ms", from, to, (value) => `${formatExact(value)} ms`)),
  ), [botPoints, from, to]);
  const apiCharts = React.useMemo(() => h("div", { key: "charts", className: "br-pair" },
    h("div", null, h("h4", { className: "du-metric-label" }, "Requests per minute"), rateChart(apiPoints ?? [], "Requests per minute", "api_requests", from, to)),
    h("div", null, h("h4", { className: "du-metric-label" }, "p95 latency"), h(ResourceChart, {
      points: apiPoints ?? [], label: "p95 latency", from, to, format: (value) => `${formatExact(value)} ms`,
      series: [{ key: "p95", label: "p95 latency", tone: "br-tone-max", value: (p) => p.p95_ms ?? null }],
    })),
  ), [apiPoints, from, to]);
  const gauge = (process, name) => (resourceFreshness(process?.current, snapshot?.generated_at_ms, nowMs) === "unavailable" ? null : process?.current?.gauges?.[name] ?? null);
  const rangeLabel = RESOURCE_RANGES.find(([value]) => value === range)?.[1].toLowerCase() ?? range;
  const note = result.error
    ? h("p", { id: "resources-status", className: "alert alert-soft alert-warning", role: "status" }, snapshot
        ? `Refresh failed: ${result.error.message}. Showing the last successful response (stale).`
        : `Bot Resources unavailable: ${result.error.message}`)
    : snapshot && snapshot.history_status !== "available"
    ? h("p", { id: "resources-status", className: "alert alert-soft alert-warning", role: "status" }, snapshot.history_status === "stale"
        ? "History could not be refreshed; trends are from the last successful read (stale)."
        : "History is unavailable: the database could not be read. Current API values are live.")
    : null;
  // Latency percentiles are the upper bound of the histogram bucket they fall in.
  const latency = (ms) => (ms == null ? null : `${formatExact(ms)} ms`);
  const bytesPerMinute = (value) => (value == null ? null : `${formatBytes(value)}/min`);
  const panelBody = (render) => (snapshot ? render() : h("p", { className: "an-empty muted", "aria-busy": result.loading ? "true" : "false" }, result.error ? UNAVAILABLE : "Loading…"));
  const costRow = (label, name, format = formatRate) =>
    h("tr", { key: name }, h("th", { scope: "row" }, label), ...[botHistory, apiHistory].map((history, i) => h("td", { key: i }, resourceValue(format(rangeRate(history, name))))));
  return h(
    "div",
    { className: "contents chart-page analytics-page resources-page" },
    pageHeader(
      "Bot Resources",
      snapshot ? `Bot and API · ${rangeLabel} (UTC) · response ${formatDuration(Math.max(0, Date.now() - snapshot.generated_at_ms)) ?? "just now"} old` : "Bot and API",
      h(ResourceRangeSelector, { value: range, onChange: setRange }),
      h(ActionButton, { id: "refresh-resources", className: "btn btn-sm", onClick: () => result.refresh() }, "Refresh"),
      h(InfoTip, { id: "resources-help", label: "About these figures" }, RESOURCES_HELP),
    ),
    note,
    !snapshot && !result.error && h("p", { className: "muted", role: "status", "aria-busy": "true" }, "Loading Bot Resources…"),
    snapshot && h(MemoryBillingPanel, { snapshot, nowMs }),
    snapshot?.provider?.status === "configured" && h(BunnyCostPanel, { provider: snapshot.provider }),
    snapshot &&
      h(
        "div",
        { id: "resources-cards", className: "du-grid" },
        ...RESOURCE_PROCESSES.map(([id, title]) =>
          h(ResourceCard, { key: id, id, title, process: snapshot.processes?.[id], generatedAtMs: snapshot.generated_at_ms, nowMs, range: rangeLabel, chartFrom: from, chartTo: to }),
        ),
      ),
    h(
      "div",
      { className: "an-grid" },
      analyticsPanel(
        "Bot activity",
        "resources-bot-activity",
        panelBody(() => [
          h(
            "section",
            { key: "tiles", className: "an-kpis", "aria-label": "Bot activity" },
            resourceTile("Gateway/min", formatRate(rangeRate(botHistory, "gateway_events")), "Gateway events per minute"),
            resourceTile("REST requests/min", formatRate(rangeRate(botHistory, "discord_rest")), "Logical Discord REST requests per minute, including log deliveries. Retries count once."),
            resourceTile("Discord 429s", rangeTotal(botHistory, "discord_429") == null ? null : formatExact(rangeTotal(botHistory, "discord_429"))),
            resourceTile("Rate-limit waits", rangeTotal(botHistory, "discord_rate_limit_events") == null ? null : formatExact(rangeTotal(botHistory, "discord_rate_limit_events")), "Discord rate-limit waits (429s and pre-emptive bucket waits)"),
            resourceTile("Shard latency", gauge(bot, "shard_latency_ms") == null ? null : `${formatExact(gauge(bot, "shard_latency_ms"))} ms`),
            resourceTile("Shards up", gauge(bot, "shards") == null ? null : `${formatExact(gauge(bot, "shards_connected"))} of ${formatExact(gauge(bot, "shards"))}`),
            resourceTile("Lease gen.", gauge(bot, "lease_generation") == null ? null : formatExact(gauge(bot, "lease_generation")), "Writer-lease generation"),
            resourceTile("Lease margin", gauge(bot, "lease_margin_ms") == null ? null : formatDuration(gauge(bot, "lease_margin_ms"))),
            resourceTile("Guilds", gauge(bot, "guilds") == null ? null : formatExact(gauge(bot, "guilds"))),
            resourceTile("Capture queue", gauge(bot, "capture_queue_items") == null ? null : `${formatExact(gauge(bot, "capture_queue_items"))} · ${formatBytes(gauge(bot, "capture_queue_bytes")) ?? "?"}`),
          ),
          botCharts,
          h("div", { key: "ranks", className: "br-pair" },
            h("div", null, h("h4", { className: "du-metric-label" }, "Gateway events by type"), h(RankList, { rows: breakdownRows(botHistory, "gateway"), label: "Gateway events by type", empty: botHistory?.minutes ? "No events in this range." : UNAVAILABLE })),
            h("div", null, h("h4", { className: "du-metric-label" }, "Rate-limit waits by bucket"), h(RankList, { rows: breakdownRows(botHistory, "discord_rate_limits"), label: "Rate-limit waits by bucket", color: "an-bans", empty: botHistory?.minutes ? "No rate-limit waits in this range." : UNAVAILABLE })),
          ),
        ]),
        "an-span-12",
      ),
      analyticsPanel(
        "API activity",
        "resources-api-activity",
        panelBody(() => [
          h(
            "section",
            { key: "tiles", className: "an-kpis", "aria-label": "API activity" },
            resourceTile("Requests/min", formatRate(rangeRate(apiHistory, "api_requests"))),
            resourceTile("p50 latency", latency(apiHistory?.p50_ms), "p50 latency (upper bound of its bucket)"),
            resourceTile("p95 latency", latency(apiHistory?.p95_ms), "p95 latency (upper bound of its bucket)"),
            resourceTile("4xx", rangeTotal(apiHistory, "api_4xx") == null ? null : formatExact(rangeTotal(apiHistory, "api_4xx"))),
            resourceTile("5xx", rangeTotal(apiHistory, "api_5xx") == null ? null : formatExact(rangeTotal(apiHistory, "api_5xx"))),
            resourceTile("Active sessions", gauge(api, "sessions_active") == null ? null : formatExact(gauge(api, "sessions_active"))),
            resourceTile("OAuth 429s", rangeTotal(apiHistory, "oauth_429") == null ? null : formatExact(rangeTotal(apiHistory, "oauth_429"))),
            resourceTile("Discord 429s", rangeTotal(apiHistory, "discord_429") == null ? null : formatExact(rangeTotal(apiHistory, "discord_429"))),
          ),
          apiCharts,
          h("div", { key: "ranks", className: "br-pair" },
            h("div", null, h("h4", { className: "du-metric-label" }, "Discord requests by route"), h(RankList, { rows: breakdownRows(apiHistory, "discord_rest_routes"), label: "Discord requests by route", empty: apiHistory?.minutes ? "No Discord requests in this range." : UNAVAILABLE })),
            h("div", null, h("h4", { className: "du-metric-label" }, "Discord 429s by route"), h(RankList, { rows: breakdownRows(apiHistory, "discord_429_routes"), label: "Discord 429s by route", color: "an-bans", empty: apiHistory?.minutes ? "No Discord 429s in this range." : UNAVAILABLE })),
          ),
        ]),
        "an-span-12",
      ),
      analyticsPanel(
        "Cost drivers",
        "resources-costs",
        panelBody(() => [
          h(
            "div",
            { key: "table", className: "table-scroll rounded-box", tabIndex: 0, role: "region", "aria-label": "Cost drivers per minute" },
            h(
              "table",
              { className: "table table-sm br-costs" },
              h("thead", null, h("tr", null, h("th", { scope: "col" }, `Average per minute, ${rangeLabel}`), h("th", { scope: "col" }, "Bot"), h("th", { scope: "col" }, "API"))),
              h(
                "tbody",
                null,
                costRow("Database round trips", "db_ops"),
                costRow("Database read transactions", "db_reads"),
                costRow("Database write transactions", "db_writes"),
                costRow("Primary-routed core database statements", "db_primary_statements"),
                costRow("Primary-routed guild-store statements", "db_guild_primary_statements"),
                h("tr", { key: "begin-timeouts" }, h("th", { scope: "row" }, `Primary BEGIN timeouts, ${rangeLabel}`), ...[botHistory, apiHistory].map((history, i) => h("td", { key: i }, resourceValue(rangeTotal(history, "db_primary_begin_timeouts") == null ? null : formatExact(rangeTotal(history, "db_primary_begin_timeouts")))))),
                costRow("Logical Discord REST requests", "discord_rest"),
                costRow("Log output", "log_bytes", (value) => bytesPerMinute(value)),
                h("tr", { key: "bytes" }, h("th", { scope: "row" }, "API response bytes"), h("td", null, "—"), h("td", null, resourceValue(bytesPerMinute(rangeRate(apiHistory, "api_bytes_out"))))),
                h("tr", { key: "rows" }, h("th", { scope: "row" }, "Database rows read"), h("td", { colSpan: 2 }, resourceValue(null), " — the database client does not report rows read.")),
              ),
            ),
          ),
          h("div", { key: "primary-paths", className: "br-pair" },
            h("div", null, h("h4", { className: "du-metric-label" }, "Bot primary statements by code path"), h(RankList, { rows: breakdownRows(botHistory, "db_primary_paths"), label: "Bot primary statements by code path", empty: botHistory?.minutes ? "No primary statements in this range." : UNAVAILABLE })),
            h("div", null, h("h4", { className: "du-metric-label" }, "API primary statements by code path"), h(RankList, { rows: breakdownRows(apiHistory, "db_primary_paths"), label: "API primary statements by code path", empty: apiHistory?.minutes ? "No primary statements in this range." : UNAVAILABLE })),
          ),
          h("div", { key: "primary-begin", className: "br-pair" },
            h("div", null, h("h4", { className: "du-metric-label" }, "Bot primary BEGIN latency"), h(RankList, { rows: breakdownRows(botHistory, "db_primary_begin_ms"), label: "Bot primary BEGIN latency", empty: botHistory?.minutes ? "No primary transactions in this range." : UNAVAILABLE })),
            h("div", null, h("h4", { className: "du-metric-label" }, "API primary BEGIN latency"), h(RankList, { rows: breakdownRows(apiHistory, "db_primary_begin_ms"), label: "API primary BEGIN latency", empty: apiHistory?.minutes ? "No primary transactions in this range." : UNAVAILABLE })),
          ),
          h("p", { key: "response-bytes", className: "an-note muted" }, "API response bytes count responses with a known length."),
        ]),
        "an-span-12",
      ),
    ),
  );
}

function PolicyLinks() {
  return h(
    "nav",
    { className: "legal-links", "aria-label": "Policies" },
    h("a", { href: "/privacy" }, "Privacy Policy"),
    h("a", { href: "/terms" }, "Terms of Service"),
  );
}

// Keep the workspace snapshot stable while only field values or baselines change.
const WORKSPACE_FIELDS = ["view", "startupStatus", "previewAvailability", "contentRevision", "editor", "notification", "activity", "edits"];
const WORKSPACE_STATE_FIELDS = ["me", "guilds", "guildId", "tab"];
let workspaceSnapshot;
function selectWorkspace(snapshot) {
  if (!workspaceSnapshot || WORKSPACE_FIELDS.some(key => workspaceSnapshot[key] !== snapshot[key]) ||
    WORKSPACE_STATE_FIELDS.some(key => workspaceSnapshot[key] !== state[key])) {
    workspaceSnapshot = Object.fromEntries([
      ...WORKSPACE_FIELDS.map(key => [key, snapshot[key]]),
      ...WORKSPACE_STATE_FIELDS.map(key => [key, state[key]]),
    ]);
  }
  return workspaceSnapshot;
}

function App() {
  const appearance = useAppearance();
  const display = useStore(ui, selectWorkspace);

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
  const navigation = workspaceNavigation
    .map(([group, items]) => [group, items.filter(([tab]) => tabAllowed(tab))])
    .filter(([, items]) => items.length);
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
            display.guilds.length > 0 && h(
              ServerPicker,
              { guilds: display.guilds, selectedId: display.guildId, onSelect: switchGuild },
            ),
            display.me?.role_discovery_incomplete && h("p", { className: "muted", role: "status" },
              "Some server access checks could not finish. ",
              h(ActionButton, { className: "btn btn-xs", onClick: async () => {
                if (canLeave()) await init();
              } }, "Retry server discovery"),
            ),
            navigation.length > 0 && h(
              "nav",
              { "aria-label": "Workspace navigation" },
              h(
                "ul",
                { id: "tabs", className: "menu w-full" },
                ...navigation.flatMap(([group, items]) => [
                  h("li", { key: `group-${group}`, className: "nav-label menu-title" }, group),
                  ...items.map(([tab, label]) =>
                    h(
                      "li",
                      { key: tab },
                      h(
                        ActionButton,
                        {
                          "data-tab": tab,
                          className: display.tab === tab ? "active menu-active btn-active" : "",
                          "aria-current": display.tab === tab ? "page" : undefined,
                          onClick: () => navigate(tab),
                        },
                        h(Icon, { name: NAV_ICONS[tab].icon, intent: NAV_ICONS[tab].intent, className: "nav-icon" }),
                        label,
                      ),
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
                  `${display.me?.user.username} (${display.me?.user.global_name || ""})`.trim(),
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
                workspaceStatus(display),
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
              h(PolicyLinks),
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
                      {
                        id: "login-link", className: "btn btn-primary", href: authUrl("/login"),
                        onClick: () => rememberListLocation(WORKSPACE_LISTS),
                      },
                      "Continue with Discord",
                      h(Icon, { name: "external-link" }),
                    ),
                    h("small", null, "For server managers and community builders."),
                    h("footer", null, h(PolicyLinks)),
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
          "aria-label": display.editor?.props?.dialogLabel || "Configuration editor",
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
// Searches, filters, and controls with their own save or draft handling opt out.
function trackDraft(event) {
  if (
    event.target.matches("input, textarea, select") &&
    !event.target.hasAttribute("data-search") &&
    !event.target.closest("[data-no-draft]") &&
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
  const ownerSections = { servers: TabOwnerServers, usage: TabDatabaseUsage, "platform-analytics": TabPlatformAnalytics, resources: TabBotResources };
  const tab = workspaceTab(state.tab);
  if (Object.hasOwn(ownerSections, tab)) return h(ownerSections[tab]);
  if (!tab)
    return emptyPanel(
      state.me?.role_discovery_incomplete ? "Server discovery incomplete" : "You don't have access to any servers using jModGuard",
      state.me?.role_discovery_incomplete
        ? "Some server access checks could not finish. Retry to check your servers again."
        : state.me?.role_scope_missing
          ? "Sign out and sign in again so jModGuard can check your server roles."
          : "",
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
            onClick: loadWorkspaceConfig,
          },
          "Retry",
        ),
      { role: "status" },
    );
  return h(components[tab]);
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
  if (record.rule_id === ONBOARDING_TRAP_RULE) {
    ui.set({ editor: h(OnboardingTrapEditor, { record }) });
    return;
  }
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
    readSessionEnding = false;
    const previousGuild = state.guildId;
    const previousAccess = guildAccess();
    state.guilds = state.me.guilds;
    restoreWorkspace();
    const guildContextChanged = state.guildId !== previousGuild || guildAccess() !== previousAccess;
    // An owner tab skips the refresh, so defer a fresh read to the next guild tab.
    if (guildContextChanged || !tabNeedsGuildConfig()) resetGuildConfig();
    if (guildContextChanged) ui.set({ editor: null });
    showView("main-view");
    await loadWorkspaceConfig();
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
  stopPrivateWatchers();
  state.guildId = null;
  ui.set({ edits: "clean" });
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
function resetGuildConfig() {
  ++configRequest;
  state.config = null;
  state.configRead = { activity: "idle", error: null };
}

function ownsConfigRead(request, guildId) {
  return request === configRequest && guildId === state.guildId;
}

const pendingLogChannelActions = new Map();
const logChannelPolls = new Map();
const authLossCleanups = new Set();

const actionReconcileDelays = [1000, 1500, 2500, 5000, 10000, 20000, 40000, 80000, 120000, 150000];

// Each sidebar entry declares the minimum access it needs.
const workspaceNavigation = [
  ["Overview", [
    ["dashboard", "Dashboard", "dashboard"], ["analytics", "Analytics", "dashboard"],
  ]],
  ["Moderation", [
    ["cases", "Cases", "dashboard"], ["bans", "Bans", "dashboard"], ["members", "Members", "dashboard"],
    ["actions", "Bot actions", "dashboard"], ["events", "Audit Log", "dashboard"],
  ]],
  ["Automod", [
    ["rules", "Automod Rules", "settings"], ["badwords", "Badwords", "settings"],
    ["known-spam", "Known spam", "settings"], ["known-bad-images", "Known-bad images", "settings"],
  ]],
  ["Configuration", [
    ["settings", "Settings", "settings"], ["logchannels", "Log Channels", "settings"],
    ["commands", "Commands", "settings"], ["templates", "Templates", "settings"],
  ]],
  ["Owner", [
    ["servers", "Bot servers", "owner"], ["resources", "Bot Resources", "owner"],
    ["usage", "Database Usage", "owner"], ["platform-analytics", "Platform analytics", "owner"],
  ]],
];
const tabAccess = new Map(workspaceNavigation.flatMap(([, items]) => items.map(([tab, , access]) => [tab, access])));
const guildAccessRank = { none: 0, dashboard: 1, settings: 2, manage: 3 };

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
  const { list, guild, caseId } = restoreListLocation(WORKSPACE_LISTS, state.guilds);
  const linkedList = WORKSPACE_LISTS.has(list);
  const linkedGuild = linkedList && state.guilds.find((g) => g.id === guild);
  state.guildId = linkedGuild?.id || state.guilds.find((g) => g.id === saved.guildId)?.id || state.guilds[0]?.id || null;
  state.tab = workspaceTab(linkedList ? linkedGuild ? list : null : saved.tab);
  if (linkedGuild && state.tab === "cases" && caseId) {
    pendingCase = { guildId: guild, caseId };
  }
  syncListLocation();
}

function syncListLocation() {
  const { list, guild } = readListLocation();
  const config = WORKSPACE_LISTS.get(list);
  if (config && (list !== state.tab || guild !== state.guildId)) clearListLocation(config);
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
  state.tab = workspaceTab(tab);
  syncListLocation();
  if (state.tab === "cases" && pendingCase?.guildId === state.guildId) {
    setCaseListLocation(state.guildId, pendingCase.caseId);
  }
  persistWorkspace();
  if (tabNeedsGuildConfig() && !state.config && state.configRead.activity !== "loading") {
    void loadWorkspaceConfig();
  } else {
    renderTab();
  }
  return true;
}

function isBotOwner() {
  return state.me?.is_bot_owner === true && !isPreview();
}

// Web UI access in the selected server: "manage" (owner or Manage Server),
// or a level a server admin mapped to one of the user's Discord roles. The API
// enforces every level; hiding controls here is only presentation.
function guildAccess() {
  const access = selectedGuild()?.access;
  return typeof access === "string" && Object.hasOwn(guildAccessRank, access) ? access : "none";
}

function canEditSettings() {
  return !isPreview() && guildAccessRank[guildAccess()] >= guildAccessRank.settings;
}

function canManageServer() {
  return guildAccess() === "manage";
}

function canBanMembers() {
  return !isPreview() && guildAccess() !== "none" && selectedGuild()?.can_ban_members === true;
}

function workspaceStatus({ activity, edits }) {
  if (isPreview()) return "Preview — changes disabled";
  if (guildAccess() === "none") return "";
  if (!canEditSettings()) return canBanMembers() ? "Dashboard access" : "Read-only access";
  if (activity === "busy") return "Working…";
  if (edits === "dirty") return "Unsaved changes";
  return "Changes saved with each action";
}

function tabAllowed(tab) {
  const access = tabAccess.get(tab);
  return access === "owner" ? isBotOwner() : guildAccessRank[guildAccess()] >= guildAccessRank[access];
}

function workspaceTab(tab) {
  if (tabAllowed(tab)) return tab;
  return tabAllowed("dashboard") ? "dashboard" : null;
}

function tabNeedsGuildConfig() {
  const access = tabAccess.get(state.tab);
  return access !== undefined && access !== "owner";
}

async function loadWorkspaceConfig() {
  // Navigation may show an owner panel while the selected guild's read is pending.
  if (tabNeedsGuildConfig() && await loadConfig() && tabNeedsGuildConfig()) renderTab();
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
  readSessionEnding = true;
  analyticsCache.clear();
  clearReadCache();
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
    if (WORKSPACE_LISTS.has(state.tab) && state.guildId) flushPendingListSearch(state.tab, state.guildId);
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
    /^\/api\/guilds\/(\d+)\/(settings|rules|badwords|testbadwords|known-spam|known-bad-images|commands|templates|copy|logchannels|logfilters|ban|unban|actions|events|members|member-sync|bans|stats|analytics|cases|case-delete-roles|access-roles|server-names)(?:\/|\?|$)/,
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
        "server-names",
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
    setSessionItemWithReadCacheFallback(window.sessionStorage, key, JSON.stringify(saved));
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

async function fetchPrivate(path) {
  const opts = credentialedOpts(path, "GET");
  const res = await fetch(apiUrl(path), opts);
  if (!res.ok) {
    let data = null;
    try {
      data = JSON.parse(await res.text());
    } catch (_) {}
    throwApiFailure(data, res.status);
  }
  return res.blob();
}

async function downloadPrivate(path, filename) {
  const blob = await fetchPrivate(path);
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
    setSessionItemWithReadCacheFallback(window.sessionStorage, key, JSON.stringify(saved));
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
  if (guildAccess() === "none") {
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
    if (tabNeedsGuildConfig()) toast("Could not refresh configuration: " + state.configRead.error, "error");
  } finally {
    if (request === configRequest) {
      state.configRead.activity = "idle";
      if (state.configRead.error && tabNeedsGuildConfig()) renderTab();
    }
  }
}

function selectedGuild() {
  return state.guilds.find((g) => g.id === state.guildId);
}

function applySpamListPatch(config, patch) {
  const rows = config[patch.kind];
  if (!Array.isArray(rows) || patch.previous_revision !== config.config_revision ||
    !/^[a-f0-9]{32}$/.test(patch.previous_revision) || !/^[a-f0-9]{32}$/.test(patch.revision)) return false;
  const knownSpam = patch.kind === "known_spam";
  const key = knownSpam ? "id" : "hash";
  if (patch.operation === "add" && typeof patch.value?.[key] === "string") {
    const row = patch.value;
    if (knownSpam && row.guild_id !== "0" && row.guild_id !== state.guildId) return false;
    // An unchanged row absent from this snapshot may be outside its bounded list.
    if (patch.previous_revision === patch.revision && !rows.some((entry) => entry[key] === row[key])) return false;
    const orderedKey = (entry) => knownSpam ? BigInt(entry.id) : entry.hash;
    const updated = [...rows.filter((entry) => entry[key] !== row[key]), row];
    updated.sort((a, b) => {
      const left = orderedKey(a), right = orderedKey(b);
      return left < right ? -1 : left > right ? 1 : 0;
    });
    config[patch.kind] = updated;
  } else if (patch.operation === "delete" && typeof patch.value === "string") {
    config[patch.kind] = rows.filter((entry) => entry[key] !== patch.value);
  } else {
    return false;
  }
  config.config_revision = patch.revision;
  return true;
}

function applyConfigPatch(res) {
  const patch = res?.config_patch;
  const config = state.config;
  if (!patch || !config) return false;
  const spamList = patch.kind === "known_spam" || patch.kind === "known_bad_images";
  if (spamList && !applySpamListPatch(config, patch)) return false;
  // Any snapshot requested before this acknowledged commit is now stale.
  ++configRequest;
  state.configRead.activity = "idle";
  state.configRead.error = null;
  if (spamList) return true;
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
  // A command save reloads instead (#623): built-in overrides are normalized
  // and aliases resolve across commands, so one saved row can change others.
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

async function reloadAfter(work, onSuccess, capturedGuild) {
  const request = configRequest;
  const enrollment = state.config?.enrollment_id;
  const user = state.me?.user.id;
  const res = await work();
  const scoped = capturedGuild !== undefined;
  if (scoped && (state.guildId !== capturedGuild || state.me?.user.id !== user)) return;
  const canPatch = !scoped || (request === configRequest &&
    state.configRead.activity !== "loading" && state.config?.enrollment_id === enrollment);
  if (!(canPatch && applyConfigPatch(res)) && !(await loadConfig())) return;
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
// The text starts at the button, so a button partway along a line on a narrow
// screen would push it past the edge; it shifts back inside before it paints.
function InfoTip({ id, label, children }) {
  const [open, setOpen] = useState(false);
  const tipRef = useRef(null);
  useLayoutEffect(() => {
    const tip = tipRef.current;
    if (!open || !tip) return;
    tip.style.transform = "";
    const { left, right, width } = tip.getBoundingClientRect();
    if (!width) return;
    const margin = 8;
    const viewport = document.documentElement.clientWidth;
    const shift = right > viewport - margin ? Math.max(viewport - margin - right, margin - left) : left < margin ? margin - left : 0;
    if (shift) tip.style.transform = `translateX(${shift}px)`;
  }, [open]);
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
      { id, ref: tipRef, role: "tooltip", className: "info-tip-text", hidden: !open },
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
  const botNickname = cfg.settings?.bot_name;
  const rules = cfg.rules ?? [];
  const commands = cfg.commands ?? [];
  const rulesOn = rules.filter((r) => r.effective?.enabled).length;
  const rulesDry = rules.filter((r) => r.effective?.dry_run).length;
  const commandsOn = commands.filter((c) => c.effective?.enabled).length;
  const chans = (cfg.log_channels ?? []).filter((l) => l.effective && String(l.effective) !== "0").length;
  const result = useCachedApiRead(`/api/guilds/${guildId}/stats`, "stats");
  const count = (key) =>
    result.loading ? "Loading…" : result.data?.[key] == null ? "Unavailable" : String(result.data[key]);
  const setupLinks = [
    ["01", "Tune automod", "Review rules and test them in dry-run.", "rules"],
    ["02", "Configure commands", "Choose commands and moderator permissions.", "commands"],
    ["03", "Route your logs", "Connect event reports to Discord channels.", "logchannels"],
  ].filter(([, , , tab]) => tabAllowed(tab));
  return h(
    "div",
    { className: "contents" },
    pageHeader(
      "Dashboard",
      g?.name,
      tabAllowed("rules") && h(ActionButton, { className: "btn btn-sm btn-primary", onClick: () => navigate("rules") }, "Review protection"),
      h(ActionButton, { className: "btn btn-sm btn-outline", onClick: () => navigate("events") }, "View audit log"),
    ),
    setupLinks.length > 0 && !setupComplete(cfg) &&
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
            ...setupLinks.map(([number, title, description, tab]) =>
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
          canEditSettings() && h(
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
      h(
        "div",
        { className: "row dashboard-overview-head" },
        h("h3", { className: "card-title", id: "dashboard-overview" }, "Overview"),
        h(LastUpdated, { result }),
        h(ActionButton, { id: "refresh-dashboard", className: "btn btn-sm", onClick: () => result.refresh() }, "Refresh"),
      ),
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
        stat("Bot nickname", botNickname === "" ? "Not set" : botNickname ?? "Unavailable", "bot-name"),
        stat("Badword patterns", (cfg.badwords ?? []).length, "badword-patterns"),
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
  "Open Members to see recovery progress, retry timing, and available controls.",
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
  const series = data?.series;
  const points = React.useMemo(() => toTrendPoints(series), [series]);
  const previous = data?.previous ?? null;
  const rangeLabel = `${days} days`;
  const members = data?.members ?? null;
  const total = (key) => sumSeries(points, key);
  const prev = (key) => (previous ? previous[key] : null);
  const daily = (key) => points.map((point) => point[key] ?? 0);
  const coverage = memberDirectoryBadge(members, guildId, "partial");
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
  const noTrendData = h("p", { className: "an-empty muted" }, "No trend data in this range.");
  return h(
    "div",
    { className: "contents chart-page analytics-page" },
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
        action: memberImportLink(coverage),
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
      analyticsPanel("Enforcement per day", "an-enforcement", body(() => points.length ? h(EnforcementChart, { points }) : noTrendData), "an-span-7"),
      analyticsPanel("Membership per day", "an-membership", body(() => points.length ? h(MembershipChart, { points }) : noTrendData), "an-span-5"),
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

const ONBOARDING_TRAP_RULE = "onboarding_bot_trap";
const ONBOARDING_TRAP_GUIDE = "https://github.com/jatmn/jModGuard/blob/main/docs/ONBOARDING-BOT-TRAP.md";

const MEDIA_RULE_PRESET = "no_links_or_media";
const CHANNEL_REPLY_DEFAULT = {
  template: "{user} No links or media are allowed in {channel}.",
  delete_after_seconds: 10,
};
const CUSTOM_RULE_ACTIONS = ["delete", "log", "channel_reply", "warn", "timeout", "kick", "ban"];

function newMediaRuleDraft() {
  const draft = newCustomRuleDraft();
  const name = "No links or media";
  const description = "Delete links and media in selected channels.";
  return {
    ...draft,
    name,
    description,
    effective: {
      ...draft.effective,
      name,
      description,
      enabled: false,
      skip_moderators: true,
      log_automod: false,
      open_case: false,
      params: {
        kind: "channel_content",
        preset: MEDIA_RULE_PRESET,
        links: true,
        attachments: true,
        gifs: true,
        stickers: true,
        channel_reply: { ...CHANNEL_REPLY_DEFAULT },
      },
      actions: [{ at: 1, action: "delete" }, { at: 1, action: "log" }],
    },
  };
}

function tabRules() {
  const rules = state.config.rules.filter((r) => r.rule_id !== "known_spam" &&
    (state.guildId !== "0" || r.rule_id !== ONBOARDING_TRAP_RULE));
  return h(
    "div",
    {},
    h("h2", {}, "Automod rules"),
    canEditSettings() && h(
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
      state.guildId && state.guildId !== "0"
        ? h(
            ActionButton,
            {
              className: "btn btn-outline btn-sm",
              onClick: () => ui.set({ editor: h(CustomRuleEditor, { record: newMediaRuleDraft(), creating: true }) }),
            },
            "No links or media",
          )
        : null,
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
  const trap = r.rule_id === ONBOARDING_TRAP_RULE;
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
            r.global && !trap ? h("span", { className: "badge badge-soft badge-sm badge-primary scope-tag" }, "global") : null,
          ),
        ),
        h(
          "div",
          { className: "card-actions" },
          canEditSettings() && h(IconButton, { action: "edit", label: "Edit", onClick: () => editRule(r) }),
        ),
      ),
      h("p", {}, r.description),
      trap ? h("p", {}, h("a", { href: ONBOARDING_TRAP_GUIDE, className: "link" }, "Onboarding bot trap setup guide")) : null,
      trap && rule.enabled && canEditSettings()
        ? h(SavedOnboardingTrapWarnings, { roleId: String(rule.params?.role_id ?? "") })
        : null,
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
  if (body.params?.preset === MEDIA_RULE_PRESET && !body.include_channels.length) body.enabled = false;
  if (body.actions.some((action) => action.action === "channel_reply")) {
    body.params.channel_reply ||= { ...CHANNEL_REPLY_DEFAULT };
  }
  return body;
}

// #621: every automod rule editor has the same layout: a live plain-language
// summary, then Trigger, Actions, Exceptions and Scope sections, with rarely
// used options under Advanced. Only the Trigger section differs between rules.

// Built-in rule triggers. Each input keeps a full label for screen readers; the
// visible words around it read as one short sentence. Phrases follow the bot's
// checks in crates/bot/src/automod.rs (mention and message counts must exceed
// the maximum; new-member links count up to the grace period).
const RULE_TRIGGERS = {
  mention_spam: {
    fields: [
      { key: "max_mentions_in_window", label: "Maximum mentions in window", fallback: 5, before: "More than", after: "mentions in" },
      { key: "window_secs", label: "Window (seconds)", fallback: 10, after: "seconds" },
    ],
    hint: "Counts @everyone, @here and role mentions.",
    phrase: (v) =>
      `When someone sends more than ${v.max_mentions_in_window} @everyone, @here or role mentions within ${v.window_secs} seconds`,
  },
  velocity: {
    fields: [
      { key: "max_messages", label: "Maximum messages in window", fallback: 8, max: 1023, before: "More than", after: "messages in" },
      { key: "window_secs", label: "Window (seconds)", fallback: 10, after: "seconds" },
    ],
    phrase: (v) => `When someone sends more than ${v.max_messages} messages within ${v.window_secs} seconds`,
  },
  new_member_url: {
    fields: [
      { key: "grace_secs", label: "Seconds after joining", fallback: 120, before: "A link or invite within", after: "seconds of joining" },
    ],
    phrase: (v) => `When a member posts a link or invite within ${v.grace_secs} seconds of joining`,
  },
  account_age: {
    fields: [
      { key: "min_age_days", label: "Minimum account age (days)", fallback: 3, before: "A message from an account younger than", after: "days" },
    ],
    hint: "Uses the Discord account's age, not time in this server.",
    phrase: (v) => `When an account younger than ${v.min_age_days} days posts a message`,
  },
  attachment_flood: {
    fields: [
      { key: "min_attachments", label: "Common distinct images per message", fallback: 2, min: 1, max: 10, before: "The same", after: "images in" },
      { key: "min_channels", label: "Distinct channels", fallback: 3, min: 2, max: 10, after: "channels within" },
      { key: "window_secs", label: "Window (seconds)", fallback: 60, after: "seconds" },
    ],
    hint: "These thresholds apply to identical image files. The rule also matches the same filename, size and media type in two channels up to 10 seconds apart, within the rule window.",
    phrase: (v) =>
      `When someone posts the same ${v.min_attachments} or more images in ${v.min_channels} or more channels within ${v.window_secs} seconds`,
  },
  repeat_text: {
    fields: [
      { key: "min_length", label: "Minimum length (characters)", fallback: 20, min: 1, max: 2000, before: "The same text of at least", after: "characters in" },
      { key: "min_channels", label: "Distinct channels", fallback: 3, min: 2, max: 10, after: "channels within" },
      { key: "window_secs", label: "Window (seconds)", fallback: 60, after: "seconds" },
    ],
    hint: "Exact match. Spaces at the ends are ignored; upper and lower case differ.",
    phrase: (v) =>
      `When someone posts the same text (at least ${v.min_length} characters) in ${v.min_channels} or more channels within ${v.window_secs} seconds`,
  },
  invite: {
    phrase: (v) =>
      String(v.allowlist_codes || "").trim()
        ? "When someone posts a Discord invite that isn't on the allowed list"
        : "When someone posts a Discord invite",
  },
  badword: {
    text: "A message matches your Badwords list.",
    hint: "Manage words and advanced regex on the Badwords page.",
    phrase: () => "When a message matches your Badwords list",
  },
  known_spam: {
    text: "A message matches a saved Known spam message or URL pattern.",
    hint: "Manage samples and URLs on the Known spam page.",
    phrase: () => "When a message matches Known spam",
  },
};

const RULE_ACTION_LABELS = {
  log: "Log only",
  channel_reply: "Reply in channel",
  delete: "Delete message",
  warn: "Warn",
  timeout: "Timeout",
  kick: "Kick",
  ban: "Ban",
};

function banMessagesLabel(step) {
  return step.purge_last_hour ? "Delete last hour" : step.delete_days ? `Delete ${step.delete_days} day(s)` : "Keep messages";
}

// One escalation step in the words the summary uses, e.g. "Ban (keep messages)".
function ruleStepPhrase(step) {
  if (step.action === "timeout") return `Timeout ${Math.max(1, Number(step.duration_minutes) || 0)} min`;
  if (step.action === "ban") return `Ban (${banMessagesLabel(step).toLowerCase()})`;
  if (step.action === "delete") return "Delete the message";
  return RULE_ACTION_LABELS[step.action] || ruleActionName(step);
}

// What the bot does on a match: one step reads as itself, several as a strike list.
function ruleOutcome(rule) {
  const steps = describeRuleActions(rule, ruleStepPhrase);
  if (steps === "No actions set") return "no action";
  return steps.startsWith("Strike 1: ") && !steps.includes(" · ") ? steps.slice("Strike 1: ".length) : steps;
}

// Exact image spam and Repeated text store test and enforce as different shapes:
// test logs only, enforce deletes then bans. Thresholds come from `number(key)`.
function setSpamRuleMode(rule, ruleId, punish, number, ban) {
  const textRule = ruleId === "repeat_text";
  rule.params = textRule
    ? {
        mode: "exact_text",
        min_channels: number("min_channels"),
        window_secs: number("window_secs"),
        min_length: number("min_length"),
      }
    : {
        mode: punish ? "exact_image" : "exact_image_test",
        min_channels: number("min_channels"),
        min_attachments: number("min_attachments"),
        window_secs: number("window_secs"),
      };
  rule.dry_run = !punish;
  rule.actions = punish
    ? [
        { at: 1, action: "delete", reason: "spam match", duration_minutes: 0, delete_days: 0 },
        { at: 1, action: "ban", reason: "spam match", duration_minutes: 0, delete_days: ban.delete_days || 0, purge_last_hour: ban.purge_last_hour === true },
      ]
    : [
        {
          at: 1,
          action: "log",
          reason: textRule ? "Repeated text test" : "Exact image spam test",
          duration_minutes: 0,
          delete_days: ban.delete_days || 0,
          purge_last_hour: ban.purge_last_hour === true,
        },
      ];
}

// The ban step a spam rule keeps across test and enforce modes.
function spamRuleBan(rule) {
  return (rule.actions || []).find((action) => action.action === "ban") || (rule.actions || [])[0] || { purge_last_hour: true };
}

// The "then" half of a rule summary, in the bot's terms.
function ruleOutcomeFor(ruleId, rule, spamBan) {
  if (ruleId === "known_spam") return "Ban and delete the last hour of messages";
  if (spamRule(ruleId)) return `Delete the messages and ban (${banMessagesLabel(spamBan).toLowerCase()})`;
  return ruleOutcome(rule);
}

function ruleSummaryText(trigger, outcome, enabled, dryRun) {
  const state = !enabled ? " Off: the rule is ignored." : dryRun ? " Dry-run: only logs." : "";
  return `${trigger}: ${outcome}.${state}`;
}

function RuleSummary({ text }) {
  return h("p", { className: "rule-summary", role: "status", "aria-live": "polite" }, text);
}

function EditorSection({ title, hint, className = "", children }) {
  return h(
    "section",
    { className: `editor-section ${className}`.trim(), "aria-label": title },
    h("h4", { className: "editor-section-title" }, title),
    hint ? h("p", { className: "field-help" }, hint) : null,
    children,
  );
}

// Enabled and Dry-run sit in the header as toggles; their labels stay the
// exact control names.
function RuleEditorHeader({ title, enabled, dryRun, dryRunDisabled, disabled, requireChannels, hasChannels, onEnabled, onDryRun, dryRunRef }) {
  return h(
    "div",
    { className: "editor-heading rule-editor-header" },
    h("div", { className: "rule-editor-title" }, h("span", { className: "eyebrow" }, "AUTOMOD RULE"), h("h3", { className: "card-title" }, title)),
    h(
      "div",
      { className: "rule-toggles" },
      h(
        "label",
        { className: "toggle-label" },
        h(Input, {
          type: "checkbox",
          className: "toggle toggle-sm",
          ...(requireChannels ? { checked: enabled, disabled: !hasChannels } : { defaultChecked: enabled, disabled }),
          onChange: (e) => onEnabled(e.target.checked),
        }),
        "Enabled",
      ),
      h(
        "label",
        { className: "toggle-label" },
        h(Input, {
          ref: dryRunRef,
          type: "checkbox",
          className: "toggle toggle-sm",
          defaultChecked: dryRun,
          disabled: disabled || dryRunDisabled,
          onChange: (e) => onDryRun(e.target.checked),
        }),
        "Dry-run (log only)",
      ),
    ),
  );
}

function TriggerSentence({ fields, values, onValue }) {
  return h(
    "div",
    { className: "trigger-sentence" },
    ...fields.map((f) =>
      h(
        React.Fragment,
        { key: f.key },
        f.before ? h("span", null, f.before) : null,
        h(
          Field,
          { className: "trigger-field" },
          h(Label, { className: "sr-only" }, f.label),
          h(Input, {
            type: "number",
            className: "input input-num",
            min: f.min ?? (f.key === "window_secs" ? 1 : 0),
            max: f.max ?? (f.key === "window_secs" ? 600 : Number.MAX_SAFE_INTEGER),
            step: 1,
            required: true,
            defaultValue: values[f.key],
            onInput: (e) => onValue(f.key, e.target.value),
          }),
        ),
        f.after ? h("span", null, f.after) : null,
      ),
    ),
  );
}

// Role and channel names come from the bot's latest snapshot of this server
// (POST /api/guilds/{id}/server-names); user names from the Members directory.
// Global rules have no server, so their pickers take IDs only.
function useServerNames(userIds) {
  const guildId = state.guildId;
  const live = Boolean(guildId) && guildId !== "0";
  const [names, setNames] = useState(() => ({
    status: live ? "loading" : "unavailable",
    roles: [],
    channels: [],
    mediaPresetChannelKinds: [],
    users: new Map(),
  }));
  useEffect(() => {
    if (!live) return undefined;
    const controller = new AbortController();
    apiRead(`/api/guilds/${guildId}/server-names`, controller.signal).then(
      (data) =>
        setNames((current) => ({
          ...current,
          status: "loaded",
          roles: Array.isArray(data?.roles) ? data.roles : [],
          channels: Array.isArray(data?.channels) ? data.channels : [],
          onboardingTrap: data?.onboarding_trap,
          mediaPresetChannelKinds: Array.isArray(data?.media_preset_channel_kinds) ? data.media_preset_channel_kinds : [],
        })),
      () => {
        if (!controller.signal.aborted) setNames((current) => ({ ...current, status: "failed" }));
      },
    );
    const ids = [...new Set(userIds)].slice(0, 25);
    if (ids.length) {
      api(`/api/guilds/${guildId}/members/names`, "POST", { user_ids: ids }, controller.signal).then(
        (data) => {
          if (controller.signal.aborted || state.guildId !== guildId) return;
          const labels = Object.entries(data ?? {});
          if (labels.length) {
            setNames((current) => ({ ...current, users: new Map([...labels, ...current.users]) }));
          }
        },
        () => {},
      );
    }
    return () => controller.abort();
  }, [guildId]);
  const rememberUser = (id, name) => setNames((current) => ({ ...current, users: new Map(current.users).set(id, name) }));
  return [names, rememberUser];
}

function roleColor(role) {
  return role?.color ? `#${Number(role.color).toString(16).padStart(6, "0")}` : null;
}

function PickerChip({ kind, id, names, onRemove }) {
  const role = kind === "role" ? names.roles.find((r) => r.id === id) : null;
  const channel = kind === "channel" ? names.channels.find((c) => c.id === id) : null;
  const user = kind === "user" ? names.users.get(id) : null;
  const known = role?.name ?? channel?.name ?? user;
  const text = known != null ? (kind === "role" ? `@${known}` : kind === "channel" ? `#${known}` : known) : `${kind === "role" ? "Role" : kind === "channel" ? "Channel" : "User"} ${id}`;
  return h(
    "li",
    { className: "badge badge-soft picker-chip", title: id },
    kind === "role" ? h("span", { className: "role-dot", style: roleColor(role) ? { background: roleColor(role) } : null, "aria-hidden": "true" }) : null,
    kind === "channel" ? h(Icon, { name: channel?.kind === "voice" || channel?.kind === "stage" ? "volume" : "hash", intent: "neutral" }) : null,
    kind === "user" ? h("span", { className: "server-icon server-icon-letter", "aria-hidden": "true" }, (Array.from(String(known || "?"))[0] || "?").toUpperCase()) : null,
    h("span", { className: "picker-chip-text" }, text),
    h(
      ActionButton,
      { type: "button", className: "picker-chip-remove", "aria-label": `Remove ${text}`, title: `Remove ${text}`, onClick: onRemove },
      h(Icon, { name: "x-circle" }),
    ),
  );
}

// Chips for the chosen roles, channels or users, plus controls to add more: a
// native select of names when the bot has shared them, a member search for
// users, and an ID box (Enter adds IDs, mentions or links). A channel picker
// restricted by type requires a choice from the snapshot. Other pickers also
// accept IDs the snapshot lacks, such as threads. Save commits
// or refuses text still in the box through `pending`, as the old ID fields did.
// The box text is picker state, so it survives the box being replaced when
// names arrive, and member results are kept with the query they answer.
// `draft: false` is for filters: choosing a value changes the view, not unsaved settings.
// `memberStatus` widens member search, e.g. to banned or departed case subjects.
function IdPicker({ kind, label, hint, ids, names, rememberUser, onChange, pending, single = false, idLabel, draft = true, memberStatus = "current", channelKinds = null }) {
  const [, bump] = useState(0);
  const [text, setText] = useState("");
  const [found, setFound] = useState(null);
  const [error, setError] = useState("");
  const searchAbort = useRef(null);
  const searchTimer = useRef(null);
  const flushRef = useRef(null);
  // Only results for the text now in the box can be chosen.
  const results = found && found.query === text.trim() ? found.members : null;
  const searchError = results ? found.error : null;
  const update = (next) => {
    ids.splice(0, ids.length, ...next);
    if (draft) markDirty();
    bump((n) => n + 1);
    onChange?.();
  };
  const add = (id) => {
    if (!id || ids.includes(id)) return;
    update(single ? [id] : [...ids, id]);
  };
  // Stop a search still waiting for typing to settle or still in flight.
  const cancelSearch = () => {
    clearTimeout(searchTimer.current);
    searchTimer.current = null;
    searchAbort.current?.abort();
  };
  // Drop shown results and ignore any pending search.
  const clearSearch = () => {
    cancelSearch();
    setFound(null);
  };
  const addTyped = () => {
    const raw = text.trim();
    if (!raw) return true;
    const typed = raw.split(",").map((part) => parseSnowflake(part));
    if (typed.some((id) => !id)) {
      setError(kind === "user" && names.status !== "unavailable"
        ? "Pick a matching member, or enter IDs as digits or mentions."
        : "Enter IDs as digits, mentions or links.");
      return false;
    }
    setError("");
    update(single ? [typed.at(-1)] : [...ids, ...typed.filter((id, i) => !ids.includes(id) && typed.indexOf(id) === i)]);
    setText("");
    clearSearch();
    return true;
  };
  const choose = (member) => {
    rememberUser?.(member.user_id, member.display_name);
    add(member.user_id);
    setText("");
    clearSearch();
  };
  const options =
    kind === "role"
      ? names.roles.filter((r) => r.id !== state.guildId)
      : kind === "channel"
        ? names.channels.filter((c) => channelKinds ? channelKinds.includes(c.kind) : c.kind !== "category")
        : [];
  const useSelect = channelKinds !== null || (kind !== "user" && names.status === "loaded" && options.length > 0);
  const noun = kind === "role" ? "role" : kind === "channel" ? "channel" : "member";
  // Names wait for typing to settle, as list search does, so one name sends one read.
  const search = (value) => {
    cancelSearch();
    const q = value.trim();
    if (kind !== "user" || names.status === "unavailable" || !q || parseSnowflake(q)) {
      setFound(null);
      return;
    }
    const guildId = state.guildId;
    searchTimer.current = setTimeout(() => {
      searchTimer.current = null;
      if (state.guildId === guildId) return readMembers(guildId, q);
    }, SEARCH_DELAY_MS);
  };
  const readMembers = async (guildId, q) => {
    const controller = new AbortController();
    searchAbort.current = controller;
    try {
      const page = await apiRead(`/api/guilds/${guildId}/members?${new URLSearchParams({ q, limit: "8", status: memberStatus })}`, controller.signal);
      if (searchAbort.current === controller && !controller.signal.aborted) {
        setFound({ query: q, members: Array.isArray(page?.members) ? page.members : [] });
      }
    } catch (error) {
      // A failed search is not an empty one: say why, so an ID can be pasted instead.
      if (searchAbort.current === controller && !controller.signal.aborted) {
        setFound({ query: q, members: [], error: String(error?.message || error).replace(/[.\s]+$/, "") });
      }
    }
  };
  useEffect(() => cancelSearch, []);
  flushRef.current = addTyped;
  useEffect(() => {
    if (!pending) return undefined;
    const flush = () => flushRef.current();
    pending.add(flush);
    return () => pending.delete(flush);
  }, [pending]);
  const textBox = (labelled) =>
    h(Input, {
      type: "text",
      value: text,
      className: "input input-sm",
      "aria-label": labelled ? undefined : idLabel || `Add a ${noun} by ID`,
      placeholder:
        kind === "user" && names.status !== "unavailable"
          ? "Search members by name, or paste an ID"
          : labelled
            ? `Paste a ${noun} ID${kind === "channel" ? ", #mention or link" : " or @mention"}`
            : "Or paste an ID",
      onChange: (e) => {
        setText(e.target.value);
        setError("");
        search(e.target.value);
      },
      onKeyDown: (e) => {
        // Enter that confirms an input method candidate belongs to the input method.
        if (e.key !== "Enter" || e.nativeEvent.isComposing) return;
        e.preventDefault();
        if (results?.length === 1 && !parseSnowflake(text)) {
          choose(results[0]);
          return;
        }
        // A name still waiting for typing to settle is searched now, not rejected as an ID.
        if (searchTimer.current && text.split(",").some((part) => !parseSnowflake(part))) {
          cancelSearch();
          readMembers(state.guildId, text.trim());
          return;
        }
        addTyped();
      },
      onBlur: () => {
        if (parseSnowflake(text)) addTyped();
      },
    });
  const categories = new Map(names.channels.filter((c) => c.kind === "category").map((c) => [c.id, c.name]));
  const channelGroups = [];
  if (kind === "channel") {
    for (const channel of options) {
      const group = categories.get(channel.parent_id) ?? "";
      let entry = channelGroups.find((g) => g.name === group);
      if (!entry) channelGroups.push((entry = { name: group, channels: [] }));
      entry.channels.push(channel);
    }
  }
  const optionFor = (item) =>
    h("option", { key: item.id, value: item.id, disabled: ids.includes(item.id) }, kind === "role" ? `@${item.name}` : `#${item.name}`);
  return h(
    "div",
    { className: "field id-picker" },
    h(
      Field,
      { className: "fieldset" },
      h(Label, { className: "fieldset-legend" }, label),
      useSelect
        ? h(
            Select,
            {
              className: "select select-sm",
              defaultValue: "",
              disabled: channelKinds !== null && options.length === 0,
              onChange: (e) => {
                add(e.target.value);
                e.target.value = "";
              },
            },
            h("option", { value: "" }, single ? `Choose a ${noun}…` : `Add a ${noun}…`),
            kind === "role"
              ? options.map(optionFor)
              : channelGroups.map((group) =>
                  group.name
                    ? h("optgroup", { key: group.name, label: group.name }, group.channels.map(optionFor))
                    : group.channels.map(optionFor),
                ),
          )
        : textBox(true),
    ),
    useSelect && channelKinds === null ? textBox(false) : null,
    results
      ? h(
          "ul",
          { className: "menu menu-sm picker-results", "aria-label": `Matching ${noun}s` },
          results.length
            ? results.map((member) =>
                h(
                  "li",
                  { key: member.user_id },
                  h(
                    ActionButton,
                    { type: "button", onClick: () => choose(member) },
                    member.display_name,
                    h("span", { className: "muted" }, member.user_id),
                  ),
                ),
              )
            : searchError
              ? h("li", { className: "err", role: "alert" }, `Couldn't search members: ${searchError}. Paste their ID instead.`)
              : h("li", { className: "muted" }, "No matching members."),
        )
      : null,
    error ? h("p", { className: "err", role: "alert" }, error) : null,
    ids.length
      ? h(
          "ul",
          { className: "picker-chips", "aria-label": label },
          ids.map((id) => h(PickerChip, { key: id, kind, id, names, onRemove: () => update(ids.filter((other) => other !== id)) })),
        )
      : null,
    hint ? h("p", { className: "field-help" }, hint) : null,
  );
}

function ruleNamesHint(names) {
  if (names.status === "unavailable") return "Global rules use IDs: names differ in every server.";
  if (names.status === "loaded" && !names.roles.length && !names.channels.length)
    return "Names appear once the bot next connects to this server. Paste IDs meanwhile.";
  if (names.status === "failed") return "Couldn't load names. Paste IDs instead.";
  return null;
}

// Text still in an ID box when Save is pressed is added, or the save stops with
// an error, so nothing typed is lost silently.
function flushPendingIds(pending) {
  const ok = [...pending].map((flush) => flush()).every(Boolean);
  if (!ok) toast("Fix the ID fields marked in red before saving.", "error");
  return ok;
}

// Shared Exceptions section: who and where the rule never checks.
function RuleExceptions({ rule, names, rememberUser, onChange, pending }) {
  return h(
    EditorSection,
    { title: "Exceptions", hint: ruleNamesHint(names) || "The rule never acts on these." },
    h(
      "label",
      { className: "toggle-label" },
      h(Input, {
        type: "checkbox",
        className: "toggle toggle-sm",
        defaultChecked: rule.skip_moderators,
        onChange: (e) => {
          rule.skip_moderators = e.target.checked;
          onChange();
        },
      }),
      "Skip moderators",
    ),
    h(
      "div",
      { className: "editor-columns" },
      h(IdPicker, { kind: "role", label: "Skip roles", ids: rule.whitelist_roles, names, onChange, pending }),
      h(IdPicker, { kind: "channel", label: "Skip channels", ids: rule.whitelist_channels, names, onChange, pending }),
      h(IdPicker, { kind: "user", label: "Skip users", ids: rule.whitelist_users, names, rememberUser, onChange, pending }),
    ),
  );
}

function RuleAdvanced({ rule, names, onChange, pending, includePicker = true, children }) {
  return h(
    "details",
    { className: "editor-advanced" },
    h("summary", null, "Advanced"),
    includePicker ? h(IdPicker, {
      kind: "channel",
      label: "Only check these channels",
      hint: "Leave empty to check every channel.",
      ids: rule.include_channels,
      names,
      onChange,
      pending,
    }) : null,
    children,
  );
}

function ensureRuleLists(rule) {
  for (const key of ["whitelist_roles", "whitelist_channels", "whitelist_users", "include_channels"]) {
    rule[key] = Array.isArray(rule[key]) ? rule[key].map(String) : [];
  }
  return rule;
}

function customRulePhrase(rule, names) {
  const p = rule.params || {};
  if (p.kind === "word") {
    const count = (p.words || []).length;
    return count ? `When a message contains ${count === 1 ? "the listed word" : `any of ${count} listed words`}` : "When a message contains a listed word (none yet)";
  }
  if (p.kind === "channel_content") {
    const parts = [
      p.links && "a link",
      p.attachments && "an attachment",
      p.gifs && !p.links && "a GIF link",
      p.stickers && "a sticker",
    ].filter(Boolean);
    return parts.length ? `When someone posts ${parts.join(" or ")}` : "Choose at least one content type";
  }
  if (p.kind === "everyone_attempt") return "When someone without permission tries to mention @everyone or @here";
  if (p.kind === "honeypot_role") {
    const role = names.roles.find((r) => r.id === String(p.role_id || ""));
    return p.role_id ? `When a member gets ${role ? `@${role.name}` : `role ${p.role_id}`}` : "When a member gets the chosen role (none yet)";
  }
  return p.pattern ? `When a message matches the pattern ${p.pattern}` : "When a message matches the pattern (none yet)";
}

function CustomRuleEditor({ record: r, creating }) {
  const [rule] = useState(() => ensureRuleLists(JSON.parse(JSON.stringify(r.effective))));
  const [, bumpForm] = useState(0);
  const refresh = () => bumpForm((n) => n + 1);
  const [names, rememberUser] = useServerNames(rule.whitelist_users);
  const [honeypotRoles] = useState(() => (rule.params?.role_id ? [String(rule.params.role_id)] : []));
  const pending = useRef(new Set()).current;
  const kind = rule.params?.kind || "pattern";
  const preset = rule.params?.preset === MEDIA_RULE_PRESET;
  const actions = rule.actions;
  const actionChoices = CUSTOM_RULE_ACTIONS.filter((action) => kind !== "honeypot_role" || action !== "channel_reply");
  const unusedActions = actionChoices.filter((choice) => !actions.some((action) => action.action === choice));
  const reply = rule.params.channel_reply || CHANNEL_REPLY_DEFAULT;
  const setReply = (key, value) => {
    rule.params.channel_reply = { ...reply, [key]: value };
    refresh();
  };
  const summary = ruleSummaryText(
    customRulePhrase(rule, names),
    actions.map((action) => action.action === "log" ? "Log to automod" : ruleStepPhrase(action)).join(", then "),
    rule.enabled,
    rule.dry_run,
  );
  return h(
    DialogPanel,
    { className: "modal-box rule-editor", onInput: trackDraft },
    h(RuleEditorHeader, {
      title: creating ? (preset ? r.name : "Add custom rule") : `Edit custom rule: ${r.name || r.rule_id}`,
      enabled: rule.enabled,
      requireChannels: preset,
      hasChannels: rule.include_channels.length > 0,
      dryRun: rule.dry_run,
      dryRunDisabled: creating,
      onEnabled: (on) => {
        rule.enabled = on;
        refresh();
      },
      onDryRun: (on) => {
        rule.dry_run = on;
        refresh();
      },
    }),
    h(RuleSummary, { text: summary }),
    h(
      EditorSection,
      { title: "Trigger", hint: creating ? "New rules start in dry-run so you can check them first." : null },
      h(
        "div",
        { className: "editor-columns" },
        field("Name", h(Input, { type: "text", defaultValue: rule.name, className: "input input-sm", onInput: (e) => (rule.name = e.target.value) })),
        !preset ? field(
          "Type",
          h(
            Select,
            {
              className: "select select-sm",
              defaultValue: kind,
              onChange: (e) => {
                const next = e.target.value;
                const prev = rule.params || {};
                rule.params = { kind: next };
                if (prev.channel_reply) rule.params.channel_reply = prev.channel_reply;
                if (next === "pattern") rule.params.pattern = prev.pattern || "";
                if (next === "word") rule.params.words = Array.isArray(prev.words) ? prev.words : [];
                if (next === "channel_content") {
                  rule.params.links = prev.links !== false;
                  rule.params.attachments = prev.attachments === true;
                }
                if (next === "honeypot_role") {
                  rule.params.role_id = prev.role_id || honeypotRoles[0] || "";
                  rule.actions = actions.filter((action) => action.action !== "channel_reply");
                  if (!rule.actions.length || (rule.actions.length === 1 && rule.actions[0].action === "delete")) {
                    rule.actions = [{ at: 1, action: "kick", reason: rule.actions[0]?.reason || "" }];
                  }
                }
                markDirty();
                refresh();
              },
            },
            h("option", { value: "pattern" }, "Pattern (regex)"),
            h("option", { value: "word" }, "Word list"),
            h("option", { value: "channel_content" }, "Channel content"),
            h("option", { value: "everyone_attempt" }, "@everyone / @here attempt"),
            h("option", { value: "honeypot_role" }, "Honeypot role"),
          ),
        ) : null,
      ),
      kind === "pattern"
        ? field(
            "Pattern",
            h(Input, {
              type: "text",
              className: "input input-sm mono",
              defaultValue: rule.params.pattern || "",
              onInput: (e) => {
                rule.params.pattern = e.target.value;
                refresh();
              },
            }),
            h("p", { className: "field-help" }, "A regular expression. Also checked with disguises removed."),
          )
        : null,
      kind === "word"
        ? field(
            "Words",
            h(Input, {
              type: "text",
              className: "input input-sm",
              defaultValue: (rule.params.words || []).join(","),
              onInput: (e) => {
                rule.params.words = e.target.value.split(",").map((w) => w.trim()).filter(Boolean);
                refresh();
              },
            }),
            h("p", { className: "field-help" }, "Separate words with commas."),
          )
        : null,
      kind === "channel_content" ? h(
        React.Fragment,
        null,
        h(
          "div",
          { className: "row checkbox-group" },
          ...[
            ["links", "Links"],
            ["attachments", "Attachments and images"],
            ["gifs", "GIF picker links"],
            ["stickers", "Stickers"],
          ].map(([key, label]) => h(
            "label",
            { key, className: "checkbox-label" },
            h(Input, {
              type: "checkbox",
              className: "checkbox",
              checked: rule.params[key] === true,
              onChange: (e) => { rule.params[key] = e.target.checked; refresh(); },
            }),
            label,
          )),
        ),
        h("p", { className: "field-help" }, "Links includes GIF picker links. To block just GIF picker links, turn Links off."),
        field(
          "Allowed domains",
          h(Input, {
            type: "text",
            className: "input input-sm",
            defaultValue: (rule.params.allowed_domains || []).join(", "),
            onInput: (e) => {
              rule.params.allowed_domains = e.target.value.split(",").map((domain) => domain.trim()).filter(Boolean);
            },
          }),
          h("p", { className: "field-help" }, "Separate exact domains with commas. Each subdomain needs its own entry."),
        ),
      ) : null,
      kind === "everyone_attempt" ? h("p", { className: "field-help" }, "Members allowed to mention everyone are never caught.") : null,
      kind === "honeypot_role"
        ? h(IdPicker, {
            kind: "role",
            label: "Honeypot role",
            hint: "Members who take this role get the action below.",
            ids: honeypotRoles,
            names,
            pending,
            single: true,
            onChange: () => {
              rule.params.role_id = honeypotRoles[0] || "";
              refresh();
            },
          })
        : null,
    ),
    h(
      EditorSection,
      { title: "Actions", hint: "Run in the order shown when this rule wins a match." },
      ...actions.map((action, index) => h(
        React.Fragment,
        { key: itemKey(action) },
        h(RuleAction, {
          action,
          index,
          strike: false,
          clearBan: true,
          choices: actionChoices.filter((choice) => choice === action.action || unusedActions.includes(choice)),
          remove: actions.length > 1 ? () => {
            actions.splice(index, 1);
            markDirty();
            refresh();
          } : null,
          onChange: refresh,
        }),
        actions.length > 1
          ? h("div", { className: "row" }, ...[-1, 1].map((offset) => {
              const next = index + offset;
              const direction = offset < 0 ? "up" : "down";
              return h(ActionButton, {
                key: direction,
                className: "btn btn-sm",
                disabled: next < 0 || next >= actions.length,
                "aria-label": `Move action ${index + 1} ${direction}`,
                onClick: () => {
                  [actions[index], actions[next]] = [actions[next], actions[index]];
                  markDirty();
                  refresh();
                },
              }, `Move ${direction}`);
            }))
          : null,
        action.action === "channel_reply" ? h(
          React.Fragment,
          null,
          field(
            "Reply text",
            h(Input, {
              type: "text",
              className: "input input-sm",
              maxLength: 2000,
              defaultValue: reply.template,
              onInput: (e) => setReply("template", e.target.value),
            }),
            h("p", { className: "field-help" }, "Use {user} and {channel} for mentions. You can paste Discord custom emoji markup."),
          ),
          field(
            "Delete reply after (seconds)",
            h(Input, {
              type: "number",
              className: "input input-sm",
              min: 0,
              max: 300,
              defaultValue: reply.delete_after_seconds ?? 10,
              onInput: (e) => setReply("delete_after_seconds", e.target.value === "" ? 10 : e.target.valueAsNumber),
            }),
            h("p", { className: "field-help" }, "Use 0 to keep replies. Each member can receive one reply per channel every 30 seconds."),
          ),
        ) : null,
      )),
      unusedActions.length ? h(ActionButton, {
        className: "btn btn-sm btn-outline",
        onClick: () => {
          const kind = unusedActions[0];
          actions.push({ at: 1, action: kind, ...(kind === "timeout" ? { duration_minutes: 60 } : {}) });
          markDirty();
          refresh();
        },
      }, "Add action") : null,
    ),
    h(RuleExceptions, { rule, names, rememberUser, onChange: refresh, pending }),
    h(
      EditorSection,
      { title: "Scope" },
      preset ? h(IdPicker, {
        kind: "channel",
        label: "Only check these channels",
        hint: "Pick a channel with direct text chat. Choices appear after the bot shares the channel list.",
        ids: rule.include_channels,
        names,
        channelKinds: names.mediaPresetChannelKinds,
        pending,
        onChange: () => {
          if (!rule.include_channels.length) rule.enabled = false;
          refresh();
        },
      }) : h("p", { className: "field-help" }, "Custom rules apply to this server only."),
      h(
        RuleAdvanced,
        { rule, names, onChange: refresh, pending, includePicker: !preset },
        h(
          "div",
          { className: "row checkbox-group" },
          !actions.some((action) => action.action === "log") ? h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.log_automod, onChange: (e) => (rule.log_automod = e.target.checked) }), "Post to automod log") : null,
          h("label", { className: "checkbox-label" }, h(Input, { type: "checkbox", className: "checkbox", defaultChecked: rule.open_case, onChange: (e) => (rule.open_case = e.target.checked) }), "Open a case"),
        ),
        kind === "honeypot_role" ? h("p", { className: "field-help" }, "For a hidden case, turn on Open a case and turn off Post to automod log.") : null,
      ),
    ),
    h(
      "div",
      { className: "modal-action" },
      h(
        ActionButton,
        {
          className: "btn btn-primary",
          onClick: async () => {
            if (!flushPendingIds(pending)) return;
            const data = buildCustomRuleSaveBody(rule, creating);
            const saved = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", { scope: "guild", data });
            if (!applyConfigPatch(saved) && !(await loadConfig())) return;
            const persisted = state.config.rules.find((record) => record.rule_id === r.rule_id)?.effective;
            const removed = preset && persisted ? data.include_channels.length - persisted.include_channels.length : 0;
            if (removed > 0) {
              const channels = removed === 1 ? "1 unavailable channel" : `${removed} unavailable channels`;
              const disabled = data.enabled && !persisted.enabled ? " The rule is now off." : "";
              toast(`Rule saved. Removed ${channels}.${disabled}`);
            } else {
              toast("Rule saved", "success");
            }
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

function OnboardingTrapWarnings({ names, roleId }) {
  if (names.status === "loading") return null;
  const setup = names.onboardingTrap;
  const warnings = [];
  if (!setup || setup.bot_can_kick == null) warnings.push("Bot setup checks are unavailable. Check jModGuard’s role and permissions in Discord.");
  if (setup?.bot_can_kick === false) warnings.push("jModGuard lacks Kick Members.");
  if (setup?.roles_at_or_above_bot?.includes(roleId)) warnings.push("Move the trap role below jModGuard’s highest role.");
  if (setup?.roles_with_grants?.includes(roleId)) warnings.push("The trap role grants permissions or channel access. Remove those grants in Discord.");
  if (setup?.roles_with_member_grants?.includes(roleId)) warnings.push("A member with the trap role has an individual channel permission grant. Review their channel overrides in Discord.");
  if (setup && !names.roles.some((role) => role.id === roleId)) warnings.push("The selected trap role is missing from the bot’s server snapshot.");
  return warnings.length ? h("ul", { className: "field-help", role: "status" }, ...warnings.map((warning) =>
    h("li", { key: warning }, warning))) : null;
}

function SavedOnboardingTrapWarnings({ roleId }) {
  const [names] = useServerNames([]);
  return h(OnboardingTrapWarnings, { names, roleId });
}

function OnboardingTrapEditor({ record }) {
  const [expected] = useState(() => record.effective);
  const [rule] = useState(() => JSON.parse(JSON.stringify(record.effective)));
  const [roleIds] = useState(() => rule.params?.role_id ? [String(rule.params.role_id)] : []);
  const [windowMinutes, setWindowMinutes] = useState(() => String(rule.params?.join_window_minutes ?? 30));
  const [cleanupMinutes, setCleanupMinutes] = useState(() => String(rule.params?.cleanup_window_minutes ?? 0));
  const [names] = useServerNames([]);
  const [, setRevision] = useState(0);
  const refresh = () => setRevision((value) => value + 1);
  const pending = useRef(new Set()).current;
  const hasRole = roleIds.length > 0 && roleIds[0] !== state.guildId;
  return h(
    DialogPanel,
    { className: "modal-box rule-editor", onInput: trackDraft },
    h(RuleEditorHeader, {
      key: hasRole,
      title: record.name,
      enabled: rule.enabled && hasRole,
      dryRun: rule.dry_run,
      disabled: !hasRole,
      onEnabled: (on) => { rule.enabled = on; refresh(); },
      onDryRun: (on) => { rule.dry_run = on; refresh(); },
    }),
    h(
      EditorSection,
      { title: "Trigger" },
      h(IdPicker, {
        kind: "role",
        label: "Trap role",
        hint: "Pick the role granted by the optional Discord Onboarding answer.",
        ids: roleIds,
        names: { ...names, roles: names.roles.filter((role) => role.id !== state.guildId && !role.managed) },
        pending,
        single: true,
        onChange: () => {
          if (!roleIds.length || roleIds[0] === state.guildId) rule.enabled = false;
          refresh();
        },
      }),
      rule.enabled && hasRole ? h(OnboardingTrapWarnings, { names, roleId: roleIds[0] }) : null,
      h(Field, {},
        h(Label, {}, "Join window (minutes)"),
        h(Input, {
          type: "number", className: "input", min: 0, max: 1440, step: 1, required: true,
          value: windowMinutes, onChange: (event) => setWindowMinutes(event.target.value),
        }),
      ),
      h("p", { className: "field-help" }, "Use 0 to check role additions at any time. Moderators, administrators and the server owner are always skipped."),
    ),
    h(
      EditorSection,
      { title: "Actions" },
      h("p", {}, "The trap opens a private kick case and posts to the Member kick and Automod logs."),
      h(Field, {},
        h(Label, {}, "Kick reason"),
        h(Input, { className: "input", value: rule.actions[0].reason, onChange: (event) => {
          rule.actions[0].reason = event.target.value;
          refresh();
        } }),
      ),
      h(Field, {},
        h(Label, {}, "Recent-message cleanup (minutes)"),
        h(Input, {
          type: "number", className: "input", min: 0, max: 60, step: 1, required: true,
          value: cleanupMinutes, onChange: (event) => setCleanupMinutes(event.target.value),
        }),
      ),
      h("p", { className: "field-help" }, "After a successful kick, delete tracked messages the member sent since joining within this window. Use 0 to leave their messages in place."),
    ),
    h("p", {}, h("a", { href: ONBOARDING_TRAP_GUIDE, className: "link" }, "Onboarding bot trap setup guide")),
    h(
      "div",
      { className: "modal-action" },
      h(ActionButton, {
        className: "btn btn-primary",
        onClick: async () => {
          if (!flushPendingIds(pending)) return;
          const roleId = roleIds[0] || null;
          if (roleId === state.guildId) throw new Error("Choose a role other than @everyone");
          if (!/^\d+$/.test(windowMinutes) || Number(windowMinutes) > 1440) throw new Error("Enter a join window from 0 to 1440 minutes");
          if (!/^\d+$/.test(cleanupMinutes) || Number(cleanupMinutes) > 60) throw new Error("Enter a cleanup window from 0 to 60 minutes");
          const data = {
            ...rule,
            enabled: rule.enabled && roleId !== null,
            params: {
              ...rule.params,
              role_id: roleId,
              join_window_minutes: Number(windowMinutes),
              cleanup_window_minutes: Number(cleanupMinutes),
            },
          };
          await reloadAfter(() => api(`/api/guilds/${state.guildId}/rules/${record.rule_id}`, "PUT", { scope: "guild", data, expected }), () => {
            toast("Rule saved", "success");
            closeOverlay();
          });
        },
      }, "Save"),
      h(ActionButton, { className: "btn btn-outline", onClick: () => canLeave() && closeOverlay() }, "Cancel"),
    ),
  );
}

function RuleEditor({ record: r }) {
  const [rule] = useState(() => ensureRuleLists(JSON.parse(JSON.stringify(r.effective))));
  const [, setRevision] = useState(0);
  const refresh = () => setRevision((value) => value + 1);
  const spam = spamRule(r.rule_id);
  const knownSpam = r.rule_id === "known_spam";
  const trigger = RULE_TRIGGERS[r.rule_id] || { phrase: () => "When this rule matches" };
  const fields = trigger.fields || [];
  const inviteCodes = (value) => (Array.isArray(value) ? value.filter((code) => typeof code === "string") : []);
  // Draft trigger values stay strings until Save validates them.
  const [values] = useState(() => {
    const draft = {};
    for (const f of fields) draft[f.key] = String(rule.params?.[f.key] ?? f.fallback);
    // Legacy allowlist_guilds was never read by the bot. Do not reinterpret
    // those guild IDs as invite codes on edit.
    if (r.rule_id === "invite") draft.allowlist_codes = inviteCodes(rule.params?.allowlist_codes).join(", ");
    return draft;
  });
  const [initialValues] = useState(() => ({ ...values }));
  const velocityLimitEdited = useRef(false);
  const [spamBan] = useState(() => ({ ...spamRuleBan(rule) }));
  // Inactive legacy image rows should convert to the safe dry-run mode by default.
  const [dryRun, setDryRun] = useState(() => (spam ? !spamConfigured(r.rule_id, rule) || rule.dry_run : rule.dry_run));
  const dryRunRef = useRef(null);
  const [names, rememberUser] = useServerNames(rule.whitelist_users);
  const pending = useRef(new Set()).current;
  const scopeSelRef = useRef(null);
  const scopeSel = scopeSelect(scopeSelRef);
  const setValue = (key, value) => {
    if (key === "max_messages") velocityLimitEdited.current = true;
    values[key] = value;
    refresh();
  };
  const shown = { ...values };
  for (const { key } of fields) shown[key] = String(values[key]).trim() || "…";
  const outcome = ruleOutcomeFor(r.rule_id, rule, spamBan);
  const summary = ruleSummaryText(trigger.phrase(shown), outcome, rule.enabled, dryRun);

  const actions = knownSpam
    ? h(
        EditorSection,
        { title: "Actions" },
        h("p", { className: "field-help" }, "Ban and purge the previous hour (3600 seconds) on the first match. No strikes."),
      )
    : spam
      ? h(
          EditorSection,
          { title: "Actions", hint: "Deletes the matched messages, then bans. Dry-run logs instead." },
          h("div", { className: "escalation-row" }, h("span", null, "Ban"), h(BanOptions, { action: spamBan, onChange: refresh })),
        )
      : h(
          EditorSection,
          { title: "Actions", hint: "Each match is a strike. The highest step reached runs." },
          ...rule.actions.map((action, i) =>
            h(RuleAction, {
              key: itemKey(action),
              action,
              index: i,
              onChange: refresh,
              remove: () => {
                rule.actions.splice(i, 1);
                markDirty();
                refresh();
              },
            }),
          ),
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
                refresh();
              },
            },
            "+ add step",
          ),
        );

  return h(
    DialogPanel,
    { className: "modal-box rule-editor", onInput: trackDraft },
    h(RuleEditorHeader, {
      title: `Edit rule: ${r.name}`,
      enabled: rule.enabled,
      dryRun,
      dryRunRef,
      onEnabled: (on) => {
        rule.enabled = on;
        refresh();
      },
      onDryRun: (on) => {
        rule.dry_run = on;
        setDryRun(on);
      },
    }),
    h(RuleSummary, { text: summary }),
    h(
      EditorSection,
      { title: "Trigger", hint: trigger.hint },
      trigger.text ? h("p", null, trigger.text) : null,
      fields.length ? h(TriggerSentence, { fields, values, onValue: setValue }) : null,
      r.rule_id === "invite"
        ? field(
            "Allowed invite codes (comma-separated)",
            h(Input, {
              type: "text",
              defaultValue: values.allowlist_codes,
              placeholder: "code-one, code-two",
              className: "input input-sm",
              onInput: (e) => setValue("allowlist_codes", e.target.value),
            }),
            h("p", { className: "field-help" }, "Invites with these codes are allowed."),
          )
        : null,
    ),
    actions,
    h(RuleExceptions, { rule, names, rememberUser, onChange: refresh, pending }),
    h(
      EditorSection,
      { title: "Scope", className: "rule-scope" },
      field("Applies to", scopeSel),
      h(RuleAdvanced, { rule, names, onChange: refresh, pending }),
    ),
    h(
      "div",
      { className: "modal-action" },
      h(
        ActionButton,
        {
          className: "btn btn-primary",
          onClick: async () => {
            let replaceVelocityLimit = false;
            try {
              if (spam) {
                // The header toggle is the source of truth, including scripted checks.
                const punish = !(dryRunRef.current?.checked ?? dryRun);
                setSpamRuleMode(rule, r.rule_id, punish, (key) => Number(values[key]), spamBan);
                if (!spamConfigured(r.rule_id, rule))
                  throw new Error(
                    "Channels must be 2–10 and window 1–600 seconds, all whole numbers. Images 1–10. Text length 1–2000.",
                  );
              } else if (r.rule_id !== "badword" && !knownSpam) {
                const params = {};
                // `rule` is the draft; compare limits with what the target scope stores.
                const persisted =
                  (scopeSelRef.current?.value === "global" && state.guildId !== "0" ? r.global : r.effective)
                    ?.params ?? {};
                const shown = r.effective?.params ?? {};
                for (const { key, label, max } of fields) {
                  const raw = String(values[key] ?? "");
                  const value = Number(raw);
                  const validInteger = raw.trim() !== "" && Number.isSafeInteger(value) && value >= 0;
                  if (max !== undefined) {
                    // Keep an untouched limit or a restored legacy value outside the
                    // accepted range. A valid input edit can replace a legacy value
                    // even when both are displayed as the same number.
                    if (raw === initialValues[key] && (!velocityLimitEdited.current || !validInteger || value > max)) {
                      if (Object.hasOwn(shown, key)) params[key] = shown[key];
                      continue;
                    }
                    replaceVelocityLimit = true;
                  }
                  const unchanged = max === undefined && value === persisted[key];
                  if (raw.trim() === "" || Number.isNaN(value) || (!unchanged && !validInteger))
                    throw new Error(`${label} must be a non-negative whole number.`);
                  if (key === "window_secs" && (value < 1 || value > 600))
                    throw new Error("Window must be 1–600 seconds.");
                  if (max !== undefined && value > max)
                    throw new Error(`${label} must be 0–${max}.`);
                  params[key] = value;
                }
                if (r.rule_id === "invite") {
                  params.allowlist_codes = String(values.allowlist_codes || "")
                    .split(",")
                    .map((code) => code.trim())
                    .filter(Boolean);
                }
                rule.params = params;
              }
            } catch (e) {
              toast("Invalid detection settings: " + e.message, "error");
              return;
            }
            if (!flushPendingIds(pending)) return;
            const scope = scopeSelRef.current.value;
            const saved = await api(`/api/guilds/${state.guildId}/rules/${r.rule_id}`, "PUT", {
              scope,
              data: rule,
              clear_guild_override: scope === "global" && state.guildId !== "0",
              ...(replaceVelocityLimit ? { replace_velocity_limit: true } : {}),
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

// A rule's escalation steps in plain words (#620), e.g. "Strike 1: Warn · Strike 3: Timeout 10 min".
// Lists only the steps the bot can run: at each strike it picks the last step with the highest
// strike count reached (RuleDef::action_for_strike), and a timeout lasts at least one minute.
function ruleActionName(step) {
  if (step.action === "timeout") return `Timeout ${Math.max(1, Number(step.duration_minutes) || 0)} min`;
  if (step.action === "ban")
    return step.purge_last_hour
      ? "Ban and delete the last hour of messages"
      : step.delete_days
        ? `Ban and delete ${step.delete_days} day(s) of messages`
        : "Ban";
  const action = String(step.action || "unknown");
  return action[0].toUpperCase() + action.slice(1);
}
function describeRuleActions(rule, name = ruleActionName) {
  const actions = rule.actions ?? [];
  const at = (step) => step.at ?? 0;
  const forStrike = (strike) =>
    actions.reduce((best, step) => (at(step) <= strike && (!best || at(step) >= at(best)) ? step : best), null);
  // Strikes start at 1, so a step at 0 first runs on strike 1.
  const strikes = [...new Set(actions.map((step) => Math.max(1, at(step))))].sort((a, b) => a - b);
  return strikes.length
    ? strikes.map((strike) => `Strike ${strike}: ${name(forStrike(strike))}`).join(" · ")
    : "No actions set";
}

// List pages (#622): Badwords, Known spam and Known-bad images share one layout:
// the rule's status, the add form, the list, then an optional tester or preview.
// Each page supplies only its add form, table columns and extras.

// A header toggle saves the effective rule with that one change, the same write
// the rule editor's Save makes with its default "This server" scope.
async function saveRuleToggle(record, change) {
  const rule = JSON.parse(JSON.stringify(record.effective));
  if ("dry_run" in change && spamRule(record.rule_id)) {
    setSpamRuleMode(rule, record.rule_id, !change.dry_run, (key) => Number(rule.params?.[key]), spamRuleBan(rule));
  }
  Object.assign(rule, change);
  await reloadAfter(
    () => api(`/api/guilds/${state.guildId}/rules/${record.rule_id}`, "PUT", { scope: "guild", data: rule, clear_guild_override: false }),
    () => toast("Rule saved", "success"),
  );
}

// Status header: Enabled and Dry-run toggles, the rule summary in the editor's
// words, and the editor for actions and exemptions.
function RuleStatusCard({ record, trigger }) {
  // Remount the toggles after every save attempt so they show the saved state.
  const [attempt, setAttempt] = useState(0);
  if (!record) return null;
  const rule = record.effective;
  const inactive = spamRule(record.rule_id) && !spamConfigured(record.rule_id, rule);
  const toggle = (key) => (on) =>
    handleEvent(async () => {
      try {
        await saveRuleToggle(record, { [key]: on });
      } finally {
        setAttempt((value) => value + 1);
      }
    }, null);
  return h(
    "section",
    { className: "card bg-base-100 card-border list-card list-status", "aria-label": "Rule status", "data-no-draft": true },
    h(
      "div",
      { className: "card-body" },
      canEditSettings()
        ? h(RuleEditorHeader, {
            key: attempt,
            title: record.name,
            enabled: rule.enabled,
            dryRun: rule.dry_run,
            disabled: inactive,
            onEnabled: toggle("enabled"),
            onDryRun: toggle("dry_run"),
          })
        : h("h3", { className: "card-title" }, record.name),
      h(RuleSummary, {
        text: inactive
          ? `${record.name} is inactive. Open it to convert it.`
          : ruleSummaryText(trigger, ruleOutcomeFor(record.rule_id, rule, spamRuleBan(rule)), rule.enabled, rule.dry_run),
      }),
      h(
        "div",
        { className: "list-status-actions" },
        record.guild_override
          ? h("span", { className: "badge badge-soft badge-sm badge-neutral override-tag" }, "server override")
          : record.global
            ? h("span", { className: "badge badge-soft badge-sm badge-primary scope-tag" }, "global")
            : null,
        canEditSettings() && h(ActionButton, { className: "btn", onClick: () => editRule(record) }, "Edit actions & exemptions"),
      ),
    ),
  );
}

function ListCard({ title, children, className = "" }) {
  return h(
    "section",
    { className: `card bg-base-100 card-border list-card ${className}`.trim(), "aria-label": title },
    h("div", { className: "card-body" }, h("h3", { className: "card-title" }, title), children),
  );
}

// The row under an add form: the note on the left, the primary Add on the right.
function ListFormSubmit({ children }) {
  return h("div", { className: "list-form-submit" }, children);
}

// The list as a table. Search and filters appear once it has items.
function ListTable({ title, rows, headings, renderRow, searchText, searchLabel, filters = [], empty }) {
  const [query, setQuery] = useState("");
  const [picked, setPicked] = useState({});
  const needle = query.trim().toLowerCase();
  const shown = rows.filter(
    (row) =>
      (!needle || searchText(row).toLowerCase().includes(needle)) &&
      filters.every((filter) => !picked[filter.key] || filter.value(row) === picked[filter.key]),
  );
  return h(
    ListCard,
    { title: `${title} (${rows.length})`, className: "list-table" },
    rows.length
      ? h(
          "div",
          { className: "list-toolbar", "data-no-draft": true },
          h(Input, {
            type: "search",
            className: "input",
            "data-search": true,
            "aria-label": searchLabel,
            placeholder: searchLabel,
            value: query,
            onChange: (event) => setQuery(event.target.value),
          }),
          ...filters.map((filter) =>
            h(
              Select,
              {
                key: filter.key,
                className: "select",
                "aria-label": filter.label,
                value: picked[filter.key] || "",
                onChange: (event) => setPicked({ ...picked, [filter.key]: event.target.value }),
              },
              h("option", { value: "" }, filter.all),
              ...filter.options.map(([value, label]) => h("option", { key: value, value }, label)),
            ),
          ),
        )
      : null,
    rows.length
      ? h(
          DataTable,
          { headings },
          shown.length
            ? shown.map(renderRow)
            : h("tr", null, h("td", { colSpan: headings.length, className: "muted" }, "Nothing matches the search.")),
        )
      : h("p", { className: "muted" }, empty),
  );
}

const SCOPE_FILTER = {
  key: "scope",
  label: "Filter by scope",
  all: "All scopes",
  options: [["guild", "This server"], ["global", "Global"]],
  value: (row) => (String(row.guild_id) === "0" ? "global" : "guild"),
};

function scopeName(row) {
  return String(row.guild_id) === "0" ? "Global" : "This server";
}

function badwordsRule() {
  return (state.config.rules || []).find((rule) => rule.rule_id === "badword");
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
const BADWORD_MATCH_SOURCES = { saved: "Saved list", draft: "Draft above" };

function BadwordEntryCount({ mode }) {
  const pattern = useStore(ui, ({ drafts }) => drafts["badwords:pattern"]?.value ?? "");
  const entryCount = pattern.split(/[,\n\r]/).filter((entry) => entry.trim()).length;
  return h("p", { className: "field-help entry-count", "aria-live": "polite" },
    mode === "regex" ? "One pattern, up to 512 bytes." : `${entryCount}/${BADWORD_MAX_ENTRIES} entries`);
}

function tabBadwords() {
  const words = state.config.badwords;
  const mode = useStore(ui, ({ drafts }) => drafts["badwords:mode"]?.value || "words");
  const testerOpen = useStore(ui, ({ draftBaselines }) => Boolean(draftBaselines["badwords:tester-open"]));
  const patternRef = useRef(null);
  const noteRef = useRef(null);
  const scopeRef = useRef(null);
  const sampleRef = useRef(null);
  const [result, setResult] = useState(null);

  return h(
    "div",
    { className: "list-page" },
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
    h(RuleStatusCard, { record: badwordsRule(), trigger: RULE_TRIGGERS.badword.phrase() }),
    canEditSettings() && h(
      DraftCard,
      { group: "badwords", className: "card bg-base-100 card-border list-card list-add" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Add words"),
        h(
          "div",
          { className: "list-form-row" },
          field(
            "Entry type",
            h(
              Select,
              { draftKey: "badwords:mode", defaultValue: "words", className: "select" },
              h("option", { value: "words" }, "Words or phrases"),
              h("option", { value: "regex" }, "Advanced regex"),
            ),
          ),
          field("Applies to", scopeSelect(scopeRef, "This server", "Global", true, "badwords:scope")),
        ),
        h(
          "div",
          { className: "list-entry" },
          h(Textarea, {
            ref: patternRef,
            draftKey: "badwords:pattern",
            rows: 3,
            maxLength: mode === "regex" ? 512 : 4096,
            "aria-label": mode === "regex" ? "Advanced regex pattern" : "Words or phrases",
            placeholder: mode === "regex" ? "Advanced regex, e.g. (scam|pump and dump)" : "Words or phrases, separated by commas or newlines",
            className: "textarea",
          }),
          h(
            "p",
            { className: "field-help entry-help" },
            mode === "regex"
              ? "Checked as written and as a lowercase copy with disguises removed. Add \\b for whole words."
              : "Matches whole words and catches common disguises.",
          ),
          h(BadwordEntryCount, { mode }),
        ),
        h(
          ListFormSubmit,
          null,
          field(
            "Note (optional)",
            h(Input, { ref: noteRef, draftKey: "badwords:note", type: "text", "data-note": true, className: "input" }),
          ),
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
    h(ListTable, {
      title: "Words and patterns",
      rows: words,
      headings: ["Entry", "Type", "Scope", "Note", "Actions"],
      searchText: (w) => `${w.word || w.pattern} ${w.note || ""}`,
      searchLabel: "Search entries and notes",
      filters: [
        {
          key: "type",
          label: "Filter by type",
          all: "All types",
          options: [["word", "Word or phrase"], ["regex", "Advanced regex"]],
          value: (w) => (w.word ? "word" : "regex"),
        },
        SCOPE_FILTER,
      ],
      empty: "No words yet. Add some above, for example: scam, free nitro",
      renderRow: (w) => {
        const canDelete = String(w.guild_id) !== "0" || isBotOwner();
        return h(
          "tr",
          { key: w.id },
          h(
            "td",
            { className: "list-entry-cell" },
            h("span", { className: "mono" }, w.word || w.pattern),
            w.valid === false
              ? h(
                  "span",
                  { className: "badge badge-soft badge-sm badge-error", title: w.error || "failed compile limits" },
                  "not enforced",
                )
              : null,
          ),
          h("td", {}, w.word ? "Word or phrase" : "Advanced regex"),
          h("td", {}, scopeName(w)),
          h("td", { className: "muted" }, w.note || "—"),
          h(
            "td",
            {},
            canEditSettings() && (canDelete
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
              : h("span", { className: "muted" }, "Owner only")),
          ),
        );
      },
    }),
    canEditSettings() && h(
      DraftCard,
      { group: "badwords-test" },
      h(
        "details",
        {
          className: "card bg-base-100 card-border list-card list-extra",
          // Open stays open after the remount each save makes; it resets on leaving the page.
          open: testerOpen,
          onToggle: (event) => {
            const open = event.currentTarget.open;
            if (open !== Boolean(ui.getSnapshot().draftBaselines["badwords:tester-open"]))
              ui.set({ draftBaselines: { ...ui.getSnapshot().draftBaselines, "badwords:tester-open": open } });
          },
        },
        h("summary", null, "Live tester"),
        h(
          "div",
          { className: "card-body" },
          h("p", { className: "field-help" }, "Checks text against your list and the draft above. It doesn't take any action."),
          h(
            ListFormSubmit,
            null,
            h(Input, {
              ref: sampleRef,
              draftKey: "badwords:sample",
              type: "text",
              "aria-label": "Sample text",
              placeholder: "sample text to test, e.g. 5c@m 🅿🆁🅾🅼🅾",
              className: "input",
            }),
            h(
              ActionButton,
              {
                className: "btn",
                onClick: async () => {
                  const sample = sampleRef.current.value;
                  if (!sample) return toast("Enter sample text", "error");
                  const draft = patternRef.current.value.trim();
                  const res = await api(`/api/guilds/${state.guildId}/testbadwords`, "POST", {
                    sample,
                    ...(draft ? { [mode === "regex" ? "pattern" : "words"]: draft } : {}),
                  });
                  setResult({ sample, ...res });
                },
              },
              "Test",
            ),
          ),
          result ? h(BadwordTestResult, { result }) : null,
        ),
      ),
    ),
  );
}

// Shown only after a test: the tested text, then each entry that matched it.
function BadwordTestResult({ result }) {
  return h(
    "div",
    { className: "tester-result", role: "status" },
    h(
      "p",
      { className: "tester-result-title" },
      result.matched ? `“${result.sample}” matched:` : `“${result.sample}” didn't match the saved list or draft.`,
    ),
    result.matched
      ? h(
          "ul",
          { className: "tester-matches" },
          ...result.matches.map((match, index) =>
            h(
              "li",
              { key: index },
              h("span", { className: "mono" }, match.label),
              h("span", { className: "badge badge-soft badge-sm badge-neutral" }, BADWORD_MATCH_SOURCES[match.source] || match.source),
            ),
          ),
        )
      : null,
    result.invalid_patterns
      ? h("p", { className: "field-help" }, `Skipped ${result.invalid_patterns} invalid saved patterns.`)
      : null,
  );
}

function CommandEnabledCell({ record, canManage }) {
  const [pending, setPending] = useState(null);
  const [error, setError] = useState("");
  const enabled = record.effective.enabled;
  if (!canManage) {
    return h("td", {},
      h("span", { className: `badge badge-soft badge-sm ${enabled ? "badge-success" : "badge-error"}` },
        enabled ? "enabled" : "disabled"),
    );
  }
  const guildId = state.guildId;
  const scope = record.builtin || record.guild_override ? "guild" : "global";
  const restriction = record.builtin && guildId === "0" ? "Built-ins are changed one server at a time."
    : scope === "global" && !isBotOwner() ? "Only the bot owner can change a global command."
    : undefined;
  return h("td", {},
    h(Input, {
      type: "checkbox",
      role: "switch",
      className: "toggle toggle-sm",
      "aria-label": `Enable /${record.name}`,
      "data-no-draft": true,
      title: restriction,
      checked: pending ?? enabled,
      disabled: pending !== null || Boolean(restriction),
      onChange: (event) => {
        const enabled = event.target.checked;
        return handleEvent(async () => {
          setPending(enabled);
          setError("");
          try {
            await reloadAfter(() => api(
              `/api/guilds/${guildId}/commands/${encodeURIComponent(record.name)}`,
              "PUT",
              { scope, enabled },
            ));
          } catch (error) {
            setError(error.message || String(error));
          } finally {
            setPending(null);
          }
        }, event);
      },
    }),
    error && h("p", { className: "err", role: "alert" }, error),
  );
}

function tabCommands() {
  const cmds = state.config.commands;
  const canManage = canManageServer() && !isPreview();
  return h(
    "div",
    {},
    h(
      "div",
      { className: "row title-row" },
      h("h2", {}, "Commands"),
      canManage &&
        h(
          ActionButton,
          { className: "btn btn-primary", onClick: () => editCommand(null) },
          "+ New custom command",
        ),
    ),
    !canManage &&
      h("p", { className: "muted" }, "Editing commands needs Manage Server: command plans can grant roles and run moderation actions."),
    h("p", { className: "muted" }, "Change any command's replies, who can use it, and its cooldown. Built-ins keep their name and behavior."),
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
          h(CommandEnabledCell, { record: c, canManage }),
          h(
            "td",
            {},
            canManage && h(IconButton, { action: "edit", label: "Edit", onClick: () => editCommand(c) }),
          ),
        );
      }),
    ),
  );
}

// What each command behavior does and the settings it takes (#623), keyed by the
// saved `behavior.kind` (src/types.rs Behavior). `does` starts the live summary;
// each field edits one of the behavior's own params, so no JSON is needed.
const COMMAND_REASON = { key: "default_reason", type: "text", label: "Default reason", hint: "Used when the moderator gives none." };
// The saved action runs only when the command gets no arguments at all (run_rule,
// run_template and run_logchannel in crates/bot), and every other subcommand needs
// a rule or event argument, so `list` is the only default that can run. A saved
// value outside `options` is still shown and kept: empty shows as Not set, any
// other value is marked with `unlisted`.
const COMMAND_SUBCOMMAND = {
  key: "action",
  type: "choice",
  label: "Default subcommand",
  hint: "Runs when the command is used with no arguments. Other subcommands must be typed.",
  options: ["list"],
  unlisted: "fails without arguments",
};
const COMMAND_BEHAVIORS = {
  ban: { does: "bans the target", fields: [COMMAND_REASON, { key: "delete_days", type: "days", label: "Delete messages" }] },
  softban: { does: "bans the target, deletes their last day of messages, then unbans", fields: [COMMAND_REASON] },
  unban: { does: "unbans the target", fields: [COMMAND_REASON] },
  kick: { does: "kicks the target", fields: [COMMAND_REASON] },
  warn: { does: "warns the target", fields: [COMMAND_REASON] },
  timeout: {
    does: "times out the target",
    fields: [{ key: "default_minutes", type: "number", label: "Default length", unit: "min", min: 0, max: 40320, hint: "Used when no minutes are given. 0 removes a timeout." }],
  },
  purge: { does: "deletes recent messages", fields: [{ key: "default_count", type: "number", label: "Default count", unit: "messages", min: 1, max: 100 }] },
  slowmode: { does: "sets this channel's slowmode", fields: [{ key: "default_seconds", type: "number", label: "Default slowmode", unit: "s", min: 0, max: 21600 }] },
  rule: { does: "manages Automod rules", fields: [COMMAND_SUBCOMMAND] },
  template: { does: "manages log templates", fields: [COMMAND_SUBCOMMAND] },
  logchannel: { does: "manages log channels", fields: [COMMAND_SUBCOMMAND] },
  warns: { does: "lists the target's warnings" },
  warn_clear: { does: "clears the target's warnings" },
  user_info: { does: "shows a member's profile, warnings and recent cases" },
  banlist: { does: "lists recent bans" },
  voice: { does: "reports which voice channel a user is in" },
  say: { does: "posts a message as the bot" },
  setname: { does: "sets the bot's nickname" },
  set_prefix: { does: "changes the command prefix" },
  help: { does: "lists commands" },
  about: { does: "shows bot info" },
  stats: { does: "shows server counts, release state and container use" },
  testregex: { does: "tests a regex against a sample" },
};

const COMMAND_PERMISSIONS = { everyone: "Everyone", mod: "Mods and up", admin: "Admins and up", owner: "Bot owner only" };

// Action plan steps (PlanStep): the label in the step picker and the summary phrase.
const PLAN_STEP_LABELS = {
  reply: "Reply",
  dm: "Send DM",
  log: "Log",
  add_role: "Add role",
  remove_role: "Remove role",
  timeout: "Timeout",
  kick: "Kick",
  ban: "Ban",
  delete_command_message: "Delete command message",
  react: "React",
  wait: "Wait",
};
const STEP_TIMEOUT_MAX = 40320;
// Common reactions offered first; "Other…" takes any Unicode emoji (the bot reacts with
// Unicode only). None are from the bot's log emoji map, which the WebUI never copies.
const REACT_EMOJI = ["👍", "👋", "🎉", "❤️", "👀", "🙏", "😂", "🔥", "💯", "👏"];

function wholeIn(value, min, max) {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= min && value <= max;
}

function daysText(days) {
  return `${days} day${days === 1 ? "" : "s"}`;
}

function deleteDaysLabel(days) {
  return days ? `Delete ${daysText(days)}` : "Keep messages";
}

function logEventLabel(event) {
  return String(event).replaceAll("_", " ").replace(/^./, (c) => c.toUpperCase());
}

// The log events a command can post: every event the Log channels page routes.
function commandLogEvents() {
  const cfg = state.config || {};
  const routed = (cfg.log_channels || []).map((lc) => lc.event).filter(Boolean);
  return routed.length ? routed : cfg.event_types || [];
}

function logEventOptions(current, withNone) {
  const events = commandLogEvents();
  return [
    withNone ? h("option", { key: "", value: "" }, "None") : current ? null : h("option", { key: "", value: "", disabled: true }, "Choose an event…"),
    current && !events.includes(current) ? h("option", { key: current, value: current }, `${current} (not a log event)`) : null,
    ...events.map((event) => h("option", { key: event, value: event }, logEventLabel(event))),
  ];
}

function joinPhrases(parts) {
  return parts.length > 1 ? `${parts.slice(0, -1).join(", ")} and ${parts.at(-1)}` : parts[0] || "";
}

function quoted(text) {
  const value = String(text ?? "").trim();
  return value ? `“${value}”` : "…";
}

function countShown(value, unit = "") {
  return Number.isSafeInteger(value) ? `${value}${unit}` : "…";
}

function roleShown(id, names) {
  if (!id) return "a role (none yet)";
  const role = names.roles.find((r) => r.id === String(id));
  return role ? `@${role.name}` : `role ${id}`;
}

function planStepPhrase(step, names) {
  switch (step.type) {
    case "reply": return `replies ${quoted(step.template)}`;
    case "dm": return `DMs ${quoted(step.template)}`;
    case "log": return `logs ${step.event ? logEventLabel(step.event) : "an event"}`;
    case "add_role": return `gives ${roleShown(step.role_id, names)}`;
    case "remove_role": return `removes ${roleShown(step.role_id, names)}`;
    case "timeout": return `times out the target for ${countShown(step.minutes, " min")}`;
    case "kick": return "kicks the target";
    case "ban": return step.delete_days ? `bans the target (deleting ${daysText(step.delete_days)} of messages)` : "bans the target";
    case "delete_command_message": return "deletes the command message";
    case "react": return `reacts ${String(step.emoji || "").trim() || "…"}`;
    case "wait": return `waits ${countShown(step.secs, "s")}`;
    default: return `runs ${step.type || "an unknown step"}`;
  }
}

function commandDoesParts(data, names) {
  const behavior = data.behavior || {};
  const params = behavior.params || {};
  if (behavior.kind === "plan") return (params.steps || []).map((step) => planStepPhrase(step, names));
  const spec = COMMAND_BEHAVIORS[behavior.kind];
  if (!spec) return [`runs the ${behavior.kind || "unknown"} behavior`];
  if (behavior.kind === "ban" && params.delete_days) return [spec.does, `deletes ${daysText(params.delete_days)} of messages`];
  const number = (spec.fields || []).find((f) => f.type === "number");
  if (number) return [`${spec.does} (default ${countShown(params[number.key], number.unit === "s" ? "s" : ` ${number.unit}`)})`];
  const choice = spec.fields?.[0];
  if (choice?.type === "choice") {
    const value = params[choice.key];
    const stale = value && !choice.options.includes(value) ? `, which ${choice.unlisted}` : "";
    return [`${spec.does} (default: ${value || "…"}${stale})`];
  }
  return [spec.does];
}

// One sentence for the whole command, e.g. "!ban <user> [reason]: bans the target, deletes
// 1 day of messages and replies “Banned {target}”. Mods and up, 5s cooldown."
function commandSummaryText(data, names) {
  const prefix = state.config?.settings?.prefix || "!";
  const call = String(data.usage || "").trim() || `${prefix}${String(data.name || "").trim().toLowerCase() || "…"}`;
  const parts = commandDoesParts(data, names);
  if (String(data.response || "").trim()) parts.push(`replies ${quoted(data.response)}`);
  const who = [COMMAND_PERMISSIONS[data.permission] || data.permission || "Everyone"];
  if (data.roles.length) who.push(`with ${data.roles.length === 1 ? "1 chosen role" : `one of ${data.roles.length} roles`}`);
  if (data.channels.length) who.push(`in ${data.channels.length === 1 ? "1 channel" : `${data.channels.length} channels`}`);
  const cooldown = Number(data.cooldown_secs);
  if (cooldown > 0) who.push(`${countShown(cooldown, "s")} cooldown`);
  const logs = data.log_event ? ` Logs ${logEventLabel(data.log_event)}.` : "";
  const off = data.enabled ? "" : " Off: the bot replies that it is disabled.";
  return `${call}: ${parts.length ? joinPhrases(parts) : "does nothing yet"}. ${who.join(", ")}.${logs}${off}`;
}

// The first thing Save must refuse, or null. Numbers are checked here so a typo
// stops the save with a message instead of being clamped or sent as-is.
function commandFormError(data) {
  if (!wholeIn(data.cooldown_secs, 0, Number.MAX_SAFE_INTEGER)) return "Cooldown must be a whole number of seconds.";
  const behavior = data.behavior || {};
  for (const f of COMMAND_BEHAVIORS[behavior.kind]?.fields || []) {
    if (f.type === "number" && !wholeIn(behavior.params?.[f.key], f.min, f.max))
      return `${f.label} must be a whole number from ${f.min} to ${f.max}.`;
  }
  if (behavior.kind !== "plan") return null;
  for (const [i, step] of (behavior.params?.steps || []).entries()) {
    const n = `Step ${i + 1}`;
    if ((step.type === "add_role" || step.type === "remove_role") && !step.role_id) return `${n}: choose a role.`;
    if (step.type === "timeout" && !wholeIn(step.minutes, 1, STEP_TIMEOUT_MAX)) return `${n}: timeout must be 1 to ${STEP_TIMEOUT_MAX} minutes.`;
    if (step.type === "wait" && !wholeIn(step.secs, 1, Number.MAX_SAFE_INTEGER)) return `${n}: wait must be a whole number of seconds, at least 1.`;
    if (step.type === "react" && !String(step.emoji || "").trim()) return `${n}: choose an emoji.`;
    if (step.type === "log" && !step.event) return `${n}: choose a log event.`;
  }
  return null;
}

// The fields a step of this type saves. Shared values carry over when the type changes.
function planStepFor(type, prev) {
  const template = typeof prev.template === "string" ? prev.template : "";
  switch (type) {
    case "reply":
    case "dm": return { type, template };
    case "log": return { type, event: prev.event || "plan_executed", template };
    case "add_role":
    case "remove_role": return { type, role_id: prev.role_id || "" };
    case "timeout": return { type, minutes: wholeIn(prev.minutes, 1, STEP_TIMEOUT_MAX) ? prev.minutes : 60 };
    case "ban": return { type, delete_days: prev.delete_days || 0 };
    case "react": return { type, emoji: prev.emoji || REACT_EMOJI[0] };
    case "wait": return { type, secs: wholeIn(prev.secs, 1, Number.MAX_SAFE_INTEGER) ? prev.secs : 5 };
    default: return { type };
  }
}

function numberValue(raw) {
  const text = String(raw ?? "").trim();
  return text === "" ? NaN : Number(text);
}

function DeleteDaysSelect({ label, value, onChange }) {
  return h(
    Field,
    { className: "trigger-field" },
    h(Label, { className: "sr-only" }, label),
    h(
      Select,
      { className: "select select-sm", defaultValue: String(value || 0), onChange: (e) => onChange(Number(e.target.value)) },
      // A saved count outside Discord's 0-7 (from the old JSON box) stays visible as it is.
      [...new Set([0, 1, 2, 3, 4, 5, 6, 7, Number(value) || 0])].map((days) => h("option", { key: days, value: String(days) }, deleteDaysLabel(days))),
    ),
  );
}

function NumberField({ label, value, unit, min, max, onChange, hidden }) {
  return h(
    Field,
    { className: "trigger-field" },
    h(Label, { className: hidden ? "sr-only" : "escalation-label" }, label),
    h(Input, {
      type: "number",
      className: "input input-num",
      min,
      max,
      step: 1,
      defaultValue: Number.isFinite(value) ? value : "",
      onInput: (e) => onChange(numberValue(e.target.value)),
    }),
    unit ? h("span", { "aria-hidden": "true" }, unit) : null,
  );
}

// One behavior param as its own input (#623).
function CommandBehaviorField({ spec, params, onChange }) {
  const set = (value) => {
    params[spec.key] = value;
    markDirty();
    onChange();
  };
  const help = spec.hint ? h("p", { className: "field-help" }, spec.hint) : null;
  if (spec.type === "number")
    return h("div", { className: "field" }, h(NumberField, { label: spec.label, value: params[spec.key], unit: spec.unit, min: spec.min, max: spec.max, onChange: set }), help);
  if (spec.type === "days")
    return h(
      "div",
      { className: "field" },
      h("span", { className: "escalation-label" }, spec.label),
      " ",
      h(DeleteDaysSelect, { label: spec.label, value: params[spec.key], onChange: set }),
    );
  if (spec.type === "choice") {
    const current = String(params[spec.key] ?? "");
    const options = spec.options.includes(current) ? spec.options : [current, ...spec.options];
    const text = (value) => (value === "" ? "Not set" : spec.options.includes(value) ? value : `${value} (${spec.unlisted})`);
    return field(
      spec.label,
      h(Select, { className: "select select-sm", defaultValue: current, onChange: (e) => set(e.target.value) }, options.map((value) => h("option", { key: value, value }, text(value)))),
      help,
    );
  }
  return field(
    spec.label,
    h(Input, { type: "text", className: "input input-sm", defaultValue: params[spec.key] ?? "", onInput: (e) => set(e.target.value) }),
    help,
  );
}

function CommandEditor({ record: c }) {
  const isNew = c === null;
  const [data] = useState(() => {
    const draft = isNew
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
      : JSON.parse(JSON.stringify(c.effective));
    draft.roles = Array.isArray(draft.roles) ? draft.roles.map(String) : [];
    draft.channels = Array.isArray(draft.channels) ? draft.channels.map(String) : [];
    draft.cooldown_secs = Number(draft.cooldown_secs) || 0;
    draft.behavior = draft.behavior && typeof draft.behavior === "object" ? draft.behavior : { kind: "plan", params: { steps: [] } };
    if (draft.behavior.kind === "plan" || COMMAND_BEHAVIORS[draft.behavior.kind]?.fields) {
      const params = draft.behavior.params;
      draft.behavior.params = params && typeof params === "object" && !Array.isArray(params) ? params : {};
    }
    if (draft.behavior.kind === "plan" && !Array.isArray(draft.behavior.params.steps)) draft.behavior.params.steps = [];
    return draft;
  });
  const [aliasText] = useState(() => {
    // Precedence can hide saved aliases temporarily. Edit that saved setting,
    // keeping the rest of the draft normalized; global built-in rows are seeds.
    const saved = c?.guild_override || (!c?.builtin && c?.global);
    return { value: (saved?.aliases ?? data.aliases ?? []).join(", ") };
  });
  const [, bump] = useState(0);
  const refresh = () => bump((n) => n + 1);
  const [names, rememberUser] = useServerNames([]);
  const pending = useRef(new Set()).current;
  const scopeSelRef = useRef(null);
  // Built-ins take per-server overrides only (#623): their name and behavior kind stay
  // shipped, and their permission never drops below the shipped level.
  const builtin = Boolean(c?.builtin);
  const fixed = builtin && state.guildId === "0";
  const scopeSel = scopeSelect(scopeSelRef, undefined, undefined, !builtin);
  const kind = data.behavior.kind;
  const isPlan = kind === "plan";
  const spec = COMMAND_BEHAVIORS[kind];
  const steps = isPlan ? data.behavior.params.steps : [];
  const text = (key, props = {}) =>
    h(Input, {
      type: "text",
      className: "input input-sm",
      defaultValue: data[key] ?? "",
      onInput: (e) => {
        data[key] = e.target.value;
        refresh();
      },
      ...props,
    });
  const structural = () => {
    markDirty();
    refresh();
  };
  const levels = Object.keys(COMMAND_PERMISSIONS);
  const floor = builtin ? Math.max(0, levels.indexOf(c.min_permission)) : 0;
  const allowed = levels.slice(floor);
  const permissions = allowed.includes(data.permission) ? allowed : [data.permission, ...allowed];

  const doesSection = isPlan
    ? h(
        React.Fragment,
        null,
        h("p", { className: "field-help" }, "Steps run in order. Kick, ban and timeout need a target: !name @user."),
        ...steps.map((step, i) =>
          h(CommandStep, {
            key: itemKey(step),
            step,
            index: i,
            names,
            pending,
            onChange: refresh,
            remove: () => {
              steps.splice(i, 1);
              structural();
            },
          }),
        ),
        h(
          ActionButton,
          {
            className: "btn btn-sm",
            onClick: () => {
              steps.push({ type: "reply", template: "" });
              structural();
            },
          },
          "+ add step",
        ),
      )
    : spec?.fields
      ? h("div", { className: "editor-columns" }, spec.fields.map((f) => h(CommandBehaviorField, { key: f.key, spec: f, params: data.behavior.params, onChange: refresh })))
      : h("p", { className: "field-help" }, spec ? "This built-in has no settings." : "This behavior has no form fields. Its saved settings are kept.");

  return h(
    DialogPanel,
    { className: "modal-box rule-editor command-editor", onInput: trackDraft },
    h(
      "div",
      { className: "editor-heading rule-editor-header" },
      h(
        "div",
        { className: "rule-editor-title" },
        h("span", { className: "eyebrow" }, builtin ? "BUILT-IN COMMAND" : isPlan ? "CUSTOM COMMAND" : "COMMAND"),
        h("h3", { className: "card-title" }, isNew ? "New custom command" : `Edit command: ${c.name}`),
      ),
      h(
        "div",
        { className: "rule-toggles" },
        h(
          "label",
          { className: "toggle-label" },
          h(Input, {
            type: "checkbox",
            className: "toggle toggle-sm",
            defaultChecked: data.enabled,
            onChange: (e) => {
              data.enabled = e.target.checked;
              refresh();
            },
          }),
          "Enabled",
        ),
      ),
    ),
    h(RuleSummary, { text: commandSummaryText(data, names) }),
    fixed ? h("p", { className: "field-help" }, "Built-ins are changed one server at a time. Pick a server to edit this one.") : null,
    h(
      EditorSection,
      { title: "Basics", hint: builtin && !fixed ? "Changes apply to this server only. Reset to default undoes them." : null },
      h(
        "div",
        { className: "editor-columns" },
        field(
          "Name",
          text("name", { placeholder: "command name", readOnly: builtin }),
          h("p", { className: "field-help" }, builtin ? "Built-in names stay fixed." : "Lowercase letters, digits, - and _."),
        ),
        field(
          "Aliases",
          h(Input, {
            type: "text",
            className: "input input-sm",
            defaultValue: aliasText.value,
            placeholder: "e.g. b, yeet",
            onInput: (e) => (aliasText.value = e.target.value),
          }),
          h("p", { className: "field-help" }, "Other names, separated by commas. Conflicting names may be inactive."),
        ),
        field("Description", text("description", { maxLength: 100 })),
        field("Usage", text("usage", { placeholder: "e.g. !name <user> [reason]" })),
      ),
    ),
    h(
      EditorSection,
      { title: "What it does" },
      doesSection,
      field(
        "Chat reply",
        h(Textarea, {
          className: "textarea textarea-sm",
          rows: 2,
          defaultValue: data.response || "",
          placeholder: isPlan ? "Optional reply after the steps" : "Leave empty for generated text or “done”.",
          onInput: (e) => {
            data.response = e.target.value;
            refresh();
          },
        }),
        h(
          "p",
          { className: "field-help" },
          "Sent after it runs. ",
          h(InfoTip, { id: "command-reply-help", label: "Reply variables" }, [
            "{target} {reason}: who it acted on and why.",
            "{user} {actor} {guild} {channel}: who ran it and where.",
            "{minutes} {duration} {count} {seconds}: the amounts used.",
            "{response}: generated reply text, if any. {emoji}: its map emoji.",
          ]),
        ),
      ),
    ),
    h(
      EditorSection,
      { title: "Who can use it and where", hint: ruleNamesHint(names) || "Leave roles and channels empty to allow all." },
      h(
        "div",
        { className: "editor-columns" },
        field(
          "Permission",
          h(
            Select,
            {
              className: "select select-sm",
              defaultValue: data.permission,
              onChange: (e) => {
                data.permission = e.target.value;
                refresh();
              },
            },
            permissions.map((p) => h("option", { key: p, value: p }, COMMAND_PERMISSIONS[p] || p)),
          ),
        ),
        h(
          "div",
          { className: "field" },
          h(NumberField, {
            label: "Cooldown",
            value: data.cooldown_secs,
            unit: "s",
            min: 0,
            onChange: (value) => {
              data.cooldown_secs = value;
              refresh();
            },
          }),
        ),
        h(IdPicker, { kind: "role", label: "Only these roles", ids: data.roles, names, rememberUser, onChange: refresh, pending }),
        h(IdPicker, { kind: "channel", label: "Only in these channels", ids: data.channels, names, onChange: refresh, pending }),
      ),
    ),
    h(
      EditorSection,
      { title: "Logging" },
      field(
        "Log event",
        h(
          Select,
          {
            className: "select select-sm",
            defaultValue: data.log_event || "",
            onChange: (e) => {
              data.log_event = e.target.value;
              refresh();
            },
          },
          logEventOptions(data.log_event || "", true),
        ),
        h("p", { className: "field-help" }, "Posted to that event's log channel after a successful run."),
      ),
      isBotOwner()
        ? h(
            "details",
            { className: "editor-advanced" },
            h("summary", null, "Advanced (raw)"),
            h("pre", { className: "mono raw-behavior", "aria-label": "Behavior as edited in this form (read-only)" }, JSON.stringify(data.behavior, null, 2)),
          )
        : null,
    ),
    h(
      "div",
      { className: "modal-action" },
      field("Applies to", scopeSel),
      h(
        ActionButton,
        {
          className: "btn btn-primary",
          hidden: fixed,
          onClick: async () => {
            data.name = String(data.name || "").trim().toLowerCase();
            if (!data.name) return toast("Name required", "error");
            // Typed role and channel IDs land in the data before it is checked.
            if (!flushPendingIds(pending)) return;
            const problem = commandFormError(data);
            if (problem) return toast(problem, "error");
            data.aliases = aliasText.value
              .split(",")
              .map((s) => s.trim())
              .filter(Boolean);
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
          // A built-in has nothing to delete until this server overrides it.
          hidden: isNew || (builtin && (fixed || !c.guild_override)),
          onClick: async () => {
            if (!confirm(builtin ? `Reset ${c.name} to its default?` : `Delete command ${c.name}?`)) return;
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
                toast(builtin ? "Reset to default" : c.guild_override ? "Override cleared" : "Deleted", "success");
                closeOverlay();
              },
            );
          },
        },
        builtin ? "Reset to default" : "Delete",
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
  ["cleanup", "Automod cleanup"],
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
  ["cleanup_action", "cleanup", "The enforcement outcome and case link, when available", "Banned · Open case"],
  ["cleanup_total", "cleanup", "The number of messages deleted", "6"],
  ["cleanup_channels", "cleanup", "How many messages were deleted from each channel", "#general 4, #media 2"],
  ["cleanup_first", "cleanup", "The earliest delete observation or failure, in UTC", "2026-11-09 22:35:49 UTC"],
  ["cleanup_last", "cleanup", "The latest delete observation or failure, in UTC", "2026-11-09 22:35:54 UTC"],
  ["cleanup_sample", "cleanup", "One short message sample, when available", "Repeated image caption"],
  ["cleanup_evidence", "cleanup", "The matched evidence hash, when available", "SHA-256 abcd…"],
  ["cleanup_failures", "cleanup", "The failure count and reasons by channel", "2 · missing permission (#media 2)"],
  ["cleanup_source_channels", "cleanup", "Comma-separated IDs of the source channels", "42,84"],
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
const CLEANUP_VARIABLE_NAMES = TEMPLATE_VARIABLES.filter(v => v.group === "cleanup").map(v => v.name);

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
  automod_delete: `mention user_id channel message_id message_url content attachments coverage reason rule rule_id ${CLEANUP_VARIABLE_NAMES.join(" ")}`,
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
    channel: "Only filled in by the exact-image and repeated-text rules. Other rules leave it as typed.",
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
  const snapshot = ui.getSnapshot();
  const current = textarea.value;
  const start = textarea.selectionStart ?? current.length;
  const end = textarea.selectionEnd ?? start;
  const token = `{${name}}`;
  const value = current.slice(0, start) + token + current.slice(end);
  const baseline = draftBaseline(snapshot, draftKey, defaultValue, true);
  const next = { ...snapshot.drafts };
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

function TemplatePreview({ record: t }) {
  const template = useStore(ui, ({ drafts }) => drafts[`template:${t.event}:text`]?.value ?? t.effective);
  const keys = [...template.matchAll(/[^}]*}/g)].flatMap(([chunk]) => {
    const start = chunk.lastIndexOf("{");
    return start === -1 ? [] : [chunk.slice(start + 1, -1)];
  });
  const cleanupVariables = t.cleanup_variables ?? [];
  const groupedCleanup = keys.some(key => cleanupVariables.includes(key));
  // {emoji} stays with the preview's own emoji rule.
  const samples = Object.fromEntries(TEMPLATE_VARIABLES
    .filter(v => v.name !== "emoji" && (groupedCleanup || v.group !== "cleanup"))
    .map(v => [v.name, v.example]));
  samples.action = t.event === "role_change" ? "updated" : "ban";
  samples.notice = t.event === "thread_create" ? "created" : "Prior moderation";
  if (groupedCleanup) {
    const supplied = new Set(["mention", "user_id", "rule", "rule_id", "guild", "time", "event", "emoji", ...cleanupVariables]);
    samples.event = t.event;
    for (const name of keys) {
      if (!supplied.has(name)) samples[name] = "unavailable for grouped cleanup";
    }
  }
  const preview = templateEmbedPreview(t, template, samples);
  return h(
    "div",
    { className: "template-preview" },
    h("strong", {}, groupedCleanup ? "Grouped cleanup preview" : "Preview"),
    groupedCleanup && h("p", { className: "muted" }, "Individual notices use the built-in layout."),
    h(LogEmbedPreview, { embed: preview }),
  );
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
        canEditSettings() && h(
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
        h(TemplatePreview, { record: t }),
      ),
      canEditSettings() && h(
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
  if (!canEditSettings()) return h("div", { className: "card bg-base-100 card-border log-channel-card" },
    h("div", { className: "card-body" },
      eventHeading(lc.event),
      h("p", null, `Destination channel: ${input.props.defaultValue || "Not set"}`),
      h("p", null, `Ignored users or bots: ${(lc.ignore?.users || []).join(", ") || "None"}`),
      h("p", null, `Ignored source channels: ${(lc.ignore?.channels || []).join(", ") || "None"}`),
    ),
  );
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
  if (!job || isPreview()) return false;
  const kind = job.action?.kind;
  const unstarted = job.state === "pending" || job.state === "claimed";
  switch (kind) {
    case "set_log_channel":
      return canEditSettings() && (unstarted || job.state === "running" || job.state === "uncertain");
    case "member_sync":
      return canManageServer() && unstarted;
    case "ban":
    case "unban":
      return canBanMembers() && unstarted;
    default:
      return false;
  }
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
    : reason
      ? `The bot could not save a member or ban change${when} (${reason})`
      : `The bot recorded a coverage gap${when}`;
}

function memberGapWhen(gapAt) {
  return gapAt ? ` at ${memberTime(gapAt)}` : "";
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

function memberPageLabel(number, offset, rows, total) {
  if (!rows) return number === 1 ? "No results" : `Page ${number} · no results`;
  const end = (offset + rows).toLocaleString();
  return `Page ${number} · ${(offset + 1).toLocaleString()}–${end}${Number.isInteger(total) ? ` of ${total.toLocaleString()}` : ""}`;
}

const memberSyncStatusLabels = {
  queued: "Queued",
  running: "Running",
  waiting: "Waiting",
  paused: "Paused",
  needs_attention: "Needs attention",
  complete: "Complete",
};

function MemberImportStatus({ store }) {
  const { coverage, summary, notes, details, error } = useStore(store);
  return h(
    React.Fragment,
    null,
    coverage,
    h(
      "section",
      { "aria-label": "Member import status" },
      h("div", { className: "row" }, ...summary),
      // Fixed slots keep an opened Details disclosure open across status refreshes.
      h("div", null, ...notes),
      details,
      error,
    ),
  );
}

function tabMembers() {
  const guildId = state.guildId;
  const enrollmentId = state.config?.enrollment_id;
  const handlers = useRef({});
  // Only the import status subscribes to countdown and status refreshes.
  const [syncView] = useState(() => createState({
    coverage: null,
    summary: [h("p", { key: "loading" }, "Loading import status…")],
    notes: [],
    details: null,
    error: null,
  }));
  const [memberRows, setMemberRows] = useState({});
  const [progress, setProgress] = useState(null);
  const [detailParts, setDetailParts] = useState([]);
  const [pagination, setPagination] = useState({ first: true, next: true, label: "" });
  // A case file's "Member page" link opens this list searched to that member.
  const [memberHandoff] = useState(() => {
    const next = pendingMember?.guildId === guildId ? pendingMember : null;
    pendingMember = null;
    return next;
  });
  const controls = useListControls(
    MEMBERS_LIST,
    guildId,
    () => handlers.current.loadChangedFilters?.(),
    () => state.guildId === guildId && state.config?.enrollment_id === enrollmentId && state.tab === "members",
    () => handlers.current.invalidateResults?.(),
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
      const scheduled = ["queued", "running", "waiting"];
      const attempts = [];
      const importAttempt = latestStatus?.next_attempt_at;
      if (importAttempt != null || scheduled.includes(latestStatus?.status))
        attempts.push(Number(importAttempt) || 0);
      const auditAttempt = latestStatus?.audit_recovery_next_attempt_at;
      const auditStatus = latestStatus?.audit_recovery_status;
      // Stalled pagination keeps a checkpoint for manual Retry.
      if (
        auditStatus !== "complete" &&
        !auditPaginationStalled(latestStatus) &&
        (auditAttempt != null || scheduled.includes(auditStatus))
      )
        attempts.push(Number(auditAttempt) || 0);
      const reconcileAttempt = latestStatus?.automatic_reconcile_at;
      if (reconcileAttempt != null) attempts.push(Number(reconcileAttempt) || 0);
      if (statusRead !== "retry-needed" && !controlPending() && !attempts.length) {
        pollDueAt = 0;
        return;
      }
      if (!pollDueAt) {
        let delay = Math.min(10000 * 2 ** Math.min(pollAttempt, 5), 300000);
        // Worker deadlines pace successful reads when no control is pending.
        if (statusRead === "current" && attempts.length && !controlPending()) {
          const statusNow = Number(latestStatus?.as_of) || latestStatusReceivedAt;
          const elapsed = Date.now() - latestStatusReceivedAt;
          delay = Math.max(delay, Math.min(...attempts) - statusNow - elapsed);
        }
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
      const stalled = sync.coverage === "stalled";
      const statusLabel = stalled ? "Stalled" : memberSyncStatusLabels[sync.status || "needs_attention"] || String(sync.status);
      const statusTone = stalled ? "badge-warning" :
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
          stalled || workerUnconfirmed ||
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
      } else if (stalled) {
        notes.push(h("p", { className: "muted" }, "Next attempt: not scheduled"));
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
      const reconciling = sync.coverage === "reconciling";
      const auditRecovery = sync.gap_auto_clear && sync.status === "complete" &&
        (reconciling || sync.coverage == null) &&
        ["queued", "running", undefined].includes(sync.audit_recovery_status);
      const reconcileInNotice = sync.reconciliation_needed && !reconciling && !auditRecovery && !isPreview() && canManageServer();
      const reconcileButton = reconcileInNotice && syncControlButton("reconcile", "Reconcile from Discord");
      let coverage;
      if (sync.reconciliation_needed) {
        const when = memberGapWhen(sync.gap_at);
        const cause = memberGapCause(sync.reconciliation_reason, when);
        let recovery = "Select Reconcile from Discord to re-check the full member and ban lists. This can take a while on large servers.";
        if (sync.automatic_reconcile_at && !reconciling) {
          recovery = Number(sync.automatic_reconcile_at) > statusNow
            ? `Next automatic Reconcile: ${asDate(sync.automatic_reconcile_at)}. You can select Reconcile from Discord to start sooner.`
            : "The bot can queue an automatic Reconcile. You can select Reconcile from Discord to start sooner.";
        } else if (reconciling) {
          recovery = "The bot will re-check the member and ban lists using its normal request pace.";
        } else if (sync.coverage && !sync.initial_completed_at) {
          recovery = "The bot can reconcile this gap after the initial import finishes.";
        } else if (sync.coverage && !sync.run_id) {
          recovery = "The saved import job needs repair before Reconcile can run.";
        } else if (sync.run_kind === "reconcile" && sync.status === "needs_attention") {
          recovery = "Resolve the reported error and select Retry to resume Reconcile.";
        } else if (sync.run_kind === "reconcile" && sync.status === "paused") {
          recovery = "Select Resume to continue Reconcile.";
        } else if (sync.phase === "complete" && sync.local_history_complete === false) {
          recovery = "The bot is finishing local case history.";
        } else if (sync.status === "waiting" || sync.audit_recovery_status === "waiting" ||
                   (sync.gap_auto_clear && sync.audit_recovery_reason === "permission_denied")) {
          recovery = "The bot will retry at the next attempt shown below.";
        }
        coverage = auditRecovery
          ? h(
              "p",
              { id: "member-coverage-notice", className: "muted", role: "status" },
              `The bot restarted${when}. Ban and unban actions from the restart are being caught up automatically; no action is needed.`,
            )
          : h(
              "div",
              { id: "member-coverage-notice", className: reconciling ? "muted" : "alert alert-soft alert-warning alert-wrap", role: "status" },
              h(
                "div",
                {},
                h("strong", {}, reconciling ? "Member list reconciling" : "Member list may be out of date"),
                h("p", {}, `${cause}, so it may have missed some joins, leaves, or bans. ${recovery}`),
              ),
              reconcileButton && h("div", { className: "row" }, reconcileButton),
            );
      } else {
        coverage = h(
          "p",
          { id: "member-coverage-notice", className: "muted", role: "status" },
          complete
            ? "Member list: up to date"
            : sync.coverage === "not_started"
              ? "Member list: not imported"
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
      syncView.set({ coverage, summary, notes, details, error: null });
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
          setMemberRows({ fallback: h("tr", null, h("td", { colSpan: 5, className: "muted" }, emptyMembersText())) });
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
        else syncView.set({ coverage: null, summary: [], notes: [], details: null });
        syncView.set({
          error: h(
            "p",
            { className: "err", role: "status" },
            latestStatus
              ? "Could not refresh import status. Showing the last saved status; retrying shortly while visible."
              : "Could not load import status. Retrying shortly while visible.",
          ),
        });
        schedulePoll();
      } finally {
        if (request === statusRequest) statusAbort = null;
      }
    };
    const loadMembers = async () => {
      const request = ++listRequest;
      listAbort?.abort();
      const controller = new AbortController();
      listAbort = controller;
      emptyMembersCell = null;
      const target = requestedPage;
      const size = controls.pageSize();
      setProgress(listLoadProgress(size));
      setPagination({ first: true, next: true, label: "" });
      setMemberRows({ fallback: h("tr", {}, h("td", { colSpan: 5 }, "Loading members…")) });
      try {
        const q = controls.query();
        if (after) q.set("after", after);
        const page = await readListPage({
          read: apiRead, path: `/api/guilds/${guildId}/members`, query: q,
          collection: "members", size, signal: controller.signal, onProgress: setProgress,
        });
        if (!aliveHere() || request !== listRequest) return;
        nextAfter = page.next_cursor || null;
        shownPage = { ...target, rows: (page.members || []).length };
        setPagination({
          first: !after,
          next: !nextAfter,
          label: memberPageLabel(shownPage.number, shownPage.offset, shownPage.rows,
            page.total ?? (nextAfter ? null : shownPage.offset + shownPage.rows)),
        });
        const renderRow = (member) =>
          h(
            "tr",
            { key: member.user_id },
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
          );
        emptyMembersCell = page.members.length
          ? null
          : h("td", { colSpan: 5, className: "muted" }, emptyMembersText());
        setMemberRows(page.members.length
          ? { rows: page.members, renderRow }
          : { fallback: h("tr", {}, emptyMembersCell) });
      } catch (error) {
        if (!aliveHere() || request !== listRequest) return;
        setPagination({ first: !after, next: true, label: "" });
        setMemberRows({ fallback:
          h("tr", {}, h("td", { colSpan: 5, className: "err", role: "alert" }, "Could not load members. Refresh to retry.")),
        });
      } finally {
        if (aliveHere() && request === listRequest) setProgress(null);
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
      setProgress(null);
      setMemberRows({ fallback: h("tr", {}, h("td", { colSpan: 5 }, "Loading members…")) });
      setDetailParts([]);
    };
    handlers.current = {
      invalidateResults: () => {
        if (aliveHere()) invalidateResults();
      },
      loadChangedFilters: () => {
        if (!aliveHere()) return;
        invalidateResults();
        loadMembers();
      },
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
      cancel: () => {
        ++listRequest;
        listAbort?.abort();
        setProgress(null);
        setPagination({ first: !after, next: true, label: "" });
        setMemberRows({ fallback:
          h("tr", null, h("td", { colSpan: 5, role: "status" }, "Loading canceled. Refresh to try again.")),
        });
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
      listAbort?.abort();
      statusAbort?.abort();
      detailAbort?.abort();
      document.removeEventListener("visibilitychange", visibility);
    };
    authLossCleanups.add(dispose);
    loadStatus();
    if (memberHandoff) controls.replaceFilters({ q: memberHandoff.userId, status: "all" });
    else loadMembers();
    return dispose;
  }, [guildId, enrollmentId]);
  return h(
    "div",
    null,
    h("h2", null, "Members"),
    h(
      "p",
      { className: "muted" },
      "Search saved member identities and known join dates. Default order is newest join first.",
    ),
    h(MemberImportStatus, { store: syncView }),
    h(
      "section",
      { className: "card bg-base-100 card-border" },
      h(
        "div",
        { className: "card-body" },
        h(ListControls, { controls }),
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
        progress !== null ? h(ListLoadProgress, { loaded: progress, onCancel: () => handlers.current.cancel?.() }) : null,
        h(PagedTable, {
          headings: [
            ...MEMBERS_LIST.columns.map((column) => h(ListSortHeading, { key: column.key, column, controls })),
            "History",
          ],
          ...memberRows,
        }),
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

// Case file (#624): one label, badge color and icon per stored action and outcome.
const CASE_ACTIONS = {
  ban: { label: "Ban", badge: "badge-error", icon: "hammer" },
  kick: { label: "Kick", badge: "badge-error", icon: "log-out" },
  timeout: { label: "Timeout", badge: "badge-warning", icon: "clock" },
  warning: { label: "Warning", badge: "badge-warning", icon: "alert-triangle" },
  delete: { label: "Message delete", badge: "badge-info", icon: "message-x" },
  unban: { label: "Unban", badge: "badge-success", icon: "user-check" },
  timeout_removal: { label: "Timeout removed", badge: "badge-success", icon: "clock" },
};
const CASE_OUTCOMES = {
  succeeded: { label: "Succeeded", badge: "badge-success", icon: "check-circle", intent: "success", result: "Discord action succeeded" },
  failed: { label: "Failed", badge: "badge-error", icon: "x-circle", intent: "danger", result: "Discord action failed" },
  uncertain: { label: "Uncertain", badge: "badge-warning", icon: "alert-circle", intent: "warning", result: "Discord result uncertain" },
  attempted: { label: "Attempted", badge: "badge-info", icon: "clock", intent: "info", result: "Waiting for Discord's result" },
};
const CASE_SOURCES = {
  application: "Recorded by jModGuard when it acted",
  discord_audit: "From the Discord audit log",
  gateway_observation: "Seen in Discord; who did it was not reported",
};

function caseEntry(table, value) {
  return Object.hasOwn(table, value || "") ? table[value] : null;
}

function CaseBadge({ kind, value }) {
  const known = caseEntry(kind === "action" ? CASE_ACTIONS : CASE_OUTCOMES, value);
  return h(
    "span",
    { className: `badge badge-soft badge-sm case-badge${known ? ` ${known.badge}` : ""}` },
    known ? h(Icon, { name: known.icon }) : null,
    known ? known.label : String(value || "Unknown").replaceAll("_", " "),
  );
}

// Relative time, with the exact time on hover.
function CaseTime({ ms }) {
  const date = memberInstant(ms);
  if (!date) return h("span", { className: "muted" }, "Time unknown");
  return h("time", { dateTime: date.toISOString(), title: memberTime(ms) }, usageAgeMs(Number(ms)));
}

// Names come from the Members directory through the case API; an unknown user keeps their ID.
function casePerson(id, names) {
  const known = id && names && Object.hasOwn(names, id) ? names[id] : null;
  return { id: id || null, name: known?.display_name || null, handle: known?.username || null };
}

// No actor is not jModGuard: only the case source can say automod acted.
function casePersonText(id, names) {
  if (!id) return null;
  return casePerson(id, names).name || `user ${id}`;
}

function LetterAvatar({ name, large = false }) {
  return h(
    "span",
    { className: `case-avatar${large ? " case-avatar-lg" : ""}`, "aria-hidden": "true" },
    (Array.from(String(name || "?").trim())[0] || "?").toUpperCase(),
  );
}

function CasePersonCell({ id, names, empty }) {
  if (!id) return h("span", { className: "muted" }, empty);
  const person = casePerson(id, names);
  if (!person.name) return h("span", { className: "mono", title: id }, id);
  return h(
    "span",
    { className: "case-person", title: id },
    h(LetterAvatar, { name: person.name }),
    h("span", { className: "case-person-name" }, person.name),
    person.handle ? h("small", { className: "muted" }, `@${person.handle}`) : null,
  );
}

function caseSourceKind(source) {
  return source && typeof source === "object" ? Object.keys(source)[0] || null : null;
}

// A case jModGuard recorded itself with no moderator is automod; any other missing
// moderator is unknown. Lists pass the summary's source_kind, the case file its source.
function caseAutomatic(actorId, sourceKind) {
  return !actorId && sourceKind === "application";
}

let pendingMember = null;

// Leave the case dialog for another view, asking first about an unsent note.
function leaveCaseView(next) {
  if (!canLeave()) return;
  closeOverlay();
  next();
}

function CasePersonCard({ role, id, names, automatic, guildId, onFilter, filterKey }) {
  if (!id) {
    return h(
      "div",
      { className: "case-person-card" },
      h("span", { className: "case-avatar case-avatar-lg", "aria-hidden": "true" }, h(Icon, { name: automatic ? "bot" : "users" })),
      h(
        "div",
        { className: "case-person-body" },
        h("span", { className: "case-role" }, role),
        h("strong", {}, automatic ? "jModGuard automod" : "Not recorded"),
        h("span", { className: "muted" }, automatic ? "Acted automatically; no moderator involved." : "Discord didn't report who did this."),
      ),
    );
  }
  const person = casePerson(id, names);
  return h(
    "div",
    { className: "case-person-card" },
    h(LetterAvatar, { name: person.name || "?", large: true }),
    h(
      "div",
      { className: "case-person-body" },
      h("span", { className: "case-role" }, role),
      h("strong", { className: "case-person-name" }, person.name || "Unknown member"),
      person.handle ? h("span", { className: "muted" }, `@${person.handle}`) : null,
      h(
        "span",
        { className: "case-id-line" },
        h("code", {}, id),
        h(IconButton, { action: "copy", label: `Copy ${role.toLowerCase()} ID ${id}`, onClick: () => navigator.clipboard.writeText(id) }),
      ),
      h(
        "span",
        { className: "case-person-links" },
        h(
          ActionButton,
          {
            type: "button",
            className: "btn btn-sm btn-link",
            onClick: () =>
              leaveCaseView(() => {
                pendingMember = { guildId, userId: id };
                if (!navigate("members")) pendingMember = null;
              }),
          },
          "Member page",
        ),
        onFilter
          ? h(
              ActionButton,
              // Shows all of this member's cases: the link replaces any other list filters.
              { type: "button", className: "btn btn-sm btn-link", onClick: () => leaveCaseView(() => onFilter(filterKey, id, person.name)) },
              role === "Subject" ? "Their other cases" : "Their other actions",
            )
          : null,
      ),
    ),
  );
}

// Readable timeline entries; never the stored JSON.
function caseTimeline(c, history, names) {
  const action = caseEntry(CASE_ACTIONS, c.action);
  const outcome = caseEntry(CASE_OUTCOMES, c.outcome);
  const source = caseSourceKind(c.source);
  const items = [
    {
      key: "created",
      icon: action?.icon || "briefcase",
      intent: "info",
      title: `${action?.label || "Action"} recorded`,
      detail: c.actor_id
        ? `By ${casePersonText(c.actor_id, names)}`
        : caseAutomatic(c.actor_id, source) ? "By jModGuard automod" : "Moderator not recorded",
      meta: CASE_SOURCES[source] || null,
      at: c.occurred_at_ms,
    },
    {
      key: "result",
      icon: outcome?.icon || "alert-circle",
      intent: outcome?.intent || "neutral",
      title: outcome?.result || `Outcome: ${c.outcome || "unknown"}`,
    },
  ];
  for (const item of history) {
    const entry = item.entry || {};
    const body = entry.body || {};
    const who = casePersonText(entry.actor_id, names);
    const by = who ? ` by ${who}` : "";
    const base = { key: item.id || item.sequence, at: entry.recorded_at_ms };
    if (body.note) {
      items.push({ ...base, icon: "message-square", intent: "info", title: `Note added${by}`, note: body.note.text });
    } else if (body.correction) {
      const fix = body.correction;
      const reason = fix.reason === "clear" ? "Reason cleared." : fix.reason?.set != null ? `Reason changed to: ${fix.reason.set}` : null;
      const changed = fix.outcome ? `Outcome set to ${caseEntry(CASE_OUTCOMES, fix.outcome)?.label || fix.outcome}.` : null;
      items.push({ ...base, icon: "pencil", intent: "warning", title: `Record corrected${by}`, detail: [changed, reason].filter(Boolean).join(" ") || null, note: fix.explanation || null });
    } else if (body.policy) {
      items.push({ ...base, icon: "shield", intent: "success", title: "Rule matched", detail: body.policy.description || null, meta: `Rule ${body.policy.rule_id} · revision ${body.policy.revision}` });
    } else {
      items.push({ ...base, icon: "file-text", intent: "neutral", title: "History entry", detail: "This kind of entry can't be shown here yet." });
    }
  }
  return items;
}

function CaseTimeline({ items }) {
  return h(
    "ol",
    { className: "case-timeline" },
    items.map((item) =>
      h(
        "li",
        { key: item.key },
        h("span", { className: "case-timeline-icon" }, h(Icon, { name: item.icon, intent: item.intent })),
        h(
          "div",
          { className: "case-timeline-body" },
          h("div", { className: "case-timeline-head" }, h("strong", {}, item.title), item.at ? h(CaseTime, { ms: item.at }) : null),
          item.detail ? h("p", {}, item.detail) : null,
          item.note ? h("p", { className: "case-note" }, item.note) : null,
          item.meta ? h("p", { className: "muted" }, item.meta) : null,
        ),
      ),
    ),
  );
}

const IMAGE_SIGNATURES = [
  ["image/png", [0x89, 0x50, 0x4e, 0x47]],
  ["image/jpeg", [0xff, 0xd8, 0xff]],
  ["image/gif", [0x47, 0x49, 0x46, 0x38]],
];
// Evidence is served as opaque bytes; only known image signatures get a preview.
function sniffImageType(bytes) {
  for (const [type, signature] of IMAGE_SIGNATURES) if (signature.every((byte, i) => bytes[i] === byte)) return type;
  const text = (from, to) => String.fromCharCode(...bytes.slice(from, to));
  return text(0, 4) === "RIFF" && text(8, 12) === "WEBP" ? "image/webp" : null;
}
const EVIDENCE_PREVIEW_BYTES = 8 * 1024 * 1024;

function evidenceSize(bytes) {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value <= 0) return "";
  return value < 1024 * 1024 ? `${Math.max(1, Math.round(value / 1024))} KB` : `${(value / (1024 * 1024)).toFixed(1)} MB`;
}

function CaseEvidenceFile({ guildId, caseId, item }) {
  const [preview, setPreview] = useState(null);
  const path = `/api/guilds/${guildId}/cases/${caseId}/evidence/${item.id}`;
  useEffect(() => {
    if (!(Number(item.bytes) <= EVIDENCE_PREVIEW_BYTES) || typeof URL.createObjectURL !== "function") return undefined;
    let url = null;
    let live = true;
    fetchPrivate(path)
      .then(async (blob) => {
        const type = sniffImageType(new Uint8Array(await blob.slice(0, 12).arrayBuffer()));
        if (!live || !type) return;
        url = URL.createObjectURL(new Blob([blob], { type }));
        setPreview(url);
      })
      .catch(() => {});
    return () => {
      live = false;
      if (url) URL.revokeObjectURL(url);
    };
  }, [path]);
  return h(
    "figure",
    { className: "case-file" },
    preview
      ? h("img", { src: preview, alt: `Attachment ${item.attachment_id || item.id}` })
      : h("span", { className: "case-file-placeholder", "aria-hidden": "true" }, h(Icon, { name: "paperclip", intent: "neutral" })),
    h(
      "figcaption",
      { className: "case-file-meta" },
      h("span", {}, preview ? "Image" : "Attachment", item.bytes ? ` · ${evidenceSize(item.bytes)}` : ""),
      h(IconButton, { action: "download", label: `Download ${item.id}`, type: "button", onClick: () => downloadPrivate(path, item.id) }),
    ),
  );
}

function CaseEvidence({ guildId, caseId, texts, files, names, channels }) {
  const channelName = (id) => channels.find((channel) => channel.id === id)?.name;
  const where = (id) => (channelName(id) ? `#${channelName(id)}` : id ? `channel ${id}` : "");
  const byMessage = new Map();
  for (const file of files) byMessage.set(file.message_id, [...(byMessage.get(file.message_id) || []), file]);
  const fileList = (list) =>
    list.length ? h("div", { className: "case-files" }, list.map((item) => h(CaseEvidenceFile, { key: item.id, guildId, caseId, item }))) : null;
  const cards = texts.map((item, index) => {
    const author = casePerson(item.author_id, names);
    const attached = byMessage.get(item.message_id) || [];
    byMessage.delete(item.message_id);
    return h(
      "article",
      { className: "case-message", key: `text-${index}` },
      h(LetterAvatar, { name: author.name || "?", large: true }),
      h(
        "div",
        { className: "case-message-body" },
        h(
          "div",
          { className: "case-message-head" },
          h("strong", { title: item.author_id }, author.name || `User ${item.author_id}`),
          h("span", { className: "muted", title: item.channel_id }, `in ${where(item.channel_id)}`),
          h("small", { className: "muted" }, h(CaseTime, { ms: item.occurred_at_ms })),
        ),
        h("pre", { className: "case-message-text" }, item.text),
        fileList(attached),
      ),
    );
  });
  for (const [messageId, list] of byMessage) {
    cards.push(
      h(
        "article",
        { className: "case-message", key: `files-${messageId}` },
        h("span", { className: "case-avatar case-avatar-lg", "aria-hidden": "true" }, h(Icon, { name: "paperclip" })),
        h(
          "div",
          { className: "case-message-body" },
          h("div", { className: "case-message-head" }, h("strong", {}, "Saved attachments"), h("span", { className: "muted", title: list[0].channel_id }, `in ${where(list[0].channel_id)}`)),
          fileList(list),
        ),
      ),
    );
  }
  return cards.length ? cards : h("p", { className: "muted" }, "No private evidence was saved for this case.");
}

function openCaseView(props) {
  ui.set({
    editor: h(CaseView, { ...props, dialogLabel: `Case ${props.caseId.slice(0, 8)}`, key: `${props.guildId}:${props.caseId}:${Date.now()}` }),
  });
}

// The case-history contract admits at most this many entries per case, so one
// page of this size holds the whole history.
const CASE_HISTORY_MAX_ENTRIES = 64;

async function loadCaseHistory(base, signal) {
  const page = await api(`${base}/history?limit=${CASE_HISTORY_MAX_ENTRIES}&after=0`, "GET", null, signal);
  return { entries: page?.history || [], names: page?.names || {} };
}

// The case file dialog (#624). Opens at once and reads the case, its history,
// and its evidence together. Evidence channel names can arrive after rendering.
function CaseView({ guildId, caseId, onRemoved, onFilter }) {
  const base = `/api/guilds/${guildId}/cases/${caseId}`;
  const [attempt, setAttempt] = useState(0);
  const [view, setView] = useState({ status: "loading" });
  const noteRef = useRef(null);
  const note = useRef({ intent: { kind: "empty" }, activity: "idle" }).current;
  useEffect(() => {
    const controller = new AbortController();
    const enrollment = pageEnrollment(base, "POST");
    const canReadChannelNames = () =>
      state.guildId === guildId && pageEnrollment(base, "POST") === enrollment && tabAllowed("settings");
    let timedOut = false;
    let timer = setTimeout(() => {
      timedOut = true;
      controller.abort();
    }, 30000);
    setView({ status: "loading" });
    Promise.all([
      apiRead(base, controller.signal),
      loadCaseHistory(base, controller.signal),
      apiRead(`${base}/evidence`, controller.signal),
    ]).then(
      ([payload, history, evidence]) => {
        clearTimeout(timer);
        if (controller.signal.aborted) return;
        const c = payload?.case || {};
        const files = Array.isArray(evidence?.evidence) ? evidence.evidence : [];
        setView({
          status: "ready",
          c,
          canDelete: !!payload?.can_delete,
          names: { ...(payload?.names || {}), ...history.names },
          history: history.entries,
          files,
          channels: [],
        });
        const texts = Array.isArray(c.evidence?.text) ? c.evidence.text : [];
        if (!canReadChannelNames() || !(texts.some((item) => item.channel_id) || files.some((item) => item.channel_id))) return;
        timer = setTimeout(() => controller.abort(), 30000);
        return apiRead(`/api/guilds/${guildId}/server-names`, controller.signal).then(
          (data) => {
            if (controller.signal.aborted || !canReadChannelNames() || !Array.isArray(data?.channels)) return;
            setView((current) => ({ ...current, channels: data.channels }));
          },
          () => {},
        );
      },
      (error) => {
        if (controller.signal.aborted && !timedOut) return;
        setView({ status: "error", message: timedOut ? "Request timed out." : error?.message || String(error) });
      },
    ).finally(() => clearTimeout(timer));
    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [base, attempt]);
  const short = caseId.slice(0, 8);
  const close = () => {
    if (canLeave()) closeOverlay();
  };
  const header = h(
    "header",
    { className: "case-header" },
    h(
      "div",
      { className: "case-title" },
      h("h2", { title: caseId }, `Case ${short}`),
      h(IconButton, { action: "copy", label: `Copy case ID ${caseId}`, onClick: () => navigator.clipboard.writeText(caseId) }),
      h(IconButton, { action: "cancel", label: "Close case", className: "btn btn-sm btn-square btn-ghost case-close", onClick: close }),
    ),
    view.status === "ready"
      ? h(
          "div",
          { className: "case-tags" },
          h(CaseBadge, { kind: "action", value: view.c.action }),
          h(CaseBadge, { kind: "outcome", value: view.c.outcome }),
          h("span", { className: "muted" }, h(CaseTime, { ms: view.c.occurred_at_ms })),
          h("span", { className: "badge badge-soft badge-sm" }, state.guilds.find((g) => g.id === guildId)?.name || "This server"),
        )
      : null,
  );
  if (view.status !== "ready") {
    return h(
      DialogPanel,
      { className: "modal-box case-view", "aria-busy": view.status === "loading" },
      header,
      view.status === "loading"
        ? h("p", { className: "case-loading", role: "status", "aria-live": "polite" }, "Loading case…")
        : h(
            "div",
            { className: "case-error", role: "alert" },
            h("p", { className: "err" }, `Could not load this case: ${view.message}`),
            h(ActionButton, { type: "button", className: "btn btn-sm", onClick: () => setAttempt((n) => n + 1) }, "Try again"),
          ),
    );
  }
  const { c, names } = view;
  const automatic = caseAutomatic(c.actor_id, caseSourceKind(c.source));
  const policies = view.history.filter((item) => item.entry?.body?.policy).map((item) => item.entry.body.policy);
  const texts = Array.isArray(c.evidence?.text) ? c.evidence.text : [];
  const addNote = async () => {
    if (note.activity === "submitting") return;
    const text = noteRef.current.value.trim();
    if (!text) throw new Error("Write a note first.");
    if (note.intent.kind === "empty" || note.intent.body.text !== text) {
      note.intent = { kind: "retained", body: { entry_id: crypto.randomUUID().replaceAll("-", ""), text } };
    }
    note.activity = "submitting";
    try {
      await api(`${base}/history`, "POST", note.intent.body);
      note.intent = { kind: "empty" };
      noteRef.current.value = "";
      ui.set({ edits: "clean" });
      try {
        const history = await loadCaseHistory(base);
        setView((current) => (current.status === "ready" ? { ...current, history: history.entries, names: { ...current.names, ...history.names } } : current));
        toast("Note added", "success");
      } catch (_) {
        // The note is saved; only the timeline refresh failed.
        toast("Note added. Reopen the case to see it in the timeline.", "success");
      }
    } finally {
      note.activity = "idle";
    }
  };
  const remove = async () => {
    if (!confirm("Remove this case record? This does not undo the Discord action.")) return;
    const result = await api(base, "DELETE");
    closeOverlay();
    toast(
      caseCleanupState(result?.cleanup_pending) === "pending"
        ? "Case record removed. Cleanup is pending. Use Removed records / retry cleanup."
        : "Case record removed",
      "success",
    );
    onRemoved?.();
  };
  return h(
    DialogPanel,
    { className: "modal-box case-view", onInput: trackDraft },
    header,
    h(
      "section",
      { className: "case-section", "aria-label": "Who" },
      h("h3", {}, "Who"),
      h(
        "div",
        { className: "case-who" },
        h(CasePersonCard, { role: "Subject", id: c.subject_id, names, guildId, onFilter, filterKey: "subject" }),
        h(CasePersonCard, { role: "Moderator", id: c.actor_id, names, automatic, guildId, onFilter, filterKey: "actor" }),
      ),
    ),
    h(
      "section",
      { className: "case-section", "aria-label": "Why" },
      h("h3", {}, "Why"),
      c.reason ? h("p", { className: "case-reason" }, c.reason) : h("p", { className: "muted" }, "No reason recorded."),
      automatic ? h("p", { className: "muted" }, "Automatic action: jModGuard automod acted on its own.") : null,
      ...policies.map((policy, index) =>
        h("p", { key: `policy-${index}`, className: "muted" }, `Rule: ${policy.description || policy.rule_id}`),
      ),
    ),
    h("section", { className: "case-section", "aria-label": "Timeline" }, h("h3", {}, "Timeline"), h(CaseTimeline, { items: caseTimeline(c, view.history, names) })),
    h(
      "section",
      { className: "case-section", "aria-label": "Evidence" },
      h("h3", {}, "Evidence"),
      h(CaseEvidence, { guildId, caseId, texts, files: view.files, names, channels: view.channels }),
    ),
    !isPreview() && (canManageServer() || view.canDelete)
      ? h(
          "div",
          { className: "case-actions" },
          canManageServer()
            ? h(
                "div",
                { className: "case-note-form" },
                h(Textarea, {
                  ref: noteRef,
                  rows: "2",
                  "aria-label": "Note",
                  placeholder: "Add a note for other moderators. Discord isn't changed.",
                  className: "textarea",
                }),
                h(ActionButton, { className: "btn btn-sm btn-primary", type: "button", onClick: addNote }, "Add note"),
              )
            : null,
          view.canDelete
            ? h(
                ActionButton,
                {
                  type: "button",
                  className: "btn btn-sm case-remove",
                  title: "Deletes jModGuard's private record only. Discord isn't changed.",
                  onClick: remove,
                },
                h(Icon, { name: ACTION_ICONS.remove.icon, intent: ACTION_ICONS.remove.intent }),
                "Remove case record",
              )
            : null,
        )
      : null,
  );
}

const CASE_LIST = {
  list: "cases",
  filterLabel: "Filter cases",
  searchPlaceholder: "Reason, case or user ID, action, outcome",
  defaultSort: "date",
  defaultDir: "desc",
  filters: [
    { key: "action", label: "Action", options: CASE_ACTIONS },
    { key: "outcome", label: "Outcome", options: CASE_OUTCOMES },
    { key: "subject", label: "Subject", kind: "user", idLabel: "Subject user ID" },
    { key: "actor", label: "Moderator", kind: "user", idLabel: "Moderator user ID" },
  ],
  columns: [
    { key: "case", label: "Case" },
    { key: "action", label: "Action" },
    { key: "outcome", label: "Outcome" },
    { key: "subject", label: "Subject", title: "Sort by subject ID" },
    { key: "actor", label: "Moderator", title: "Sort by moderator ID" },
    { key: "date", label: "When", initialDir: "desc", orderLabels: { desc: "Newest first", asc: "Oldest first" } },
  ],
};
const BANS_LIST = {
  list: "bans",
  filterLabel: "Filter bans",
  defaultSort: "date",
  defaultDir: "desc",
  searchPlaceholder: "User, ID, reason or moderator",
  columns: [
    { key: "subject", label: "Banned user" },
    { key: "id", label: "ID" },
    { key: "status", label: "Status" },
    { key: "actor", label: "Moderator" },
    { key: "reason", label: "Reason" },
    { key: "date", label: "Time", initialDir: "desc", orderLabels: { desc: "Newest first", asc: "Oldest first" } },
  ],
  filters: [
    { key: "subject", label: "User", kind: "text", placeholder: "Name or ID" },
    { key: "actor", label: "Moderator" },
    { key: "reason", label: "Reason contains", kind: "text" },
    { key: "status", label: "Status", options: { active: { label: "Active" }, unbanned: { label: "Unbanned" } } },
  ],
};

const MEMBERS_LIST = {
  list: "members",
  filterLabel: "Filter members",
  searchPlaceholder: "User ID, mention, display name or username",
  defaultSort: "joined",
  defaultDir: "desc",
  dateLabels: { from: "Joined from", to: "Joined to" },
  filters: [
    {
      key: "status",
      label: "Status",
      emptyLabel: "Current members",
      options: {
        banned: { label: "Currently banned" },
        previously_banned: { label: "Previously banned" },
        all: { label: "All retained" },
      },
    },
  ],
  columns: [
    { key: "name", label: "Member" },
    { key: "user_id", label: "User ID" },
    { key: "joined", label: "Joined", initialDir: "desc", orderLabels: { desc: "Newest first", asc: "Oldest first" } },
    { key: "status", label: "Status" },
  ],
};
const EVENTS_LIST = {
  list: "events",
  filterLabel: "Filter the audit log",
  searchPlaceholder: "Reason, name, ID or event",
  defaultSort: "date",
  defaultDir: "desc",
  filters: [
    { key: "event", label: "Event", emptyLabel: "All events" },
    { key: "subject", label: "User", kind: "text", placeholder: "Name or ID" },
    { key: "actor", label: "Actor", kind: "text", placeholder: "Name or ID" },
  ],
  // Older Audit Log links name the user ID filter `user`.
  legacyFilters: { user: "subject" },
  columns: [
    { key: "date", label: "Time", initialDir: "desc", orderLabels: { desc: "Newest first", asc: "Oldest first" } },
    { key: "event", label: "Event" },
    { key: "user", label: "User", title: "Sort by user ID" },
    { key: "actor", label: "Actor", title: "Sort by actor ID" },
  ],
};
const WORKSPACE_LISTS = new Map([CASE_LIST, BANS_LIST, MEMBERS_LIST, EVENTS_LIST].map((config) => [config.list, config]));

function tabCases() {
  const guildId = state.guildId;
  const handlers = useRef({});
  const [rows, setRows] = useState(null);
  const [loading, setLoading] = useState(true);
  const [progress, setProgress] = useState(null);
  const [total, setTotal] = useState(null);
  const [pagination, setPagination] = useState({ first: true, next: true });
  const [canDelete, setCanDelete] = useState(false);
  const [removedView, setRemovedView] = useState(false);
  const controls = useListControls(CASE_LIST, guildId, () => handlers.current.first?.());
  const [pickerNames, setPickerNames] = useState(() => ({ status: "loaded", roles: [], channels: [], users: new Map() }));
  const rememberUser = (id, name) =>
    setPickerNames((current) => ({ ...current, users: new Map(current.users).set(id, name) }));
  const [handoff] = useState(() => {
    const next = pendingCase;
    pendingCase = null;
    return next;
  });
  useEffect(() => {
    let lifecycle = "active";
    let request = 0;
    let listController;
    let after = "";
    let listMode = "live";
    let nextAfter = "";
    const open = (id) => openCaseView({
      guildId,
      caseId: id,
      onRemoved: () => renderTab(),
      onFilter: (key, userId, name) => {
        if (name) rememberUser(userId, name);
        controls.replaceFilters({ [key]: userId });
      },
    });
    const run = async () => {
      const current = ++request;
      listController?.abort();
      const controller = new AbortController();
      listController = controller;
      const size = listMode === "live" ? controls.pageSize() : "50";
      setLoading(true);
      setProgress(listLoadProgress(size));
      setTotal(null);
      setPagination({ next: true, first: true });
      try {
        const query = listMode === "live" ? controls.query() : new URLSearchParams();
        query.set("removed", String(listMode === "removed"));
        if (after) query.set("after", after);
        const payload = await readListPage({
          read: apiRead, path: `/api/guilds/${guildId}/cases`, query,
          collection: "cases", size, signal: controller.signal, onProgress: setProgress,
        });
        if (lifecycle === "disposed" || current !== request || state.guildId !== guildId) return;
        setCanDelete(!!payload.can_delete);
        const ids = payload.cases;
        const names = payload.names;
        if (Object.keys(names).length)
          setPickerNames((value) => {
            const users = new Map(value.users);
            for (const [id, person] of Object.entries(names)) users.set(id, person.display_name);
            return { ...value, users };
          });
        nextAfter = payload.next_cursor || "";
        setPagination({ next: !nextAfter, first: !after });
        setTotal(payload.total ?? null);
        if (!ids.length) {
          setRows({ fallback: listMode === "removed"
            ? h("tr", null, h("td", { colSpan: 6, className: "muted" }, "No more removed records."))
            : h(ListEmptyRow, {
                controls, colSpan: 6,
                empty: "No selected cases on this page.",
                filtered: "No cases match these filters.",
              }) });
          return;
        }
        const renderRow = listMode === "removed"
          ? (id) => h(
                "tr",
                { key: id },
                h("td", {}, id),
                h(
                  "td",
                  { colSpan: 5 },
                  !isPreview() && h(
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
              )
          : (id) => {
            const c = payload.summaries?.[id];
            return h(
              "tr",
              {
                key: id,
                className: "case-row",
                // The whole row opens the case; its own controls keep their action.
                onClick: (event) => {
                  if (!event.target.closest("button, a, input, select, textarea")) open(id);
                },
              },
              h(
                "td",
                {},
                h("span", { className: "mono", title: id }, id.slice(0, 8)),
                " ",
                h(IconButton, { action: "view", label: `Open case ${id}`, onClick: () => open(id) }),
              ),
              h("td", {}, c ? h(CaseBadge, { kind: "action", value: c.action }) : "Summary unavailable"),
              h("td", {}, c?.outcome ? h(CaseBadge, { kind: "outcome", value: c.outcome }) : ""),
              h("td", {}, c?.subject_id ? h(CasePersonCell, { id: String(c.subject_id), names }) : ""),
              h("td", {}, c ? h(CasePersonCell, { id: c.actor_id ? String(c.actor_id) : null, names, empty: caseAutomatic(c.actor_id, c.source_kind) ? "jModGuard automod" : "Not recorded" }) : ""),
              h("td", { className: "muted" }, c?.occurred_at_ms ? h(CaseTime, { ms: c.occurred_at_ms }) : ""),
            );
          };
        setRows({ rows: ids, renderRow });
      } catch (e) {
        if (lifecycle === "disposed" || current !== request || state.guildId !== guildId) return;
        setPagination({ first: !after, next: true });
        setRows({ fallback: h(
            "tr",
            {},
            h(
              "td",
              { colSpan: 6, className: "err", role: "alert" },
              "Could not load cases: " + (e.message || String(e)),
            ),
          ) });
      } finally {
        if (lifecycle === "active" && current === request && state.guildId === guildId) {
          setLoading(false);
          setProgress(null);
        }
      }
    };
    handlers.current = {
      run,
      cancel: () => {
        ++request;
        listController?.abort();
        setLoading(false);
        setProgress(null);
        setPagination({ first: !after, next: true });
        setRows({ fallback:
          h("tr", null, h("td", { colSpan: 6, role: "status" }, "Loading canceled. Refresh to try again.")),
        });
      },
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
    if (handoff?.guildId === guildId) open(handoff.caseId);
    return () => {
      lifecycle = "disposed";
      listController?.abort();
      ++request;
    };
  }, [guildId, handoff]);
  return h(
    "div",
    null,
    h("h2", null, "Cases"),
    h("p", { className: "muted" }, "Moderation cases for this server. Select a case to open its file."),
    h(GoodToKnow, {
      items: [
        "Only moderation details are kept, never ordinary messages.",
        "Removing a record doesn't undo the ban, kick, timeout or warning in Discord.",
        "Removed records can retry cleanup. Their private content stays deleted.",
      ],
    }),
    removedView ? null : h(ListControls, {
      controls,
      UserPicker: IdPicker,
      userPickerProps: { names: pickerNames, rememberUser, memberStatus: "all" },
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
      Number.isInteger(total)
        ? h("span", { className: "muted", role: "status" }, `${total.toLocaleString()} matching cases`)
        : null,
    ),
    progress !== null ? h(ListLoadProgress, { loaded: progress, onCancel: () => handlers.current.cancel?.() }) : null,
    rows === null ? h(
      "div",
      { className: "card bg-base-100 card-border", role: "status", "aria-live": "polite" },
      h("div", { className: "card-body" }, "Loading cases…"),
    ) : h(PagedTable, {
      headings: CASE_LIST.columns.map((column) =>
        removedView ? column.label : h(ListSortHeading, { key: column.key, column, controls })),
      className: "table table-zebra case-table",
      ...(loading ? { fallback: h("tr", null, h("td", { colSpan: 6, role: "status" }, "Loading cases…")) } : rows),
    }),
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
      .filter((g) => g.id !== state.guildId && g.access === "manage")
      .map((g) => h("option", { key: g.id, value: g.id }, g.name)),
  );
  const sectionSelRef = useRef(null);
  const sectionSel = h(
    Select,
    { ref: sectionSelRef, className: "select", draftKey: "copy:section", defaultValue: "all" },
    ["all", "rules", "badwords", "commands", "templates", "settings"].map((s) =>
      h("option", { key: s, value: s }, s === "all" ? "everything" : s),
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

  const cards = [];
  if (isPreview()) cards.push(
    h(ListCard, { title: "This server" },
      h("p", null, `Prefix: ${cfg.settings.prefix}`),
      h("p", null, `Bot nickname: ${cfg.settings.bot_name || "Default"}`),
    ),
  );
  else cards.push(
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
  );

  const [roleBaseline, setRoleBaseline] = useState(null);
  const [accessRoleSettings, setAccessRoleSettings] = useState(null);
  const roleInputRef = useRef(null);
  const roleInput = h(Input, {
    ref: roleInputRef,
    draftKey: "settings:roles",
    defaultValue: roleBaseline ?? "",
    type: "text",
    placeholder: "role id, <@&id> mention, comma-separated",
    className: "input",
  });
  const roleRevisions = useRef({ access: "", caseRemoval: "" });
  const caseRoleWriteGeneration = useRef(0);
  const canManageRoles = canManageServer();
  const canEditCaseRemoval = canManageRoles && !isPreview() && selectedGuild()?.can_delete_cases === true;
  useEffect(() => {
    if (!canManageRoles) return undefined;
    let active = true;
    const guildId = state.guildId;
    const casePath = `/api/guilds/${guildId}/case-delete-roles`;
    const enrollment = pageEnrollment(casePath, "POST");
    const isCurrent = () => active && state.guildId === guildId && pageEnrollment(casePath, "POST") === enrollment;
    const initialCaseGeneration = caseRoleWriteGeneration.current;
    const load = async () => {
      const accessPath = `/api/guilds/${guildId}/access-roles`;
      const readPath = canEditCaseRemoval ? `${accessPath}?include_case_delete_roles=true` : accessPath;
      let loaded;
      try {
        loaded = await apiRead(readPath);
      } catch (error) {
        const { status } = error;
        if (!canEditCaseRemoval || readAccessFailed(error, enrollment) ||
          (status && status !== 408 && status !== 429 && status < 500)) throw error;
        if (isCurrent()) reportApiError(error);
      }
      if (!isCurrent()) return;
      try {
        if (canEditCaseRemoval) {
          const caseSettings = loaded?.case_delete_roles == null
            ? await apiRead(casePath)
            : { roles: loaded.case_delete_roles, revision: loaded.revision };
          if (isCurrent() && initialCaseGeneration === caseRoleWriteGeneration.current) {
            roleRevisions.current.caseRemoval = caseSettings.revision || "";
            setRoleBaseline((caseSettings.roles || []).join(", "));
          }
        }
      } finally {
        if (isCurrent() && loaded) {
          roleRevisions.current.access = loaded.revision || "";
          setAccessRoleSettings({ roles: accessRoleRows(loaded.roles || []), ttl_secs: loaded.ttl_secs });
        }
      }
    };
    load().catch((e) => {
      if (isCurrent() && !isUnauthenticated(e)) reportApiError(e);
    });
    return () => {
      active = false;
    };
  }, []);
  if (canManageRoles) cards.push(
    h(AccessRolesCard, { key: "access-roles", settings: accessRoleSettings, revisions: roleRevisions }),
  );
  if (canEditCaseRemoval) cards.push(
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
                const revision = roleRevisions.current.caseRemoval;
                let recovered = false;
                const saved = await api(`/api/guilds/${state.guildId}/case-delete-roles`, "PUT", {
                  roles: splitIds(roleInputRef.current.value),
                  revision,
                }, undefined, () => {
                  recovered = true;
                  return apiRead(`/api/guilds/${state.guildId}/case-delete-roles`);
                });
                caseRoleWriteGeneration.current++;
                advanceRoleRevision(roleRevisions.current, "caseRemoval", recovered ? "" : revision, saved.revision);
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

  if (canManageServer() && !isPreview()) cards.push(
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
  ["dashboard", "Dashboard"],
  ["settings", "Dashboard and settings"],
];

function accessRoleRows(roles) {
  return roles.map((role) => ({ ...role, key: crypto.randomUUID() }));
}

function advanceRoleRevision(revisions, card, previous, next) {
  const sibling = card === "access" ? "caseRemoval" : "access";
  // A fresh save can carry forward a sibling baseline from the same snapshot.
  // A recovered receipt only establishes the baseline read for its own card.
  if (previous && revisions[sibling] === previous) revisions[sibling] = next;
  revisions[card] = next;
}

// Role -> access mappings for this server. Only Manage Server sees this card;
// the API refuses the read and the save for everyone else.
function AccessRolesCard({ settings, revisions }) {
  const guildId = state.guildId;
  const [savedRows, setSavedRows] = useState(null);
  const baselineRows = savedRows ?? settings?.roles ?? null;
  const ttl = settings?.ttl_secs || 30;
  const draftKey = "settings:access-roles";
  const draftRows = useStore(ui, ({ drafts }) => drafts[draftKey]?.value);
  const rows = baselineRows === null ? null : draftRows ?? baselineRows;
  const changeRows = (next) => {
    const values = (items) => JSON.stringify(items.map(({ role_id, level }) => ({ role_id, level })));
    const nextDrafts = { ...currentDrafts() };
    if (baselineRows !== null && values(next) === values(baselineRows)) delete nextDrafts[draftKey];
    else nextDrafts[draftKey] = { value: next, group: "access-roles" };
    ui.set({ drafts: nextDrafts, edits: Object.keys(nextDrafts).length > 0 ? "dirty" : "clean" });
  };
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
          "Discord’s Ban Members permission allows ban actions at either access level.",
          "Settings access can also edit rules, badwords, known spam and images, templates, log channels and bot settings.",
          "Commands, copying configuration, member import controls, case notes and this list need Manage Server.",
          "Removing case records requires server ownership or both Manage Server and Administrator permissions.",
          `A removed role stops working as soon as the bot sees the change, and within ${ttl >= 120 ? `${Math.round(ttl / 60)} minutes` : `${ttl} seconds`} at most.`,
        ],
      }),
      rows === null
        ? h("p", { className: "muted", role: "status" }, "Loading roles…")
        : isPreview()
          ? rows.length
            ? h("ul", null, ...rows.map((row) => h("li", { key: row.key }, `${row.role_id} — ${ACCESS_LEVELS.find(([level]) => level === row.level)?.[1] || row.level}`)))
            : h("p", { className: "muted" }, "No roles have Web UI access.")
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
                  onClick: () => changeRows([...rows, ...accessRoleRows([{ role_id: "", level: "dashboard" }])]),
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
                    const revision = revisions.current.access;
                    let recovered = false;
                    const saved = await api(`/api/guilds/${guildId}/access-roles`, "PUT", {
                      roles,
                      revision,
                    }, undefined, () => {
                      recovered = true;
                      return apiRead(`/api/guilds/${guildId}/access-roles`);
                    });
                    advanceRoleRevision(revisions.current, "access", recovered ? "" : revision, saved.revision);
                    setSavedRows(accessRoleRows(saved.roles || roles));
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
function BanOptions({ action, onChange }) {
  const id = React.useId();
  // An existing day count stays selectable until the editor closes.
  const [legacyDays] = useState(() => action.delete_days || 0);
  const selected = action.purge_last_hour ? "purge" : action.delete_days ? "legacy" : "ban";
  return h(
    "div",
    { className: "ban-options" },
    h(
      Field,
      { className: "ban-behavior" },
      h(Label, { className: "sr-only" }, "Ban behavior"),
      h(
        Select,
        {
          className: "select select-sm",
          defaultValue: selected,
          onChange: (event) => {
            const mode = event.target.value;
            action.purge_last_hour = mode === "purge";
            action.delete_days = mode === "legacy" ? legacyDays : 0;
            onChange?.();
          },
        },
        h("option", { value: "ban" }, "Keep messages"),
        h("option", { value: "purge" }, "Delete last hour"),
        legacyDays > 0 && h("option", { value: "legacy" }, `Delete ${legacyDays} day(s)`),
      ),
    ),
    h(InfoTip, { id: `ban-help-${id}`, label: "About ban behavior" }, [
      "Keep messages: their earlier messages stay.",
      "Delete last hour: Discord removes their messages from the past hour, server-wide.",
      "Dry-run never bans or deletes.",
    ]),
  );
}

// One escalation step as a compact row: "Strike [1] [Ban] [Keep messages]".
// Only the chosen action's own setting shows: minutes for a timeout, messages for a ban.
function RuleAction({ action, index, remove, onChange, strike = true, clearBan = false, choices = ["log", "delete", "warn", "timeout", "kick", "ban"] }) {
  const [kind, setKind] = useState(action.action);
  return h(
    "div",
    { className: "escalation-step" },
    h(
      "div",
      { className: "escalation-row" },
      strike
        ? h(
            Field,
            { className: "trigger-field" },
            h(Label, { className: "escalation-label" }, "Strike", h("span", { className: "sr-only" }, " count")),
            h(Input, {
              className: "input input-num",
              type: "number",
              min: 1,
              defaultValue: action.at,
              onChange: (event) => {
                action.at = Math.max(1, parseInt(event.target.value) || 1);
                onChange?.();
              },
            }),
          )
        : null,
      h(
        Field,
        { className: "trigger-field" },
        h(Label, { className: "sr-only" }, "Action"),
        h(
          Select,
          {
            className: "select select-sm",
            defaultValue: kind,
            onChange: (event) => {
              const next = event.target.value;
              action.action = next;
              // Custom rules always defaulted a new timeout to an hour; escalation steps keep their value.
              if (clearBan && next === "timeout" && !action.duration_minutes) action.duration_minutes = 60;
              if (clearBan && next !== "ban") {
                action.delete_days = 0;
                action.purge_last_hour = false;
              }
              setKind(next);
              onChange?.();
            },
          },
          ...choices.map((value) => h("option", { key: value, value }, !strike && value === "log" ? "Log to automod" : RULE_ACTION_LABELS[value])),
        ),
      ),
      kind === "timeout"
        ? h(
            Field,
            { className: "trigger-field" },
            h(Label, { className: "sr-only" }, "Timeout (minutes)"),
            h(Input, {
              className: "input input-num",
              type: "number",
              min: clearBan ? 1 : 0,
              defaultValue: action.duration_minutes ?? 0,
              onChange: (event) => {
                action.duration_minutes = parseInt(event.target.value) || 0;
                onChange?.();
              },
            }),
            h("span", { "aria-hidden": "true" }, "min"),
          )
        : null,
      kind === "ban" ? h(BanOptions, { action, onChange }) : null,
      remove
        ? h(IconButton, { action: "remove", label: `Remove action ${index + 1}`, className: "btn btn-sm btn-square escalation-remove", onClick: remove })
        : null,
    ),
    h(
      Field,
      { className: "escalation-reason" },
      h(Label, { className: "escalation-label" }, "Reason", h("span", { className: "sr-only" }, " template")),
      h(Input, {
        className: "input input-sm",
        type: "text",
        defaultValue: action.reason || "",
        placeholder: "Shown in logs (optional)",
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
// One action plan step as a compact row (#623): "1. [Add role] [@Role]". Only the
// chosen type's own fields show; the step object is what Save sends.
function CommandStep({ step, index, names, pending, onChange, remove }) {
  const [roleIds] = useState(() => (step.role_id ? [String(step.role_id)] : []));
  const [customEmoji, setCustomEmoji] = useState(() => !REACT_EMOJI.includes(step.emoji));
  const [, bump] = useState(0);
  const n = index + 1;
  const changed = () => {
    markDirty();
    bump((value) => value + 1);
    onChange();
  };
  const set = (key) => (value) => {
    step[key] = value;
    changed();
  };
  const type = step.type;
  const sr = (text) => h(Label, { className: "sr-only" }, `Step ${n} ${text}`);
  const known = Object.hasOwn(PLAN_STEP_LABELS, type);
  let inline = null;
  if (type === "log")
    inline = h(
      Field,
      { className: "trigger-field" },
      sr("log event"),
      h(Select, { className: "select select-sm", defaultValue: step.event || "", onChange: (e) => set("event")(e.target.value) }, logEventOptions(step.event || "", false)),
    );
  else if (type === "timeout")
    inline = h(NumberField, { label: `Step ${n} minutes`, hidden: true, value: step.minutes, unit: "min", min: 1, max: STEP_TIMEOUT_MAX, onChange: set("minutes") });
  else if (type === "wait")
    inline = h(NumberField, { label: `Step ${n} seconds`, hidden: true, value: step.secs, unit: "s", min: 1, onChange: set("secs") });
  else if (type === "ban") inline = h(DeleteDaysSelect, { label: `Step ${n} messages`, value: step.delete_days, onChange: set("delete_days") });
  else if (type === "react")
    inline = h(
      React.Fragment,
      null,
      h(
        Field,
        { className: "trigger-field" },
        sr("emoji"),
        h(
          Select,
          {
            className: "select select-sm emoji-select",
            defaultValue: customEmoji ? "" : step.emoji,
            onChange: (e) => {
              const custom = e.target.value === "";
              setCustomEmoji(custom);
              if (!custom) set("emoji")(e.target.value);
            },
          },
          REACT_EMOJI.map((emoji) => h("option", { key: emoji, value: emoji }, emoji)),
          h("option", { value: "" }, "Other…"),
        ),
      ),
      customEmoji
        ? h(
            Field,
            { className: "trigger-field" },
            sr("other emoji"),
            h(Input, { type: "text", className: "input input-sm input-num", defaultValue: step.emoji || "", placeholder: "Emoji", onInput: (e) => set("emoji")(e.target.value.trim()) }),
          )
        : null,
    );
  let below = null;
  if (type === "reply" || type === "dm" || type === "log")
    below = h(
      Field,
      { className: "escalation-reason" },
      h(Label, { className: "escalation-label" }, "Message", h("span", { className: "sr-only" }, ` for step ${n}`)),
      h(Input, {
        type: "text",
        className: "input input-sm",
        defaultValue: step.template || "",
        placeholder: type === "dm" ? "e.g. You were warned: {reason}" : type === "log" ? "e.g. {actor} ran {command}" : "e.g. Hi {user}!",
        onInput: (e) => set("template")(e.target.value),
      }),
    );
  else if (type === "add_role" || type === "remove_role")
    below = h(IdPicker, {
      kind: "role",
      label: `Role for step ${n}`,
      idLabel: `Add a role by ID for step ${n}`,
      ids: roleIds,
      names,
      pending,
      single: true,
      onChange: () => {
        step.role_id = roleIds[0] || "";
        changed();
      },
    });
  return h(
    "div",
    { className: "escalation-step command-step" },
    h(
      "div",
      { className: "escalation-row" },
      h(
        Field,
        { className: "trigger-field" },
        h(Label, { className: "escalation-label" }, `${n}.`, h("span", { className: "sr-only" }, ` Step ${n} action`)),
        h(
          Select,
          {
            className: "select select-sm",
            defaultValue: type,
            onChange: (event) => {
              const next = planStepFor(event.target.value, step);
              Object.keys(step).forEach((key) => delete step[key]);
              Object.assign(step, next);
              roleIds.splice(0, roleIds.length, ...(step.role_id ? [String(step.role_id)] : []));
              setCustomEmoji(false);
              changed();
            },
          },
          known ? null : h("option", { value: type }, type),
          Object.entries(PLAN_STEP_LABELS).map(([value, label]) => h("option", { key: value, value }, label)),
        ),
      ),
      // Keyed by type so a new type's inputs start from its own values.
      h(React.Fragment, { key: type }, inline),
      h(IconButton, { action: "remove", label: `Remove step ${n}`, className: "btn btn-sm btn-square escalation-remove", onClick: remove }),
    ),
    below ? h(React.Fragment, { key: type }, below) : null,
  );
}

// Display reads opted into `cache` are kept per signed-in user in memory and in
// this tab's sessionStorage. A cached copy renders at once on navigation or
// reload while the same read revalidates it, and visible pages revalidate every
// few minutes. The server still decides freshness: a refresh always asks the API,
// whose Discord data has its own refresh interval. Sign-out and auth loss clear it.
const READ_CACHE_PREFIX = "jmodguard.read:";
const READ_CACHE_ENTRIES = 32;
// Bound the combined length of keys and serialized values, both in storage (leaving
// room for request identities) and for the copies kept in memory. That length is an
// admission proxy, not JS heap bytes. A visible view keeps its own data either way.
const READ_CACHE_STORAGE_LIMIT = 1024 * 1024;
const READ_REFRESH_MS = 3 * 60 * 1000;
const readCache = new Map();
let readCacheWeight = 0;
let readSessionEnding = false;

function readCacheKey(path, listSize, enrollment) {
  const user = state.me?.user?.id;
  if (!user || isPreview() || readSessionEnding || (path.startsWith("/api/guilds/") && !enrollment)) return null;
  return READ_CACHE_PREFIX + JSON.stringify([apiBase(), user, path, listSize, enrollment]);
}

function forgetRead(key) {
  const old = readCache.get(key);
  if (!old) return;
  readCache.delete(key);
  readCacheWeight -= old.weight;
}

function rememberRead(key, entry, weight) {
  const old = readCache.get(key);
  weight ??= old?.entry === entry ? old.weight : key.length + JSON.stringify(entry).length;
  forgetRead(key);
  if (weight > READ_CACHE_STORAGE_LIMIT) return;
  readCache.set(key, { entry, weight });
  readCacheWeight += weight;
  for (const oldKey of readCache.keys()) {
    if (readCache.size <= READ_CACHE_ENTRIES && readCacheWeight <= READ_CACHE_STORAGE_LIMIT) break;
    forgetRead(oldKey);
  }
}

function cachedRead(key, shape) {
  if (readCache.has(key)) return readCache.get(key).entry;
  try {
    const saved = JSON.parse(window.sessionStorage.getItem(key));
    if (saved && Number.isSafeInteger(saved.at) && saved.at >= 0 && saved.at <= 8.64e15 && validDisplayRead(saved.data, shape)) {
      return saved;
    }
  } catch (_) { /* An unreadable copy is only a cache miss. */ }
  return null;
}

function storedReadKeys(storage) {
  const keys = [];
  for (let i = 0; i < storage.length; i += 1) {
    const key = storage.key(i);
    if (key?.startsWith(READ_CACHE_PREFIX)) keys.push(key);
  }
  return keys;
}

function setSessionItemWithReadCacheFallback(storage, key, value) {
  try {
    storage.setItem(key, value);
  } catch (error) {
    // Request identities must be persisted before dispatch. Display copies can be fetched again.
    if (error.name !== "QuotaExceededError") throw error;
    for (const oldKey of storedReadKeys(storage)) storage.removeItem(oldKey);
    storage.setItem(key, value);
  }
}

function storeRead(key, data, at) {
  const entry = { at, data };
  let value;
  try {
    value = JSON.stringify(entry);
  } catch (_) {
    // A response too large to serialize is too large to cache.
    discardRead(key);
    return;
  }
  const size = key.length + value.length;
  rememberRead(key, entry, size);
  try {
    const storage = window.sessionStorage;
    if (size > READ_CACHE_STORAGE_LIMIT) {
      storage.removeItem(key);
      return;
    }
    const others = storedReadKeys(storage).filter((oldKey) => oldKey !== key).map((oldKey) => ({
      key: oldKey, size: oldKey.length + storage.getItem(oldKey).length,
    }));
    let used = others.reduce((total, item) => total + item.size, size);
    for (const [index, old] of others.entries()) {
      if (others.length - index < READ_CACHE_ENTRIES && used <= READ_CACHE_STORAGE_LIMIT) break;
      storage.removeItem(old.key);
      used -= old.size;
    }
    setSessionItemWithReadCacheFallback(storage, key, value);
  } catch (_) { /* Memory still holds this tab's copy. */ }
}

function discardRead(key) {
  forgetRead(key);
  try {
    window.sessionStorage.removeItem(key);
  } catch (_) { /* Optional storage can be unavailable. */ }
}

function readAccessFailed(error, enrollment) {
  return isUnauthenticated(error) || error.status === 403 || error.status === 409 ||
    error.code === "guild_access_denied" || error.code === "guild_access_unverified" ||
    (enrollment && error.code === "discord_rate_limited");
}

function clearReadCache() {
  readCache.clear();
  readCacheWeight = 0;
  try {
    const storage = window.sessionStorage;
    for (const key of storedReadKeys(storage)) storage.removeItem(key);
  } catch (_) { /* Nothing readable is left to clear. */ }
}

function isReadObject(data) {
  return data != null && typeof data === "object" && !Array.isArray(data);
}

function validDisplayRead(data, shape) {
  if (!isReadObject(data)) return false;
  if (shape === "stats") {
    return ["events", "warns", "bans"].every((key) => Number.isSafeInteger(data[key]) && data[key] >= 0) &&
      (data.cases == null || (Number.isSafeInteger(data.cases) && data.cases >= 0));
  }
  return Array.isArray(data[shape]) && data[shape].every(isReadObject) &&
    (data.next_cursor == null || typeof data.next_cursor === "string") &&
    (data.names == null || isReadObject(data.names)) &&
    (data.moderators == null || (Array.isArray(data.moderators) && data.moderators.every(isReadObject)));
}

// Running chunks can finish while hidden. A background read waits here before
// dispatching another chunk, outside that chunk's network deadline.
function waitForReadVisibility(signal) {
  signal.throwIfAborted();
  if (document.visibilityState === "visible") return;
  return new Promise((resolve, reject) => {
    const cleanup = () => {
      document.removeEventListener("visibilitychange", visible);
      signal.removeEventListener("abort", abort);
    };
    const visible = () => {
      if (document.visibilityState !== "visible") return;
      cleanup();
      resolve();
    };
    const abort = () => {
      cleanup();
      reject(signal.reason);
    };
    document.addEventListener("visibilitychange", visible);
    signal.addEventListener("abort", abort, { once: true });
  });
}

function useCachedApiRead(path, shape, { listSize, holdBackgroundResult = false } = {}) {
  const enrollment = pageEnrollment(path, "POST");
  const cacheKey = readCacheKey(path, listSize, enrollment);
  const identity = cacheKey ?? JSON.stringify([path, listSize, enrollment]);
  const poll = Boolean(cacheKey) || isPreview();
  const [result, setResult] = useState(null);
  const controls = useRef(null);
  const holdResult = useRef(holdBackgroundResult);
  useLayoutEffect(() => { holdResult.current = holdBackgroundResult; }, [holdBackgroundResult]);

  // Read a matching snapshot before the first output. The effect owns all
  // further publication and storage changes for this identity.
  const initialCopy = result?.identity !== identity && cacheKey ? cachedRead(cacheKey, shape) : null;
  const shown = result?.identity === identity ? result : initialCopy
    ? { data: initialCopy.data, updatedAt: initialCopy.at }
    : { loading: true };

  useEffect(() => {
    if (readSessionEnding) return undefined;
    const [endpoint, query] = path.split("?", 2);
    let snapshot = cacheKey ? cachedRead(cacheKey, shape) : null;
    if (snapshot) rememberRead(cacheKey, snapshot);
    let active = null;
    let timer = null;
    let nextAt = Date.now();
    let disposed = false;
    const show = (fields = {}) => setResult({
      identity, data: snapshot?.data, updatedAt: snapshot?.at, ...fields,
    });
    const clearTimer = () => {
      if (timer !== null) clearTimeout(timer);
      timer = null;
    };
    const cancelActive = () => {
      const request = active;
      active = null;
      request?.cancel();
    };
    const schedule = () => {
      clearTimer();
      if (disposed || !poll || active || document.visibilityState !== "visible" || !Number.isFinite(nextAt)) return;
      const delay = nextAt - Date.now();
      if (delay <= 0) void start(true);
      else timer = setTimeout(schedule, delay);
    };
    async function start(background) {
      if (disposed || (background && active)) return;
      clearTimer();
      cancelActive();
      const controller = new AbortController();
      const { signal } = controller;
      const timeout = listSize === undefined ? setTimeout(() => controller.abort(), 30000) : null;
      const request = { cancel() {
        if (timeout !== null) clearTimeout(timeout);
        controller.abort();
      } };
      active = request;
      const copy = cacheKey ? snapshot : null;
      show({
        data: copy?.data, updatedAt: copy?.at, loading: !copy,
        revalidating: Boolean(copy), progress: listLoadProgress(listSize),
      });
      const read = async (url, requestSignal) => {
        const data = await apiRequest(url, "POST", null, requestSignal,
          enrollment ? { "x-jmodguard-enrollment": enrollment } : {});
        if (!validDisplayRead(data, shape)) throw new Error("The API returned an invalid display response.");
        return data;
      };
      const holdsBackgroundResult = () => background && snapshot !== null && holdResult.current;
      try {
        const data = listSize === undefined ? await read(path, signal) : await readListPage({
          read, path: endpoint, query, collection: shape, size: listSize, signal,
          beforeChunk: background ? () => waitForReadVisibility(signal) : undefined,
          // The first scoped read still detects access loss. The editor only
          // prevents dispatching subsequent chunks that cannot be published.
          shouldContinue: background ? () => !holdsBackgroundResult() : undefined,
          onProgress: (progress) => {
            if (active === request) setResult((current) => ({ ...current, progress }));
          },
        });
        if (active !== request) return;
        signal.throwIfAborted();
        // A normal data refresh must not remove an inline reason editor. The
        // access-failure path below still clears the restricted snapshot.
        if (data !== null && !holdsBackgroundResult()) {
          const at = Date.now();
          snapshot = { at, data };
          if (cacheKey) storeRead(cacheKey, data, at);
        }
        show();
      } catch (error) {
        if (active !== request) return;
        const failure = signal.aborted
          ? apiError("Request timed out. Refresh to verify saved state before trying again.")
          : error;
        const accessFailed = readAccessFailed(failure, enrollment);
        const { status, code } = failure;
        const canKeep = cacheKey && !accessFailed &&
          (!status || status === 408 || status === 429 || status >= 500 || code === "list_deadline");
        if (cacheKey && accessFailed) clearReadCache();
        if (snapshot && canKeep) {
          show({ refreshError: failure });
        } else {
          if (cacheKey && !accessFailed) discardRead(cacheKey);
          snapshot = null;
          show({ error: failure, accessFailed });
        }
      } finally {
        if (timeout !== null) clearTimeout(timeout);
        if (active === request) {
          active = null;
          nextAt = Date.now() + READ_REFRESH_MS;
          schedule();
        }
      }
    }

    const dispose = () => {
      disposed = true;
      clearTimer();
      cancelActive();
      document.removeEventListener("visibilitychange", schedule);
      authLossCleanups.delete(stop);
      controls.current = null;
    };
    const stop = () => {
      dispose();
      snapshot = null;
      show({ loading: true });
    };
    controls.current = {
      cancel: () => {
        cancelActive();
        clearTimer();
        nextAt = Infinity;
        setResult({ identity, canceled: true });
      },
      refresh: () => { void start(false); },
    };
    authLossCleanups.add(stop);
    if (poll) document.addEventListener("visibilitychange", schedule);
    if (snapshot) schedule();
    else void start(false);
    return dispose;
  }, [path, shape, listSize, enrollment, cacheKey, identity, poll]);

  return {
    ...shown,
    progress: shown.progress ?? null,
    cancel: () => controls.current?.cancel(),
    refresh: () => controls.current?.refresh(),
  };
}

function useApiRead(path, { retainData = false, listSize, collection } = {}) {
  const [revision, setRevision] = useState(0);
  const [result, setResult] = useState({ kind: "loading" });
  const cancel = useRef(null);
  useEffect(() => {
    if (!path) return;
    const controller = new AbortController();
    let publication = "current";
    const timeout = collection ? null : setTimeout(() => controller.abort(), 30000);
    setResult((current) => {
      const retained = retainData && current.path === path && current.data != null;
      // A pending retry does not make the retained snapshot fresh again.
      return {
        kind: "loading", path, data: retained ? current.data : null, error: retained ? current.error : null,
        progress: listLoadProgress(listSize),
      };
    });
    const [endpoint, query] = path.split("?", 2);
    const read = collection ? readListPage({
      read: apiRead, path: endpoint, query, collection, size: listSize, signal: controller.signal,
      onProgress: (progress) => {
        if (publication === "current") setResult((current) => ({ ...current, progress }));
      },
    }) : apiRead(path, controller.signal);
    cancel.current = () => {
      publication = "disposed";
      controller.abort();
      setResult({ kind: "canceled", path });
    };
    read
      .then((data) => {
        if (publication === "current") setResult({ kind: "loaded", path, data });
      })
      .catch((error) => {
        if (publication === "current")
          setResult((current) => ({
            ...current,
            kind: "failed",
            progress: null,
            error: controller.signal.aborted
              ? apiError("Request timed out. Refresh to verify saved state before trying again.")
              : error,
          }));
      })
      .finally(() => { if (timeout !== null) clearTimeout(timeout); });
    return () => {
      publication = "disposed";
      if (timeout !== null) clearTimeout(timeout);
      controller.abort();
    };
  }, [path, revision, retainData, collection, listSize]);
  return {
    loading: Boolean(path) && result.kind === "loading",
    data: retainData ? (result.path === path ? result.data ?? null : null) : result.kind === "loaded" ? result.data : null,
    error: retainData ? (result.path === path ? result.error ?? null : null) : result.kind === "failed" ? result.error : null,
    progress: result.progress ?? null,
    canceled: result.kind === "canceled",
    cancel: () => cancel.current?.(),
    refresh: () => setRevision((value) => value + 1),
  };
}

// When a cached display read was last confirmed by the API.
function LastUpdated({ result }) {
  const text = result.revalidating
    ? "Refreshing…"
    : result.updatedAt
      ? `Last updated ${new Date(result.updatedAt).toLocaleTimeString()}`
      : null;
  return h(React.Fragment, null,
    text ? h("span", { className: "muted last-updated", role: "status" }, text) : null,
    result.refreshError ? h("span", { className: "err", role: "alert" },
      `Refresh failed: ${result.refreshError.message}. Showing the last successful response (stale).`,
    ) : null,
  );
}
function DataTable({ headings, children, className = "table table-zebra", rowCount }) {
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
      { className, "aria-rowcount": rowCount },
      h("thead", null, h("tr", { "aria-rowindex": rowCount ? 1 : undefined }, ...headings.map((heading) => typeof heading === "string" ? h("th", { key: heading, scope: "col" }, heading) : heading))),
      h("tbody", null, ...React.Children.toArray(children)),
    ),
  );
}

const TABLE_WINDOW_SIZE = 100;

function PagedTable({ rows, renderRow, fallback, canMove, ...table }) {
  if (rows?.length >= 500) return h(TableWindow, { rows, renderRow, canMove, table });
  return h(DataTable, table, rows?.map(renderRow) ?? fallback);
}

function TableWindow({ rows, renderRow, canMove = () => true, table }) {
  const [requestedOffset, setOffset] = useState(0);
  const lastOffset = Math.floor((rows.length - 1) / TABLE_WINDOW_SIZE) * TABLE_WINDOW_SIZE;
  const offset = Math.min(requestedOffset, lastOffset);
  const end = Math.min(offset + TABLE_WINDOW_SIZE, rows.length);
  // Keep the navigation buttons focusable at the ends of the table.
  const move = (label, available, next) => h("button", {
    type: "button", className: "btn btn-sm", "aria-disabled": !available,
    onClick: () => { if (available && canMove()) setOffset(next); },
  }, label);
  return h(React.Fragment, null,
    h("div", { className: "row list-pagination" },
      move("Previous rows", offset > 0, offset - TABLE_WINDOW_SIZE),
      move("Next rows", end < rows.length, end),
      h("span", { className: "muted", role: "status" },
        `Showing rows ${(offset + 1).toLocaleString()}–${end.toLocaleString()} of ${rows.length.toLocaleString()} on this page`,
      ),
    ),
    h(DataTable, { ...table, rowCount: rows.length + 1 },
      rows.slice(offset, end).map((row, i) =>
        React.cloneElement(renderRow(row, offset + i), { "aria-rowindex": offset + i + 2 }),
      ),
    ),
  );
}
// Audit Log events written outside the log-channel routes in event_types.
const AUDIT_EXTRA_EVENT_TYPES = ["ban_reason_edit", "command_enabled"];
// Command rows are written after the command succeeds, but `!timeout` stores
// "timeout" both when it adds and when it removes a timeout.
const AUDIT_MOD_ACTION_DONE = {
  ban: "Banned", kick: "Kicked", softban: "Softbanned", unban: "Unbanned", timeout: "Timeout changed",
  warn: "Warned", warnclear: "Warnings cleared", purge: "Purged", slowmode: "Slowmode set",
};

function auditText(value) {
  return typeof value === "string" && value.trim() ? value.trim() : typeof value === "number" ? String(value) : null;
}

function auditReason(label, data, empty = "No reason given") {
  return `${label}: ${auditText(data.reason) ?? empty}`;
}

function auditSource(data) {
  return data.source === "discord" ? "via Discord" : data.source === "webui" ? "via the Web UI" : null;
}

function auditTime(value) {
  const date = new Date(value);
  return Number.isNaN(date.getTime())
    ? null
    : date.toLocaleString(undefined, { month: "numeric", day: "numeric", hour: "numeric", minute: "2-digit" });
}

// One readable line per event type. Each returns only fields people need:
// internal payload fields such as audit_after_ms or action_id stay in Raw.
const AUDIT_EVENT_SUMMARIES = {
  member_join: () => ["Joined the server"],
  member_leave: () => ["Left the server"],
  member_ban: (data) => [
    auditReason("Banned", data, "Reason unavailable"),
    auditSource(data),
  ],
  member_unban: (data) => [
    auditText(data.reason) ? auditReason("Unbanned", data) : "Unbanned",
    auditSource(data),
    data.outcome === "already_unbanned" ? "Already unbanned" : null,
  ],
  member_kick: (data) => [auditReason("Kicked", data)],
  message_edit: () => ["Message edited"],
  message_delete: () => ["Message deleted"],
  message_bulk_delete: () => ["Messages deleted in bulk"],
  // Automod rows store the rule's action, also after a failed or skipped attempt.
  automod: (data) => [
    auditReason(auditText(data.action) ? `Automod ${auditText(data.action)} action` : "Automod action", data),
    auditText(data.rule) ? `Rule: ${data.rule}` : null,
  ],
  mod_action: (data) => [
    auditReason(AUDIT_MOD_ACTION_DONE[data.action] ?? "Moderation command", data),
    "via command",
  ],
  timeout_add: (data) => [auditTime(data.until) ? `Timed out until ${auditTime(data.until)}` : "Timed out"],
  timeout_remove: () => ["Timeout removed"],
  plan_executed: (data) => [
    auditText(data.command) ? `Ran command ${data.command}` : "Ran a command",
    Number.isInteger(data.steps) ? `${data.steps} ${data.steps === 1 ? "step" : "steps"}` : null,
  ],
  system: (data) => ["System event", auditText(data.message) ?? auditText(data.reason)],
  ban_reason_edit: (data) => [
    `Ban reason changed from “${auditText(data.previous_reason) ?? "No reason given"}” to “${auditText(data.reason) ?? "No reason given"}”`,
  ],
  command_enabled: (data) => [
    `Command ${auditText(data.command) ?? "unknown"} turned ${data.enabled ? "on" : "off"}`,
    data.scope === "global" ? "for every server" : "for this server",
  ],
};

// The readable summary, or null when the type is unknown or the data is not a JSON object.
function auditEventSummary(record) {
  const format = Object.hasOwn(AUDIT_EVENT_SUMMARIES, record.event_type) ? AUDIT_EVENT_SUMMARIES[record.event_type] : null;
  if (!format) return null;
  let data;
  try {
    data = JSON.parse(record.data || "{}");
  } catch (_) {
    return null;
  }
  if (!data || typeof data !== "object" || Array.isArray(data)) return null;
  return { text: format(data).filter(Boolean).join(" · "), banId: data.ban_id ?? null };
}

function AuditRawDetails({ data }) {
  const [open, setOpen] = useState(false);
  let raw;
  if (open) {
    try {
      raw = JSON.stringify(JSON.parse(data), null, 1);
    } catch (_) {
      raw = data;
    }
  }
  return h("details", {
    className: "audit-raw",
    onToggle: (event) => setOpen(event.currentTarget.open),
  }, h("summary", null, "Raw"), open ? h("pre", { className: "mono" }, raw) : null);
}

function AuditEventDetails({ record, guildId }) {
  const summary = auditEventSummary(record);
  const openBan = () => openFilteredList(EVENTS_LIST, BANS_LIST, guildId, { subject: String(record.user_id) }, () => navigate("bans"));
  return h(
    "div",
    { className: "audit-details" },
    h("span", { className: summary ? null : "muted" }, summary ? summary.text : "No readable summary for this event."),
    summary?.banId != null && record.user_id
      ? h("button", { type: "button", className: "btn btn-xs btn-link", onClick: openBan }, "View ban")
      : null,
    isBotOwner() && record.data ? h(AuditRawDetails, { data: record.data }) : null,
  );
}

function AuditPersonCell({ id, names, label, filterLink }) {
  if (!id) return h("span", { className: "muted" }, "-");
  return h(BanPersonCell, { person: banPerson(id, null, null, names), label, filterLink });
}

function tabEvents() {
  const guildId = state.guildId;
  const [after, setAfter] = useState("");
  const controls = useListControls(EVENTS_LIST, guildId, () => setAfter(""));
  const query = controls.query();
  if (after) query.set("after", after);
  const result = useCachedApiRead(`/api/guilds/${guildId}/events?${query}`, "events", { listSize: controls.view.pageSize });
  const rows = result.data?.events;
  const names = result.data?.names || {};
  const next = result.data?.next_cursor;
  const eventTypes = Object.fromEntries([...new Set([...state.config.event_types, ...AUDIT_EXTRA_EVENT_TYPES])]
    .map((value) => [value, { label: value }]));
  const fallback = result.canceled
    ? "Loading canceled. Refresh to try again."
    : result.error
      ? "Could not load records: " + result.error.message
      : result.loading
        ? "Loading…"
        : rows?.length ? null : controls.filtered() ? "No events match these filters." : "No events recorded.";
  const renderRow = (record, i) => h(
    "tr",
    { key: record.id ?? i },
    h("td", { className: "muted" }, new Date(record.created_at).toLocaleString()),
    h("td", null, record.event_type),
    h("td", null, h(AuditPersonCell, {
      id: record.user_id, names, label: "user", filterLink: (id) => controls.filterLink({ subject: id }),
    })),
    h("td", null, h(AuditPersonCell, {
      id: record.actor_id, names, label: "actor", filterLink: (id) => controls.filterLink({ actor: id }),
    })),
    h("td", null, h(AuditEventDetails, { record, guildId })),
  );
  return h(
    "div",
    null,
    h("h2", null, "Audit log"),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      h(ActionButton, { type: "button", className: "btn", onClick: result.refresh }, "Refresh"),
      h(LastUpdated, { result }),
    ),
    h(ListControls, { controls, filterOptions: { event: eventTypes } }),
    h("div", { className: "row list-pagination" },
      h(ActionButton, { type: "button", className: "btn", disabled: !after || result.loading, onClick: () => setAfter("") }, "First page"),
      h(ActionButton, { type: "button", className: "btn", disabled: !next || !!fallback, onClick: () => setAfter(next) }, "Next page"),
    ),
    result.progress !== null ? h(ListLoadProgress, { loaded: result.progress, onCancel: result.cancel }) : null,
    h(PagedTable, {
      headings: [
        ...EVENTS_LIST.columns.map((column) => h(ListSortHeading, { key: column.key, column, controls })),
        "Details",
      ],
      className: "table table-zebra audit-table",
      rows: fallback ? null : rows,
      renderRow,
      fallback: fallback
        ? h("tr", null, h("td", {
          colSpan: EVENTS_LIST.columns.length + 1,
          className: result.error ? "err" : result.loading ? "" : "muted",
        }, fallback))
        : null,
    }),
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

function KnownSpamAddError({ draftKey }) {
  const error = useStore(ui, ({ drafts }) => drafts[draftKey]?.error);
  return error ? h("p", { className: "error", role: "alert" }, error) : null;
}

function tabKnownSpam() {
  const scopeRef = useRef(null);
  const guildId = state.guildId;
  const rows = state.config.known_spam || [];
  const rule = (state.config.rules || []).find((r) => r.rule_id === "known_spam");
  // The chosen form is view state, not an edit: it lives with the baselines, so it
  // survives the remount after each save and resets on leaving the page.
  const kind = useStore(ui, ({ draftBaselines }) => draftBaselines["known-spam:kind"] || "sample");
  const setKind = (value) =>
    ui.set({ draftBaselines: { ...ui.getSnapshot().draftBaselines, "known-spam:kind": value } });
  const add = async (kind) => {
    const key = kind === "sample" ? "known-spam:sample" : "known-spam:url";
    const drafts = currentDrafts();
    const value = drafts[key]?.value || "";
    if (!value.trim()) return toast("Enter a full sample or URL pattern", "error");
    const scope = scopeRef.current.value;
    const enrollment = state.config?.enrollment_id;
    const samePage = () => state.guildId === guildId && state.tab === "known-spam" &&
      state.config?.enrollment_id === enrollment;
    setKnownSpamAddError(key, null);
    try {
      await reloadAfter(async () => {
        const res = await api(`/api/guilds/${guildId}/known-spam`, "POST", {
          scope, kind, value, note: drafts["known-spam:note"]?.value || null,
        });
        // Acknowledged: keep this scope selected, without a draft, even if the refresh fails.
        if (samePage()) ui.set({ draftBaselines: { ...ui.getSnapshot().draftBaselines, "known-spam:scope": scope } });
        return res;
      }, () => toast("Known spam entry added", "success"), guildId);
    } catch (error) {
      // The browser cannot tell a rejected save from one whose outcome is unknown
      // or pending, so state only that the add was not confirmed.
      const label = kind === "sample" ? "Sample" : "URL pattern";
      const lookup = error?.saveLookup ? " Retrying the same entry, note and scope only looks up the earlier request; no new save was sent." : "";
      if (samePage()) setKnownSpamAddError(key, `${label} not confirmed as added: ${error?.message || error}${lookup}`);
      throw error;
    }
  };
  const sample = kind === "sample";
  return h("div", { className: "list-page" },
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
    h(RuleStatusCard, { record: rule, trigger: RULE_TRIGGERS.known_spam.phrase() }),
    canEditSettings() && h(DraftCard, { group: "known-spam", className: "card bg-base-100 card-border list-card list-add" },
      h("div", { className: "card-body" },
        h("h3", { className: "card-title" }, "Add spam"),
        h("div", { className: "list-form-row" },
          field("Entry type", h(Select, { className: "select", "data-no-draft": true, value: kind, onChange: (event) => setKind(event.target.value) },
            h("option", { value: "sample" }, "Full message sample"),
            h("option", { value: "url" }, "URL pattern"),
          )),
          field("Applies to", scopeSelect(scopeRef, "This server", "Global", true, "known-spam:scope")),
        ),
        sample
          ? h("div", { className: "list-entry" },
              h(Textarea, { className: "textarea", draftKey: "known-spam:sample", rows: 5, maxLength: 4096, "aria-label": "Full sample message", placeholder: "Paste the entire confirmed spam message. No regex needed." }),
              h("p", { className: "field-help" }, "Matches the same whole message, even with case, accent, spacing or leetspeak changes. A reworded copy may not match."),
              h("p", { className: "field-help entry-count" }, "Up to 4096 bytes."))
          : h("div", { className: "list-entry" },
              h(Input, { className: "input", draftKey: "known-spam:url", "data-url": true, maxLength: 512, "aria-label": "URL wildcard", placeholder: "tinyurl.com/*" }),
              h("p", { className: "field-help" }, "Use * as a wildcard, for example tinyurl.com/* or *.example.com/*."),
              h("p", { className: "field-help entry-count" }, "Up to 512 bytes."),
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
              })),
        h(ListFormSubmit, null,
          field("Note (optional, up to 512 bytes)", h(Input, { className: "input", draftKey: "known-spam:note", "data-note": true, maxLength: 512 })),
          sample
            ? h(ActionButton, { className: "btn btn-primary", savedFields: ["known-spam:sample", "known-spam:note", "known-spam:scope"], onClick: () => add("sample") }, "Add sample")
            : h(ActionButton, { className: "btn btn-primary", savedFields: ["known-spam:url", "known-spam:note", "known-spam:scope"], onClick: () => add("url") }, "Add URL pattern"),
        ),
        h(KnownSpamAddError, { draftKey: sample ? "known-spam:sample" : "known-spam:url" }),
      ),
    ),
    h(ListTable, {
      title: "Samples and URL patterns",
      rows,
      headings: ["Entry", "Type", "Scope", "Note", "Actions"],
      searchText: (row) => `${row.value} ${row.note || ""}`,
      searchLabel: "Search entries and notes",
      filters: [
        { key: "type", label: "Filter by type", all: "All types", options: [["sample", "Sample"], ["url", "URL pattern"]], value: (row) => (row.kind === "url" ? "url" : "sample") },
        SCOPE_FILTER,
      ],
      empty: "No known spam entries.",
      renderRow: (row) => h("tr", { key: row.id },
        h("td", { className: "list-entry-cell", style: { whiteSpace: "pre-wrap" } }, row.value),
        h("td", null, row.kind === "url" ? "URL pattern" : "Sample"),
        h("td", null, scopeName(row)),
        h("td", { className: "muted" }, row.note || "—"),
        h("td", null, canEditSettings() && (String(row.guild_id) !== "0" || isBotOwner() ? h(IconButton, { action: "remove", label: "Remove",
          onClick: async () => {
            if (!confirm("Remove this known spam entry?")) return;
            await reloadAfter(() => api(`/api/guilds/${guildId}/known-spam/${row.id}`, "DELETE"), undefined, guildId);
          },
        }) : h("span", { className: "muted" }, "Owner only"))),
      ),
    }),
    h("details", { className: "card bg-base-100 card-border list-card list-extra" },
      h("summary", null, "Paste-as-sample examples (not active until saved)"),
      h("div", { className: "card-body" },
        h("p", null, "Copy the full owner sample you want to save into the sample field. These examples are not preloaded entries."),
        h("pre", { style: { whiteSpace: "pre-wrap" } }, STOCKS_SPAM_EXAMPLE),
        h("pre", { style: { whiteSpace: "pre-wrap" } }, AI_SPAM_EXAMPLE),
      ),
    ),
  );
}

// The toggles here run the whole Exact image spam rule, so the summary names both of its
// triggers: a listed image, and the same images posted across channels.
function knownBadTrigger(rule) {
  const shown = {};
  for (const f of RULE_TRIGGERS.attachment_flood.fields) shown[f.key] = String(rule.params?.[f.key] ?? f.fallback);
  return `When someone posts a listed image, or ${RULE_TRIGGERS.attachment_flood.phrase(shown).replace(/^When someone /, "")}`;
}

function AddKnownBadImage({ guildId }) {
  const validHash = useStore(ui, ({ drafts }) => /^[a-fA-F0-9]{64}$/.test(drafts["known-bad:hash"]?.value ?? ""));
  return h(ActionButton, {
    className: "btn btn-primary",
    disabled: !validHash,
    savedFields: ["known-bad:hash", "known-bad:note"],
    onClick: async () => {
      const drafts = currentDrafts();
      const hash = drafts["known-bad:hash"]?.value ?? "";
      const note = drafts["known-bad:note"]?.value ?? "";
      await reloadAfter(
        () => api(`/api/guilds/${guildId}/known-bad-images`, "POST", { hash, note: note || null }),
        (result) => toast(result.changed ? "Hash added." : "Hash already listed.", "success"),
        guildId,
      );
    },
  }, "Add hash");
}

function tabKnownBadImages() {
  const guildId = state.guildId;
  const rows = state.config.known_bad_images || [];
  const rule = (state.config.rules || []).find((r) => r.rule_id === "attachment_flood");
  if (guildId === "0") return h("p", { className: "muted" }, "Known-bad images are managed per server. Select a server.");
  return h("div", { className: "list-page" },
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
    h(RuleStatusCard, { record: rule, trigger: rule ? knownBadTrigger(rule.effective) : "" }),
    canEditSettings() && h(
      DraftCard,
      { group: "known-bad-images", className: "card bg-base-100 card-border list-card list-add" },
      h(
        "div",
        { className: "card-body" },
        h("h3", { className: "card-title" }, "Add an image"),
        h("div", { className: "list-entry" },
          field("SHA-256 digest", h(Input, {
            className: "input mono", draftKey: "known-bad:hash", "data-hash": true, maxLength: 64,
          })),
          h("p", { className: "field-help" }, "Copy it from an exact-image notice in your log channel."),
        ),
        h(ListFormSubmit, null,
          field("Note (optional)", h(Input, {
            className: "input", draftKey: "known-bad:note", "data-note": true, maxLength: 256,
          })),
          h(AddKnownBadImage, { guildId }),
        ),
      ),
    ),
    h(ListTable, {
      title: "Images",
      rows,
      headings: ["SHA-256", "Added", "Source", "Actor", "Note", "Actions"],
      searchText: (row) => `${row.hash} ${row.note || ""} ${row.added_via} ${row.actor}`,
      searchLabel: "Search hashes or notes",
      empty: "No images yet. Add a hash above.",
      renderRow: (row) => h("tr", { key: row.hash },
        h("td", { className: "list-entry-cell mono" }, row.hash),
        h("td", null, row.added_at), h("td", null, row.added_via),
        h("td", null, row.actor), h("td", { className: "muted" }, row.note || "—"),
        h("td", null, canEditSettings() && h(IconButton, { action: "remove", label: "Remove",
          onClick: async () => {
            if (!confirm(`Remove SHA-256 ${row.hash}?`)) return;
            await reloadAfter(() => api(`/api/guilds/${guildId}/known-bad-images/${row.hash}`, "DELETE"), undefined, guildId);
          },
        })),
      ),
    }),
  );
}

// Bans page (#625). Rows come from the ban records; display names come from the
// row or the member directory, and IDs only appear as copyable chips.
const AUTOMOD_BAN_REASON = /^\[automod:([^\]]+)\]\s*/;
// Texts the bot stores in place of a reason: Discord's audit entry had none, or
// no audit entry named the ban yet. Both show as a muted placeholder.
const BAN_REASON_PLACEHOLDERS = { "No reason provided": "No reason given", "Reason unavailable": "Reason unavailable" };

function banReasonParts(reason) {
  const raw = String(reason || "");
  const automod = AUTOMOD_BAN_REASON.exec(raw);
  const rest = (automod ? raw.slice(automod[0].length) : raw).trim();
  const placeholder = Object.hasOwn(BAN_REASON_PLACEHOLDERS, rest) ? BAN_REASON_PLACEHOLDERS[rest] : rest ? null : "No reason given";
  return { rule: automod ? automod[1] : null, text: placeholder ? "" : rest, placeholder };
}

function banPerson(id, storedName, storedDisplay, names) {
  const known = id && names && Object.hasOwn(names, id) ? names[id] : null;
  const handle = storedName || known?.username || null;
  return { id, name: storedDisplay || known?.display_name || handle, handle };
}

// The one place a ban names its user: "Name (@handle)", else "user <id>".
function banPersonText(person) {
  if (!person.name) return `user ${person.id}`;
  return person.handle && person.handle !== person.name ? `${person.name} (@${person.handle})` : person.name;
}

// A small ID chip with the shared copy icon action (#588), never inline ID text.
function IdChip({ id, label }) {
  return h(
    "span",
    { className: "id-chip" },
    h("code", null, id),
    h(IconButton, {
      action: "copy",
      label: `Copy ${label} ID ${id}`,
      className: "btn btn-xs btn-square btn-ghost",
      onClick: async () => {
        try {
          await navigator.clipboard.writeText(id);
          toast("ID copied", "success");
        } catch (_) {
          toast("Could not copy the ID.", "error");
        }
      },
    }),
  );
}

function BanPersonCell({ person, label, filterLink, showId = true }) {
  return h(
    "div",
    { className: "ban-person" },
    h(LetterAvatar, { name: person.name || "?" }),
    h(
      "div",
      { className: "ban-person-text" },
      h("strong", { className: "ban-person-name" }, person.id && filterLink
        ? h("a", { ...filterLink(String(person.id)), className: "list-person-filter",
          "aria-label": `Filter by ${label} ${banPersonText(person)}` }, person.name || "Unknown user")
        : person.name || "Unknown user"),
      person.handle && person.handle !== person.name ? h("span", { className: "muted" }, `@${person.handle}`) : null,
      showId ? h(IdChip, { id: person.id, label }) : null,
    ),
  );
}

function BanModeratorCell({ record, names, rule, filterLink }) {
  if (record.moderator_id) {
    const moderator = h(BanPersonCell, { person: banPerson(record.moderator_id, record.moderator_name, null, names), label: "moderator", filterLink });
    // The reason cell drops the automod prefix, so its rule is named here
    // beside the person who acted rather than replacing them.
    return rule
      ? h("div", { className: "ban-moderator" }, moderator, h("span", { className: "muted ban-moderator-rule" }, `jModGuard (automod: ${rule})`))
      : moderator;
  }
  return h(
    "div",
    { className: "ban-person" },
    h("span", { className: "case-avatar", "aria-hidden": "true" }, h(Icon, { name: rule ? "bot" : "users" })),
    rule
      ? h("strong", { className: "ban-person-name" }, `jModGuard (automod: ${rule})`)
      : h("span", { className: "muted" }, "Unknown"),
  );
}

// Two lines until clicked; a long reason never stretches the row. A disclosure,
// not a row action, so it is the text itself rather than an icon button.
function BanReasonText({ text }) {
  const [open, setOpen] = useState(false);
  return h(
    "span",
    {
      role: "button",
      tabIndex: 0,
      className: `ban-reason${open ? " ban-reason-open" : ""}`,
      "aria-expanded": open,
      title: open ? "Show less" : "Show the whole reason",
      onClick: () => setOpen(!open),
      onKeyDown: (event) => {
        if (event.key !== "Enter" && event.key !== " ") return;
        event.preventDefault();
        setOpen(!open);
      },
    },
    text,
  );
}

// The Bans page holds its one open reason editor ({ id, draft }) so a list reload keeps the
// draft, and the page's single unsaved-changes flag belongs to that one editor.
function BanReasonCell({ record, names, guildId, editor: reasonEditor, onEdit, onDraft, onClose, onSaved }) {
  // Like the shared draft controls: no typing while a save is in flight, so a
  // newer draft cannot be closed and dropped when the earlier save completes.
  const working = useContext(BusyContext);
  const editing = reasonEditor?.id === record.id;
  const draft = editing ? reasonEditor.draft : "";
  const parts = banReasonParts(record.reason);
  const current = record.edited_reason != null ? record.edited_reason : parts.text;
  if (editing) {
    return h(
      "div",
      { className: "ban-reason-editor" },
      h("textarea", noAutofill({
        className: "textarea textarea-sm",
        "aria-label": "jModGuard reason",
        maxLength: 512,
        rows: 2,
        value: draft,
        autoFocus: true,
        disabled: working,
        onChange: (event) => {
          onDraft(event.target.value.replace(/[\r\n]+/g, " "));
          markDirty();
        },
      })),
      h("p", { className: "field-help" }, "Changes jModGuard's record only. Discord keeps the original reason."),
      h(
        "div",
        { className: "row" },
        h(
          ActionButton,
          {
            type: "button",
            className: "btn btn-sm btn-primary",
            disabled: !draft.trim(),
            onClick: async () => {
              const result = await api(`/api/guilds/${guildId}/bans/${record.id}/reason`, "PUT", { reason: draft.trim() });
              onClose();
              toast(result?.changed === false ? "Reason unchanged." : "Reason updated.", "success");
              onSaved();
            },
          },
          "Save reason",
        ),
        h(ActionButton, { type: "button", className: "btn btn-sm", onClick: onClose }, "Cancel"),
      ),
    );
  }
  const editor = record.reason_edited_by
    ? banPerson(record.reason_edited_by, record.reason_edited_by === record.moderator_id ? record.moderator_name : null, null, names)
    : null;
  return h(
    "div",
    { className: "ban-reason-cell" },
    current ? h(BanReasonText, { text: current }) : h("span", { className: "muted" }, parts.placeholder || "No reason given"),
    record.edited_reason != null
      ? h(
          "p",
          { className: "ban-reason-history muted" },
          `Edited${editor ? ` by ${banPersonText(editor)}` : ""} `,
          h(CaseTime, { ms: memberInstant(record.reason_edited_at)?.getTime() }),
          h("br"),
          `Original: ${parts.text || (parts.placeholder || "No reason given").toLowerCase()}`,
        )
      : null,
    canBanMembers()
      ? h(IconButton, {
          action: "edit",
          label: "Edit reason",
          className: "btn btn-xs btn-square btn-ghost",
          onClick: () => onEdit(current || ""),
        })
      : null,
  );
}

// Readable bot-action results for ban and unban receipts, here and in Bot actions.
const BAN_ACTION_OUTCOMES = {
  banned: "Banned in Discord.",
  unbanned: "Unbanned in Discord.",
  already_unbanned: "Already unbanned in Discord.",
  cannot_ban_owner: "Refused: the server owner can't be banned.",
  cannot_ban_bot: "Refused: jModGuard can't ban itself.",
  bot_missing_ban_permission: "Refused: jModGuard needs the Ban Members permission.",
  bot_missing_permissions: "Discord refused: jModGuard lacks Ban Members or its role is too low.",
  target_role_not_below_bot: "Refused: this member's highest role is not below jModGuard's.",
  target_role_not_below_moderator: "Refused: this member's highest role is not below yours.",
  unknown_user: "Refused: Discord has no user with this ID.",
  ban_in_progress: "Refused: another ban or unban for this user was in progress. Try again.",
  permission_revoked: "Refused: you no longer have permission for this action.",
  permission_check_unavailable: "Discord couldn't confirm your permissions. Try again.",
  ban_check_unavailable: "Discord couldn't be checked before the ban. Try again.",
  discord_rejected: "Discord rejected the request.",
  discord_outcome_unknown: "Discord's answer was lost. Check Discord before trying again.",
  request_timed_out: "Discord didn't answer in time. Check Discord before trying again.",
  expired_before_execution: "The bot didn't pick this up in time. It may be offline.",
};

function botActionOutcomeText(job) {
  return (job.outcome && BAN_ACTION_OUTCOMES[job.outcome]) || job.outcome || null;
}

function banReceiptText(job) {
  if (job.state === "succeeded") return botActionOutcomeText(job) || "Banned in Discord.";
  if (job.state === "pending" || job.state === "claimed") return "Queued. Waiting for the bot to pick it up…";
  if (job.state === "running") return "The bot is banning this user…";
  if (job.state === "uncertain") return botActionOutcomeText(job) || "The result is unknown. Check Discord.";
  return botActionOutcomeText(job) || `Ban ${job.state}.`;
}

// The dialog follows its own receipt for a bounded time, like a log-channel save.
const BAN_RECEIPT_POLLS = 30;
const BAN_RECEIPT_POLL_MS = 2000;

function BanDialog({ guildId, onDone }) {
  const [userIds] = useState(() => []);
  const [, bump] = useState(0);
  const [pickerNames, setPickerNames] = useState(() => ({ status: "loaded", roles: [], channels: [], users: new Map() }));
  const [reason, setReason] = useState("");
  const [days, setDays] = useState("0");
  const [duration, setDuration] = useState("");
  const [receipt, setReceipt] = useState(null);
  const pending = useRef(new Set()).current;
  const alive = useRef(true);
  useEffect(() => () => {
    alive.current = false;
  }, []);
  const rememberUser = (id, name) => setPickerNames((current) => ({ ...current, users: new Map(current.users).set(id, name) }));
  const userId = userIds[0] || "";
  const who = userId ? pickerNames.users.get(userId) || `user ${userId}` : "";
  const minutes = duration.trim() ? Number(duration) : null;
  const durationValid = minutes === null || (Number.isInteger(minutes) && minutes > 0 && minutes <= 4294967295);
  const close = () => {
    if (canLeave()) closeOverlay();
  };
  const watch = async (job) => {
    for (let poll = 0; poll < BAN_RECEIPT_POLLS && alive.current && botActionPhase(job) === "active"; poll++) {
      await new Promise((resolve) => setTimeout(resolve, BAN_RECEIPT_POLL_MS));
      if (!alive.current) return;
      try {
        const jobs = await apiRead(`/api/guilds/${guildId}/actions`);
        const next = Array.isArray(jobs) ? jobs.find((candidate) => candidate.id === job.id) : null;
        if (!next) return;
        job = next;
        if (alive.current) setReceipt(job);
      } catch (_) {
        return;
      }
    }
    if (alive.current && botActionPhase(job) !== "active") onDone();
  };
  const submit = async () => {
    for (const flush of pending) if (!flush()) return;
    const id = userIds[0];
    if (!id) throw new Error("Choose a member or paste a user ID.");
    if (!durationValid) throw new Error("Enter the duration as a whole number of minutes, or leave it empty.");
    if (!confirm(`Ban ${pickerNames.users.get(id) || `user ${id}`} from this server? This runs in Discord.`)) return;
    const job = await submitBotAction(`/api/guilds/${guildId}/ban`, "POST", {
      user_id: id,
      reason: reason.trim() || null,
      delete_message_days: Number(days),
      ...(minutes === null ? {} : { duration_minutes: minutes }),
    });
    ui.set({ edits: "clean" });
    setReceipt(job);
    if (botActionPhase(job) === "active") watch(job);
    else onDone();
  };
  return h(
    DialogPanel,
    { className: "modal-box ban-dialog" },
    h(
      "header",
      { className: "case-header" },
      h(
        "div",
        { className: "case-title" },
        h("h2", null, "Ban a user"),
        h(IconButton, { action: "cancel", label: "Close", className: "btn btn-sm btn-square btn-ghost case-close", onClick: close }),
      ),
    ),
    receipt
      ? h(
          "div",
          { className: "ban-receipt", role: "status", "aria-live": "polite" },
          h(
            "p",
            { className: receipt.state === "succeeded" ? "" : ["failed", "uncertain", "cancelled", "expired"].includes(receipt.state) ? "err" : "muted" },
            h("strong", null, who ? `${who}: ` : ""),
            banReceiptText(receipt),
          ),
          h("p", { className: "field-help" }, "Bot actions keeps this result."),
          h("div", { className: "row" }, h(ActionButton, { type: "button", className: "btn btn-sm", onClick: closeOverlay }, "Close")),
        )
      : h(
          "div",
          { className: "ban-form" },
          h(IdPicker, {
            kind: "user",
            label: "Member",
            ids: userIds,
            names: pickerNames,
            rememberUser,
            single: true,
            pending,
            idLabel: "User ID",
            hint: "Search this server's members, or paste the ID of someone who isn't in it.",
            onChange: () => bump((n) => n + 1),
          }),
          field(
            "Reason",
            h(Input, {
              type: "text",
              className: "input",
              maxLength: 512,
              value: reason,
              placeholder: "Shown in Discord's audit log",
              onChange: (event) => {
                setReason(event.target.value);
                markDirty();
              },
            }),
          ),
          field(
            "Delete their messages from",
            h(
              Select,
              { className: "select", value: days, onChange: (event) => setDays(event.target.value) },
              h("option", { value: "0" }, "Don't delete any"),
              h("option", { value: "1" }, "The last day"),
              h("option", { value: "3" }, "The last 3 days"),
              h("option", { value: "7" }, "The last 7 days"),
            ),
          ),
          field(
            "Unban after (minutes, optional)",
            h(Input, {
              type: "text",
              inputMode: "numeric",
              className: `input${durationValid ? "" : " input-error"}`,
              value: duration,
              placeholder: "Leave empty for a permanent ban",
              onChange: (event) => {
                setDuration(event.target.value);
                markDirty();
              },
            }),
          ),
          h(
            "div",
            { className: "row" },
            h(ActionButton, { type: "button", className: "btn btn-error", disabled: !durationValid, onClick: submit }, h(Icon, { name: "hammer" }), "Ban user"),
            h(ActionButton, { type: "button", className: "btn", onClick: close }, "Cancel"),
          ),
        ),
  );
}

function openBanDialog(guildId, onDone) {
  ui.set({ editor: h(BanDialog, { guildId, onDone, dialogLabel: "Ban a user", key: `ban:${guildId}:${Date.now()}` }) });
}

function tabBans() {
  const guildId = state.guildId;
  const [reasonEditor, setReasonEditor] = useState(null);
  const [after, setAfter] = useState("");
  const [moderators, setModerators] = useState({});
  // Closing the open reason editor for another task asks before discarding its draft.
  const leaveReasonEditor = () => {
    if (!reasonEditor) return true;
    if (!canLeave()) return false;
    setReasonEditor(null);
    ui.set({ edits: "clean" });
    return true;
  };
  const closeReasonEditor = () => {
    setReasonEditor(null);
    ui.set({ edits: "clean" });
  };
  const controls = useListControls(BANS_LIST, guildId, () => setAfter(""), leaveReasonEditor);
  const query = controls.query();
  if (after) query.set("after", after);
  const result = useCachedApiRead(`/api/guilds/${guildId}/bans?${query}`, "bans", {
    listSize: controls.view.pageSize, holdBackgroundResult: reasonEditor !== null,
  });
  const rows = Array.isArray(result.data?.bans) ? result.data.bans : [];
  const names = result.data?.names || {};
  const availableModerators = result.data?.moderators;
  useEffect(() => {
    if (result.accessFailed) setModerators({});
    else if (Array.isArray(availableModerators)) setModerators(Object.fromEntries(availableModerators.map(({ id, name }) =>
      [id, { label: name ? `${name} (${id})` : id }],
    )));
  }, [availableModerators, result.accessFailed]);
  const next = result.data?.next_cursor;
  const changePage = (cursor) => {
    if (leaveReasonEditor()) setAfter(cursor);
  };
  const renderRow = (record, i) => {
    const user = banPerson(record.user_id, record.user_name, record.user_display_name, names);
    const { rule } = banReasonParts(record.reason);
    return h(
      "tr",
      { key: record.id ?? i },
      h("td", null, h(BanPersonCell, { person: user, label: "user", showId: false,
        filterLink: (id) => controls.filterLink({ subject: id }) })),
      h("td", null, h(IdChip, { id: user.id, label: "user" })),
      h(
        "td",
        null,
        h(
          "span",
          { className: `badge badge-soft badge-sm ${record.active ? "badge-error" : "badge-success"}` },
          record.active ? "Banned" : "Unbanned",
        ),
      ),
      h("td", null, h(BanModeratorCell, { record, names, rule, filterLink: (id) => controls.filterLink({ actor: id }) })),
      h(
        "td",
        null,
        h(BanReasonCell, {
          record,
          names,
          guildId,
          editor: reasonEditor,
          onEdit: (draft) => leaveReasonEditor() && setReasonEditor({ id: record.id, draft }),
          onDraft: (draft) => setReasonEditor((current) => (current?.id === record.id ? { ...current, draft } : current)),
          onClose: closeReasonEditor,
          onSaved: result.refresh,
        }),
      ),
      h("td", { className: "muted" }, h(CaseTime, { ms: memberInstant(record.created_at)?.getTime() })),
      h(
        "td",
        null,
        record.active && canBanMembers() &&
          h(IconButton, {
            action: "unban",
            label: `Unban ${banPersonText(user)}`,
            onClick: async () => {
              if (confirm(`Unban ${banPersonText(user)}?`))
                await submitBotAction(`/api/guilds/${guildId}/unban`, "POST", {
                  user_id: String(record.user_id),
                  reason: "unbanned via WebUI",
                });
            },
          }),
      ),
    );
  };
  const fallback = result.canceled
    ? h("tr", null, h("td", { colSpan: 7, role: "status" }, "Loading canceled. Refresh to try again."))
    : result.loading || result.error
      ? h("tr", null, h("td", { colSpan: 7, className: result.error ? "err" : "", role: result.error ? "alert" : undefined },
          result.error ? "Could not load records: " + result.error.message : "Loading…"))
      : !rows.length
        ? h(ListEmptyRow, { controls, colSpan: 7, empty: "No bans recorded.", filtered: "No bans match these filters." })
        : null;
  return h(
    "div",
    null,
    h("h2", null, "Bans"),
    h("p", { className: "muted" }, "Bans the bot made or saw in Discord."),
    h(GoodToKnow, {
      items: [
        "Bans and unbans you request here run in Discord through the bot. Bot actions shows the result.",
        "Editing a reason changes jModGuard's record only. Discord keeps the original, and the Audit log lists each edit.",
      ],
    }),
    h(
      "div",
      { className: "row", style: { marginBottom: "12px" } },
      canBanMembers() && h(
        ActionButton,
        { type: "button", className: "btn btn-error", onClick: () => leaveReasonEditor() && openBanDialog(guildId, result.refresh) },
        h(Icon, { name: "hammer" }),
        "Ban user",
      ),
      h(ActionButton, { type: "button", className: "btn", onClick: result.refresh }, "Refresh"),
      h(LastUpdated, { result }),
    ),
    h(ListControls, { controls, filterOptions: { actor: result.accessFailed ? {} : moderators } }),
    h("div", { className: "row list-pagination" },
      h(ActionButton, { type: "button", className: "btn", disabled: !after || result.loading, onClick: () => changePage("") }, "First page"),
      h(ActionButton, { type: "button", className: "btn", disabled: !next || !!fallback, onClick: () => changePage(next) }, "Next page"),
      Number.isInteger(result.data?.total)
        ? h("span", { className: "muted", role: "status" }, `${result.data.total.toLocaleString()} matching bans`)
        : null,
    ),
    result.progress !== null ? h(ListLoadProgress, { loaded: result.progress, onCancel: result.cancel }) : null,
    h(PagedTable, {
      headings: [
        ...BANS_LIST.columns.map((column) => h(ListSortHeading, { key: column.key, column, controls })),
        "Actions",
      ],
      className: "table table-zebra ban-table",
      rows: fallback ? null : rows,
      renderRow,
      fallback,
      canMove: leaveReasonEditor,
    }),
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
                  : job.action.kind === "ban"
                    ? `Ban ${job.action.user_id}`
                    : job.action.kind === "member_sync"
                      ? `Member sync: ${job.action.operation}`
                      : `${job.action.event}: ${job.action.channel_id || "clear log channel"}`,
              ),
              h("td", null, job.actor_id),
              h("td", null, job.state === "uncertain" ? "Outcome unknown" : job.state),
              h("td", null, botActionOutcomeText(job) || "—"),
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
