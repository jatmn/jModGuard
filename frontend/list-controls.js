import React, { useEffect, useRef, useState } from "react";
import { Icon } from "./icons.js";
import { noAutofill } from "./no-autofill.js";

const h = React.createElement;
const SEARCH_LIMIT = 200;
const SEARCH_DELAY_MS = 300;
const savedListLocation = "jmodguard.list-before-login";
const commonKeys = ["q", "from", "to"];
const PAGE_SIZES = ["50", "100", "250", "500", "1000", "all"];
const REQUEST_PAGE_SIZE = 250;
const PAGE_TIMEOUT_MS = 30000;
let pendingListSearch = null;

function filterKeys(config) {
  const keys = config.filters.map(({ key }) => key);
  return config.commonFilters === false ? keys : [...keys, ...commonKeys];
}

// Filter keys older links used, mapped to their current key.
function legacyFilterKeys(config) {
  return Object.keys(config.legacyFilters ?? {});
}

function emptyFilters(config) {
  return Object.fromEntries(filterKeys(config).map((key) => [key, ""]));
}

function validCaseId(id) {
  return /^[a-f0-9]{32}$/.test(id ?? "");
}

export function readListLocation(query = new URLSearchParams(location.search)) {
  const list = query.get("list");
  const guild = query.get("guild");
  const caseId = query.get("case");
  return { list, guild, caseId: list === "cases" && guild && validCaseId(caseId) ? caseId : null };
}

export function flushPendingListSearch(list, guildId) {
  if (pendingListSearch?.list === list && pendingListSearch.guildId === guildId) {
    pendingListSearch.flush();
  }
}

function listLocation(query) {
  const search = query.toString();
  return location.pathname + (search ? "?" + search : "") + location.hash;
}

function replaceListLocation(query) {
  history.replaceState(history.state, "", listLocation(query));
}

export function setCaseListLocation(guildId, caseId) {
  if (!guildId || !validCaseId(caseId)) return;
  const query = new URLSearchParams(location.search);
  query.set("list", "cases");
  query.set("guild", guildId);
  query.set("case", caseId);
  replaceListLocation(query);
}

// OAuth returns to the workspace root. Preserve a list link in this browser tab.
export function rememberListLocation(configs) {
  if (!configs.has(readListLocation().list)) return;
  try {
    window.sessionStorage.setItem(savedListLocation, location.search);
  } catch (_) {}
}

export function restoreListLocation(configs, guilds) {
  const current = readListLocation();
  try {
    const remembered = new URLSearchParams(window.sessionStorage.getItem(savedListLocation));
    window.sessionStorage.removeItem(savedListLocation);
    const { list, guild, caseId } = readListLocation(remembered);
    const config = configs.get(list);
    if (!current.list && config && guilds.some(({ id }) => id === guild)) {
      const restored = new URLSearchParams(location.search);
      for (const key of ["list", "guild", "sort", "dir", "page_size", ...filterKeys(config), ...legacyFilterKeys(config)]) {
        if (remembered.has(key)) restored.set(key, remembered.get(key));
      }
      if (caseId) restored.set("case", caseId);
      else restored.delete("case");
      replaceListLocation(restored);
      return { list, guild, caseId };
    }
  } catch (_) {}
  return current;
}

export function clearListLocation(config) {
  const query = new URLSearchParams(location.search);
  if (query.get("list") !== config.list) return;
  for (const key of ["list", "guild", "case", "sort", "dir", "page_size", ...filterKeys(config), ...legacyFilterKeys(config)]) {
    query.delete(key);
  }
  replaceListLocation(query);
}

// Leave list `from` for list `config` with only these filters. The location is
// restored when `open` declines, for example when the user keeps unsaved changes.
export function openFilteredList(from, config, guildId, filters, open) {
  const previous = location.search;
  clearListLocation(from);
  replaceListLocation(viewQuery(config, guildId, {
    filters: { ...emptyFilters(config), ...filters },
    sort: config.defaultSort,
    dir: config.defaultDir,
    pageSize: preferredPageSize(config),
  }));
  if (open()) return true;
  replaceListLocation(new URLSearchParams(previous));
  return false;
}

function boundedSearch(value) {
  return Array.from(value).slice(0, SEARCH_LIMIT).join("");
}

function pageSizePreference(config) {
  return `jmodguard.list-page-size.${config.list}`;
}

function preferredPageSize(config) {
  try {
    const stored = window.localStorage.getItem(pageSizePreference(config));
    if (PAGE_SIZES.includes(stored)) return stored;
  } catch (_) {}
  return PAGE_SIZES[0];
}

function readView(config, guildId) {
  const query = new URLSearchParams(location.search);
  const filters = emptyFilters(config);
  const initial = { filters, sort: config.defaultSort, dir: config.defaultDir, pageSize: preferredPageSize(config) };
  if (query.get("list") !== config.list || query.get("guild") !== guildId) return initial;
  for (const key of filterKeys(config)) filters[key] = query.get(key) || "";
  for (const [legacy, key] of Object.entries(config.legacyFilters ?? {})) {
    filters[key] ||= query.get(legacy) || "";
  }
  const textKeys = config.filters.filter(({ kind }) => kind === "text").map(({ key }) => key);
  if (config.commonFilters !== false) textKeys.push("q");
  for (const key of textKeys) {
    filters[key] = boundedSearch(filters[key].trim());
  }
  const sort = query.get("sort");
  const dir = query.get("dir");
  const pageSize = query.get("page_size");
  return {
    filters,
    sort: config.columns.some((column) => column.key === sort) ? sort : initial.sort,
    dir: dir === "asc" || dir === "desc" ? dir : initial.dir,
    pageSize: PAGE_SIZES.includes(pageSize) ? pageSize : initial.pageSize,
  };
}

function viewQuery(config, guildId, view) {
  const query = new URLSearchParams(location.search);
  const link = readListLocation(query);
  // A case link belongs to the Cases list for its original server.
  if (link.list !== config.list || link.guild !== guildId || !link.caseId) query.delete("case");
  query.set("list", config.list);
  query.set("guild", guildId);
  query.set("sort", view.sort);
  query.set("dir", view.dir);
  query.set("page_size", view.pageSize);
  for (const key of legacyFilterKeys(config)) query.delete(key);
  for (const [key, value] of Object.entries(view.filters)) {
    if (value) query.set(key, value);
    else query.delete(key);
  }
  return query;
}

function hasFilters(view) {
  return Object.values(view.filters).some(Boolean);
}

export function useListControls(config, guildId, onChange, canChange = () => true, onSearchInput) {
  const [view, setView] = useState(() => readView(config, guildId));
  const current = useRef(view);
  const guard = useRef(canChange);
  guard.current = canChange;
  const [drafts, setDrafts] = useState({});
  const timers = useRef(new Map());
  const cancelText = (key) => {
    const timer = timers.current.get(key);
    clearTimeout(timer);
    if (pendingListSearch?.timer === timer) pendingListSearch = null;
    timers.current.delete(key);
  };
  const cancelTexts = () => {
    for (const key of timers.current.keys()) cancelText(key);
  };
  useEffect(() => cancelTexts, []);
  useEffect(() => {
    // Include a non-default saved preference in links copied before any interaction.
    if (current.current.pageSize !== PAGE_SIZES[0] && guard.current()) {
      replaceListLocation(viewQuery(config, guildId, current.current));
    }
  }, [config, guildId]);

  const commit = (next) => {
    if (!guard.current()) return false;
    current.current = next;
    setView(next);
    replaceListLocation(viewQuery(config, guildId, next));
    onChange();
    return true;
  };
  const setPageSize = (pageSize) => {
    const previous = current.current;
    if (!PAGE_SIZES.includes(pageSize) || previous.pageSize === pageSize) return;
    if (commit({ ...previous, pageSize })) {
      try { window.localStorage.setItem(pageSizePreference(config), pageSize); } catch (_) {}
    }
  };
  const setFilter = (key, value, reload = false) => {
    cancelText(key);
    setDrafts((previous) => {
      const remaining = { ...previous };
      delete remaining[key];
      return remaining;
    });
    const previous = current.current;
    if (previous.filters[key] === value && !reload) return;
    commit({ ...previous, filters: { ...previous.filters, [key]: value } });
  };
  const withFilters = (filters) => ({ ...current.current, filters: { ...emptyFilters(config), ...filters } });
  const replaceFilters = (filters = {}) => {
    cancelTexts();
    setDrafts({});
    commit(withFilters(filters));
  };
  const filterLink = (filters) => ({
    href: listLocation(viewQuery(config, guildId, withFilters(filters))),
    onClick: (event) => {
      if (event.button || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
      event.preventDefault();
      replaceFilters(filters);
    },
  });
  const setText = (key, value) => {
    const text = boundedSearch(value);
    const trimmed = text.trim();
    setDrafts((previous) => ({ ...previous, [key]: text }));
    cancelText(key);
    const reload = key === "q" && !!onSearchInput;
    if (reload) onSearchInput();
    const timer = setTimeout(() => setFilter(key, trimmed, reload), SEARCH_DELAY_MS);
    timers.current.set(key, timer);
    if (key === "q") {
      pendingListSearch = {
        list: config.list, guildId, timer,
        flush: () => {
          cancelText(key);
          const previous = current.current;
          replaceListLocation(viewQuery(config, guildId, {
            ...previous, filters: { ...previous.filters, q: trimmed },
          }));
        },
      };
    }
  };
  const sortBy = (column) => {
    const previous = current.current;
    const dir = previous.sort === column.key
      ? (previous.dir === "asc" ? "desc" : "asc")
      : column.initialDir || "asc";
    commit({ ...previous, sort: column.key, dir });
  };
  const query = () => {
    const { filters, sort, dir } = current.current;
    return new URLSearchParams({ sort, dir, ...Object.fromEntries(Object.entries(filters).filter(([, value]) => value)) });
  };
  return {
    config, view, drafts, setText, setFilter, replaceFilters, filterLink, sortBy, query, setPageSize,
    pageSize: () => current.current.pageSize,
    filtered: () => hasFilters(current.current),
  };
}

function ListField({ label, children }) {
  return h("div", { className: "field" }, h("label", null, label, children));
}

export function ListPageSize({ controls }) {
  return h(ListField, { label: "Rows per page" },
    h("select", {
      className: "select select-sm",
      value: controls.view.pageSize,
      onChange: (event) => controls.setPageSize(event.target.value),
    }, ...PAGE_SIZES.map((size) => h("option", { key: size, value: size }, size === "all" ? "All" : size))),
  );
}

export function ListControls({ controls, UserPicker, userPickerProps, filterOptions = {} }) {
  const { config, view, drafts, setText, setFilter, replaceFilters } = controls;
  const { filters } = view;
  const { from: fromLabel = "From", to: toLabel = "To" } = config.dateLabels ?? {};
  const date = (key, label, bounds) => h(ListField, { label },
    h("input", noAutofill({
      className: "input input-sm",
      type: "date",
      value: filters[key],
      ...bounds,
      onChange: (event) => setFilter(key, event.target.value),
    })),
  );
  const text = (key, label, placeholder, type = "text") => h(ListField, { key, label },
    h("input", noAutofill({
      className: "input input-sm",
      type,
      value: drafts[key] ?? filters[key],
      placeholder,
      maxLength: SEARCH_LIMIT * 2,
      onChange: (event) => setText(key, event.target.value),
    })),
  );
  const chips = [
    ...config.filters.filter(({ kind }) => kind !== "user"),
    { key: "q", label: "Search" },
    { key: "from", label: fromLabel },
    { key: "to", label: toLabel },
  ].filter(({ key }) => filters[key]).map(({ key, label, options }) => {
    const value = (filterOptions[key] ?? options)?.[filters[key]]?.label ?? filters[key];
    return h("button", {
      key, type: "button", className: "btn btn-xs btn-outline",
      "aria-label": `Remove ${label.toLowerCase()} filter`,
      onClick: () => setFilter(key, ""),
    }, `${label}: ${value}`, " ×");
  });
  return h("div", { className: "list-filters", "data-no-draft": true, role: "group", "aria-label": config.filterLabel },
    ...config.filters.map(({ key, label, options, kind, idLabel, render, placeholder, emptyLabel }) => {
      if (kind === "user") {
        const ids = filters[key] ? [filters[key]] : [];
        return h(UserPicker, {
          ...userPickerProps,
          key, label, idLabel, ids,
          kind: "user",
          single: true,
          draft: false,
          onChange: () => setFilter(key, ids[0] || ""),
        });
      }
      if (kind === "text") return text(key, label, placeholder);
      if (render) return h(ListField, { key, label }, render(filters[key], (value) => setFilter(key, value)));
      return h(ListField, { key, label },
        h("select", {
          className: "select select-sm",
          value: filters[key],
          onChange: (event) => setFilter(key, event.target.value),
        },
        h("option", { value: "" }, emptyLabel ?? `Any ${label.toLowerCase()}`),
        ...Object.entries(filterOptions[key] ?? options ?? {}).map(([value, entry]) => h("option", { key: value, value }, entry.label)),
        ),
      );
    }),
    text("q", "Search", config.searchPlaceholder, "search"),
    date("from", `${fromLabel} (UTC)`, { max: filters.to || undefined }),
    date("to", `${toLabel} (UTC)`, { min: filters.from || undefined }),
    h(ListPageSize, { controls }),
    hasFilters(view) || Object.values(drafts).some(Boolean)
      ? h("button", { type: "button", className: "btn btn-sm", onClick: () => replaceFilters() }, "Clear filters")
      : null,
    chips.length ? h("div", { className: "row list-filter-chips" }, ...chips) : null,
  );
}

export function ListSortHeading({ column, controls }) {
  const { sort, dir } = controls.view;
  const active = sort === column.key;
  return h("th", { scope: "col", "aria-sort": active ? (dir === "asc" ? "ascending" : "descending") : undefined },
    h("button", { type: "button", className: "list-sort", title: column.title, onClick: () => controls.sortBy(column) },
      column.label,
      active && column.orderLabels?.[dir] ? h("span", { className: "muted list-sort-order" }, column.orderLabels[dir]) : null,
      active ? h(Icon, {
        name: dir === "asc" ? "chevron-up" : "chevron-down",
        intent: "neutral",
        className: "list-sort-mark",
      }) : null,
    ),
  );
}

export function ListEmptyRow({ controls, empty, filtered, colSpan }) {
  return h("tr", null, h("td", { colSpan, className: "muted" }, controls.filtered() ? filtered : empty));
}

async function readListChunk(read, path, signal) {
  signal.throwIfAborted();
  const controller = new AbortController();
  const abort = () => controller.abort();
  signal.addEventListener("abort", abort, { once: true });
  const timeout = setTimeout(abort, PAGE_TIMEOUT_MS);
  try {
    const payload = await read(path, controller.signal);
    controller.signal.throwIfAborted();
    return payload;
  } catch (error) {
    if (!signal.aborted && controller.signal.aborted) {
      throw new Error("Request timed out. Refresh to try again.");
    }
    throw error;
  } finally {
    signal.removeEventListener("abort", abort);
    clearTimeout(timeout);
  }
}

export async function readListPage({ read, path, query, collection, size, signal, onProgress, beforeChunk, shouldContinue }) {
  const target = size === "all" ? Infinity : Number(size);
  const params = new URLSearchParams(query);
  const seen = new Set([params.get("after")]);
  const rows = [];
  const names = {};
  const summaries = {};
  let first, next;
  do {
    params.set("limit", String(Math.min(target - rows.length, REQUEST_PAGE_SIZE)));
    if (collection === "bans") {
      if (first) params.set("skip_metadata", "true");
    } else if (params.get("removed") !== "true") {
      if (first) params.delete("include_total");
      else params.set("include_total", "true");
    }
    if (beforeChunk) await beforeChunk();
    // Check after the wait. A skipped continuation leaves no complete page.
    if (first !== undefined && shouldContinue?.() === false) return null;
    const page = await readListChunk(read, `${path}?${params}`, signal);
    first ??= page;
    const batch = page[collection] || [];
    rows.push(...batch);
    Object.assign(names, page.names);
    Object.assign(summaries, page.summaries);
    next = page.next_cursor || null;
    if (!next || target <= REQUEST_PAGE_SIZE || rows.length >= target) break;
    if (!batch.length || seen.has(next)) throw new Error("The list cursor did not advance. Refresh to try again.");
    seen.add(next);
    params.set("after", next);
    onProgress(rows.length);
  } while (next);
  return { ...first, [collection]: rows, names, summaries, next_cursor: next };
}

export function listLoadProgress(size) {
  return size === "all" || Number(size) > REQUEST_PAGE_SIZE ? 0 : null;
}

export function ListLoadProgress({ loaded, onCancel }) {
  return h("div", { className: "row" },
    h("span", { className: "muted", role: "status" },
      loaded === 0 ? "Loading rows…" : `${loaded.toLocaleString()} rows loaded…`,
    ),
    h("button", { type: "button", className: "btn btn-sm", onClick: onCancel }, "Cancel loading"),
  );
}
