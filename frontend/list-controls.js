import React, { useEffect, useRef, useState } from "react";
import { Icon } from "./icons.js";

const h = React.createElement;
const SEARCH_LIMIT = 200;
const SEARCH_DELAY_MS = 300;
const savedListLocation = "jmodguard.list-before-login";
const commonKeys = ["q", "from", "to"];
let pendingListSearch = null;

function filterKeys(config) {
  return [...config.filters.map(({ key }) => key), ...commonKeys];
}

function emptyFilters(config) {
  return Object.fromEntries(filterKeys(config).map((key) => [key, ""]));
}

export function readListLocation() {
  const query = new URLSearchParams(location.search);
  return { list: query.get("list"), guild: query.get("guild") };
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
    const list = remembered.get("list");
    const guild = remembered.get("guild");
    const config = configs.get(list);
    if (!current.list && config && guilds.some(({ id }) => id === guild)) {
      const restored = new URLSearchParams(location.search);
      for (const key of ["list", "guild", "sort", "dir", ...filterKeys(config)]) {
        if (remembered.has(key)) restored.set(key, remembered.get(key));
      }
      replaceListLocation(restored);
      return { list, guild };
    }
  } catch (_) {}
  return current;
}

export function clearListLocation(config) {
  const query = new URLSearchParams(location.search);
  if (query.get("list") !== config.list) return;
  for (const key of ["list", "guild", "sort", "dir", ...filterKeys(config)]) query.delete(key);
  replaceListLocation(query);
}

function boundedSearch(value) {
  return Array.from(value).slice(0, SEARCH_LIMIT).join("");
}

function readView(config, guildId) {
  const query = new URLSearchParams(location.search);
  const filters = emptyFilters(config);
  const initial = { filters, sort: config.defaultSort, dir: config.defaultDir };
  if (query.get("list") !== config.list || query.get("guild") !== guildId) return initial;
  for (const key of filterKeys(config)) filters[key] = query.get(key) || "";
  for (const key of ["q", ...config.filters.filter(({ kind }) => kind === "text").map(({ key }) => key)]) {
    filters[key] = boundedSearch(filters[key].trim());
  }
  const sort = query.get("sort");
  const dir = query.get("dir");
  return {
    filters,
    sort: config.columns.some((column) => column.key === sort) ? sort : initial.sort,
    dir: dir === "asc" || dir === "desc" ? dir : initial.dir,
  };
}

function viewQuery(config, guildId, view) {
  const query = new URLSearchParams(location.search);
  query.set("list", config.list);
  query.set("guild", guildId);
  query.set("sort", view.sort);
  query.set("dir", view.dir);
  for (const [key, value] of Object.entries(view.filters)) {
    if (value) query.set(key, value);
    else query.delete(key);
  }
  return query;
}

function hasFilters(view) {
  return Object.values(view.filters).some(Boolean);
}

export function useListControls(config, guildId, onChange, canChange = () => true) {
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

  const commit = (next) => {
    if (!guard.current()) return;
    current.current = next;
    setView(next);
    replaceListLocation(viewQuery(config, guildId, next));
    onChange();
  };
  const setFilter = (key, value) => {
    cancelText(key);
    setDrafts((previous) => {
      const remaining = { ...previous };
      delete remaining[key];
      return remaining;
    });
    const previous = current.current;
    if (previous.filters[key] === value) return;
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
    const timer = setTimeout(() => setFilter(key, trimmed), SEARCH_DELAY_MS);
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
    config, view, drafts, setText, setFilter, replaceFilters, filterLink, sortBy, query,
    filtered: () => hasFilters(current.current),
  };
}

function ListField({ label, children }) {
  return h("div", { className: "field" }, h("label", null, label, children));
}

export function ListControls({ controls, UserPicker, userPickerProps, filterOptions = {} }) {
  const { config, view, drafts, setText, setFilter, replaceFilters } = controls;
  const { filters } = view;
  const date = (key, label, bounds) => h(ListField, { label },
    h("input", {
      className: "input input-sm",
      type: "date",
      value: filters[key],
      ...bounds,
      onChange: (event) => setFilter(key, event.target.value),
    }),
  );
  const text = (key, label, placeholder, type = "text") => h(ListField, { key, label },
    h("input", {
      className: "input input-sm",
      type,
      value: drafts[key] ?? filters[key],
      placeholder,
      maxLength: SEARCH_LIMIT * 2,
      onChange: (event) => setText(key, event.target.value),
    }),
  );
  const chips = [
    ...config.filters.filter(({ kind }) => kind !== "user"),
    { key: "q", label: "Search" },
    { key: "from", label: "From" },
    { key: "to", label: "To" },
  ].filter(({ key }) => filters[key]).map(({ key, label, options }) => {
    const value = (filterOptions[key] ?? options)?.[filters[key]]?.label ?? filters[key];
    return h("button", {
      key, type: "button", className: "btn btn-xs btn-outline",
      "aria-label": `Remove ${label.toLowerCase()} filter`,
      onClick: () => setFilter(key, ""),
    }, `${label}: ${value}`, " ×");
  });
  return h("div", { className: "list-filters", "data-no-draft": true, role: "group", "aria-label": config.filterLabel },
    ...config.filters.map(({ key, label, options, kind, idLabel, render, placeholder }) => {
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
        h("option", { value: "" }, `Any ${label.toLowerCase()}`),
        ...Object.entries(filterOptions[key] ?? options ?? {}).map(([value, entry]) => h("option", { key: value, value }, entry.label)),
        ),
      );
    }),
    text("q", "Search", config.searchPlaceholder, "search"),
    date("from", "From (UTC)", { max: filters.to || undefined }),
    date("to", "To (UTC)", { min: filters.from || undefined }),
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
