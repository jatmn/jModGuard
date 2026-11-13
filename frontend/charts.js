import React, { useState } from "react";

const h = React.createElement;

// Shared trend rendering for the workspace and owner analytics views. Charts
// are dependency-free SVG: the data arrives already bucketed per UTC day.
export const TREND_METRICS = [
  ["warns", "Warnings"],
  ["bans", "Bans"],
  ["automod", "Spam hits"],
  ["modActions", "Mod actions"],
  ["timeouts", "Timeouts"],
  ["joins", "Joins"],
  ["leaves", "Leaves"],
];

export function sumSeries(points, metric) {
  return points.reduce((total, point) => total + (point[metric] ?? 0), 0);
}

export function useTrendMetric(initial) {
  const [metric, setMetric] = useState(initial);
  return [metric, setMetric];
}

const CHART_WIDTH = 720;
const CHART_HEIGHT = 240;
const PAD_LEFT = 46;
const PAD_RIGHT = 14;
const PAD_TOP = 16;
const PAD_BOTTOM = 30;

function scale(value, max) {
  return value / max;
}

function dayLabel(day) {
  // YYYY-MM-DD to "Nov 12" without locale-dependent output.
  const [year, month, date] = String(day).split("-").map(Number);
  const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  if (!year || !month || !date) return String(day);
  return `${months[month - 1]} ${date}`;
}

export function TrendChart({ points, metric, title }) {
  const values = points.map((point) => point[metric] ?? 0);
  const count = points.length;
  const max = Math.max(1, ...values);
  const innerWidth = CHART_WIDTH - PAD_LEFT - PAD_RIGHT;
  const innerHeight = CHART_HEIGHT - PAD_TOP - PAD_BOTTOM;
  const x = (index) => PAD_LEFT + (count <= 1 ? innerWidth / 2 : (index * innerWidth) / (count - 1));
  const y = (value) => PAD_TOP + innerHeight - scale(value, max) * innerHeight;
  const linePoints = values
    .map((value, index) => `${x(index).toFixed(1)},${y(value).toFixed(1)}`)
    .join(" ");
  const area = `M${x(0).toFixed(1)},${(PAD_TOP + innerHeight).toFixed(1)} L${linePoints.split(" ").join(" L ")} L${x(count - 1).toFixed(1)},${(PAD_TOP + innerHeight).toFixed(1)} Z`;
  const gridValues = [max, max / 2, 0];
  const latest = values[count - 1] ?? 0;
  const first = points[0]?.day ?? "";
  const last = points[count - 1]?.day ?? "";
  return h(
    "figure",
    { className: "trend-chart" },
    h(
      "svg",
      {
        viewBox: `0 0 ${CHART_WIDTH} ${CHART_HEIGHT}`,
        role: "img",
        "aria-label": `${title} per day from ${first} to ${last}; latest day ${latest}.`,
      },
      // Horizontal gridlines with their value labels.
      ...gridValues.map((value) =>
        h("g", { key: `grid-${value}`, className: "trend-grid" },
          h("line", { x1: PAD_LEFT, x2: CHART_WIDTH - PAD_RIGHT, y1: y(value).toFixed(1), y2: y(value).toFixed(1) }),
          h("text", { x: PAD_LEFT - 8, y: (y(value) + 4).toFixed(1), textAnchor: "end" }, String(Math.round(value))),
        ),
      ),
      h("path", { className: "trend-area", d: area }),
      h("polyline", { className: "trend-line", points: linePoints }),
      // Day labels under both ends of the window.
      h("text", { className: "trend-day", x: PAD_LEFT, y: CHART_HEIGHT - 8, textAnchor: "start" }, dayLabel(first)),
      h("text", { className: "trend-day", x: CHART_WIDTH - PAD_RIGHT, y: CHART_HEIGHT - 8, textAnchor: "end" }, dayLabel(last)),
    ),
    h("figcaption", { className: "sr-only" }, `${title} per day, ${first} through ${last}.`),
  );
}

export function TrendMetricPicker({ metric, onChange }) {
  return h(
    "div",
    { className: "trend-picker", role: "group", "aria-label": "Trend metric" },
    ...TREND_METRICS.map(([key, label]) =>
      h(
        "button",
        {
          key,
          type: "button",
          className: `btn btn-sm trend-pick${metric === key ? " trend-pick-active" : ""}`,
          "aria-pressed": metric === key ? "true" : "false",
          onClick: () => onChange(key),
        },
        label,
      ),
    ),
  );
}

// ---------------------------------------------------------------------------
// Analytics dashboard (#587). Dense, dependency-free panels for one range.
// Every value shown here comes from one summary response; nothing refetches.
// ---------------------------------------------------------------------------

export const ANALYTICS_RANGES = [7, 30, 90];

// Enforcement metrics in panel order. Records overlap (a !warn writes a
// warning and a mod action), so they are shown side by side, never summed.
export const ENFORCEMENT_METRICS = [
  ["warns", "Warnings", "an-warns"],
  ["bans", "Bans", "an-bans"],
  ["automod", "Spam hits", "an-spam"],
  ["modActions", "Mod actions", "an-mod"],
  ["timeouts", "Timeouts", "an-timeouts"],
];

const COMPACT_UNITS = [
  [1e12, "T"],
  [1e9, "B"],
  [1e6, "M"],
  [1e3, "k"],
];

function groupDigits(value) {
  return String(value).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

// Exact integer with thousands separators, independent of the browser locale.
export function formatExact(value) {
  const n = Math.trunc(Number(value) || 0);
  return (n < 0 ? "\u2212" : "") + groupDigits(Math.abs(n));
}

// Short integer for tiles and axes: at most five characters plus the sign, so
// tiles never wrap. Exact values stay in titles and accessible labels.
export function formatCount(value) {
  const n = Math.trunc(Number(value) || 0);
  const abs = Math.abs(n);
  const sign = n < 0 ? "\u2212" : "";
  if (abs < 10000) return sign + groupDigits(abs);
  for (const [size, unit] of COMPACT_UNITS) {
    if (abs >= size) {
      const scaled = abs / size;
      const text = scaled >= 100 ? String(Math.floor(scaled)) : (Math.floor(scaled * 10) / 10).toFixed(1).replace(/\.0$/, "");
      return `${sign}${text}${unit}`;
    }
  }
  return sign + String(abs);
}

// Change against the previous window of the same length. `previous` is null
// when that window is not fully recorded.
export function formatDelta(current, previous) {
  if (previous == null) return null;
  const diff = (Number(current) || 0) - (Number(previous) || 0);
  const text = diff === 0 ? "±0" : `${diff > 0 ? "+" : "\u2212"}${formatCount(Math.abs(diff))}`;
  const exact = diff === 0 ? "no change" : `${diff > 0 ? "up" : "down"} ${formatExact(Math.abs(diff))}`;
  return { diff, text, exact, direction: diff > 0 ? "up" : diff < 0 ? "down" : "flat" };
}

// Integer axis ticks for sparse daily counts: 0..max in at most four steps.
export function niceTicks(max) {
  const top = Math.max(1, Math.ceil(Number(max) || 0));
  if (top <= 4) return Array.from({ length: top + 1 }, (_, i) => i);
  const raw = top / 4;
  const magnitude = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * magnitude).find((s) => s >= raw && Number.isInteger(s)) ?? Math.ceil(raw);
  const ticks = [];
  for (let value = 0; value < top + step; value += step) ticks.push(value);
  return ticks;
}

// Indices that get an x-axis day label: every day for a week, otherwise
// evenly spaced so labels never collide.
export function dayTickIndices(count) {
  if (count <= 0) return [];
  if (count <= 8) return Array.from({ length: count }, (_, i) => i);
  const step = count <= 31 ? 7 : 15;
  const ticks = [];
  for (let i = count - 1; i >= 0; i -= step) ticks.unshift(i);
  return ticks;
}

export function shortDay(day) {
  return dayLabel(day);
}

// The first and last day labels align to the plot edges so they never clip.
function dayAnchor(index, count) {
  if (count > 1 && index === 0) return "start";
  if (count > 1 && index === count - 1) return "end";
  return "middle";
}

// One hover/focus position shared by every chart in a panel. Arrow keys move
// it so tooltips are reachable without a pointer; Escape clears it.
export function useChartCursor(count) {
  const [index, setIndex] = useState(null);
  const onKeyDown = (event) => {
    const last = count - 1;
    if (last < 0) return;
    const moves = {
      ArrowRight: (i) => Math.min(last, (i ?? -1) + 1),
      ArrowLeft: (i) => Math.max(0, (i ?? last + 1) - 1),
      Home: () => 0,
      End: () => last,
    };
    if (event.key === "Escape") {
      setIndex(null);
      return;
    }
    const move = moves[event.key];
    if (!move) return;
    event.preventDefault();
    setIndex((i) => move(i));
  };
  return { index: index != null && index < count ? index : null, setIndex, onKeyDown };
}

function Sparkline({ values, className }) {
  const width = 72;
  const height = 18;
  const max = Math.max(1, ...values);
  const slot = values.length ? width / values.length : width;
  const bar = Math.max(1, slot - (values.length > 40 ? 0.4 : 1));
  return h(
    "svg",
    { className: `an-spark ${className ?? ""}`, viewBox: `0 0 ${width} ${height}`, "aria-hidden": "true", focusable: "false", preserveAspectRatio: "none" },
    ...values.map((value, i) => {
      const barHeight = value > 0 ? Math.max(1.5, (value / max) * height) : 0.75;
      return h("rect", { key: i, x: (i * slot).toFixed(2), y: (height - barHeight).toFixed(2), width: bar.toFixed(2), height: barHeight.toFixed(2), className: value > 0 ? "an-spark-bar" : "an-spark-zero" });
    }),
  );
}

// Compact KPI tile: label, short number (exact in the title), change against
// the previous window and an optional daily sparkline.
export function KpiTile({ label, value, delta, values, color, badge, loading, unavailable, rangeLabel }) {
  const shown = loading ? "…" : unavailable ? "—" : formatCount(value);
  const exact = loading ? "loading" : unavailable ? "unavailable" : formatExact(value);
  const deltaText = delta ? `${delta.exact} vs previous ${rangeLabel}` : null;
  return h(
    "div",
    { className: "an-kpi", role: "group", "aria-label": `${label}: ${exact}${deltaText ? `, ${deltaText}` : ""}${badge ? `, ${badge.label}` : ""}`, "aria-busy": loading ? "true" : "false" },
    h("div", { className: "an-kpi-head" }, h("span", { className: "an-kpi-label" }, label)),
    h(
      "div",
      { className: "an-kpi-body" },
      h("span", { className: "an-kpi-value", title: exact }, shown),
      delta ? h("span", { className: `an-delta an-delta-${delta.direction}`, title: deltaText }, delta.text) : null,
      badge ? h("span", { className: `an-badge an-badge-${badge.tone}`, title: badge.title }, badge.label) : null,
    ),
    values?.length ? h(Sparkline, { values, className: color }) : null,
  );
}

export function RangeSelector({ value, onChange, disabled }) {
  return h(
    "div",
    { className: "an-range", role: "group", "aria-label": "Analytics date range" },
    ...ANALYTICS_RANGES.map((days) =>
      h(
        "button",
        {
          key: days,
          type: "button",
          className: `btn btn-sm an-range-btn${value === days ? " an-range-active" : ""}`,
          "aria-pressed": value === days ? "true" : "false",
          "aria-label": `Last ${days} days`,
          disabled,
          onClick: () => onChange(days),
        },
        `${days}d`,
      ),
    ),
  );
}

function ChartReadout({ id, children }) {
  return h("div", { id, className: "an-readout", "aria-live": "polite" }, children);
}

// Small multiples: one aligned daily bar strip per enforcement metric, sharing
// one x axis and one hover day so metrics compare without being summed.
export function EnforcementChart({ points, metrics = ENFORCEMENT_METRICS }) {
  const cursor = useChartCursor(points.length);
  const width = 640;
  const rowHeight = 34;
  const labelWidth = 92;
  const axisHeight = 16;
  const innerWidth = width - labelWidth - 8;
  const height = metrics.length * rowHeight + axisHeight;
  const slot = points.length ? innerWidth / points.length : innerWidth;
  const gap = points.length > 45 ? 0.6 : Math.min(3, slot * 0.25);
  const first = points[0]?.day ?? "";
  const last = points.at(-1)?.day ?? "";
  const active = cursor.index != null ? points[cursor.index] : null;
  const rows = metrics.map(([key, label, color], row) => {
    const values = points.map((p) => p[key] ?? 0);
    const max = Math.max(1, ...values);
    const empty = values.every((value) => !(value > 0));
    const top = row * rowHeight + 4;
    const usable = rowHeight - 10;
    return h(
      "g",
      { key, className: `an-row ${color}` },
      h("text", { className: "an-axis-label", x: 0, y: top + usable / 2 + 4 }, label),
      // An empty row says so instead of drawing a scale for values it does not have.
      empty
        ? h("text", { className: "an-row-empty", x: labelWidth + 4, y: top + usable / 2 + 4 }, "None in this range")
        : h("text", { className: "an-axis-value", x: labelWidth - 6, y: top + 9, textAnchor: "end" }, formatCount(max)),
      h("line", { className: "an-baseline", x1: labelWidth, x2: width - 8, y1: top + usable, y2: top + usable }),
      ...values.map((value, i) => {
        const barHeight = value > 0 ? Math.max(2, (value / max) * usable) : 0;
        return barHeight
          ? h("rect", { key: i, className: "an-bar", x: (labelWidth + i * slot + gap / 2).toFixed(2), y: (top + usable - barHeight).toFixed(2), width: Math.max(0.8, slot - gap).toFixed(2), height: barHeight.toFixed(2) })
          : null;
      }),
    );
  });
  return h(
    "figure",
    { className: "an-figure" },
    h(
      ChartReadout,
      { id: "an-enforcement-readout" },
      active
        ? [h("strong", { key: "d" }, shortDay(active.day)), ...metrics.map(([key, label, color]) => h("span", { key, className: `an-readout-item ${color}` }, h("i", { className: "an-swatch", "aria-hidden": "true" }), `${label} ${formatExact(active[key])}`))]
        : h("span", { className: "muted" }, "Hover or use arrow keys for daily values"),
    ),
    h(
      "svg",
      {
        viewBox: `0 0 ${width} ${height}`,
        role: "img",
        tabIndex: 0,
        "aria-label": `Daily enforcement from ${first} to ${last}: ${metrics.map(([key, label]) => `${label} ${formatExact(sumSeries(points, key))}`).join(", ")}. Use arrow keys to read each day.`,
        "aria-describedby": "an-enforcement-readout",
        onKeyDown: cursor.onKeyDown,
        onMouseLeave: () => cursor.setIndex(null),
        onBlur: () => cursor.setIndex(null),
      },
      ...rows,
      cursor.index != null
        ? h("rect", { className: "an-cursor", x: (labelWidth + cursor.index * slot).toFixed(2), y: 0, width: slot.toFixed(2), height: height - axisHeight })
        : null,
      ...dayTickIndices(points.length).map((i) =>
        h("text", { key: `t${i}`, className: "an-axis-day", x: (labelWidth + i * slot + [0, slot / 2, slot][["start", "middle", "end"].indexOf(dayAnchor(i, points.length))]).toFixed(2), y: height - 3, textAnchor: dayAnchor(i, points.length) }, shortDay(points[i].day)),
      ),
      // Transparent day columns carry the pointer and native tooltips.
      ...points.map((p, i) =>
        h(
          "rect",
          { key: `hit${i}`, className: "an-hit", x: (labelWidth + i * slot).toFixed(2), y: 0, width: slot.toFixed(2), height: height - axisHeight, onMouseEnter: () => cursor.setIndex(i) },
          h("title", {}, `${p.day}: ${metrics.map(([key, label]) => `${label} ${formatExact(p[key])}`).join(", ")}`),
        ),
      ),
    ),
  );
}

// Joins above and leaves below a zero baseline, with the running net change
// across the range drawn as a line with points.
export function MembershipChart({ points }) {
  const cursor = useChartCursor(points.length);
  const width = 440;
  const height = 176;
  const padLeft = 34;
  const padRight = 36;
  const padTop = 8;
  const axisHeight = 16;
  const innerWidth = width - padLeft - padRight;
  const half = (height - padTop - axisHeight) / 2;
  const zero = padTop + half;
  const maxBar = Math.max(1, ...points.map((p) => Math.max(p.joins ?? 0, p.leaves ?? 0)));
  const ticks = niceTicks(maxBar);
  const tickMax = ticks.at(-1);
  let running = 0;
  const net = points.map((p) => (running += (p.joins ?? 0) - (p.leaves ?? 0)));
  const netExtent = Math.max(1, ...net.map((v) => Math.abs(v)));
  const slot = points.length ? innerWidth / points.length : innerWidth;
  const gap = points.length > 45 ? 0.6 : Math.min(3, slot * 0.25);
  const x = (i) => padLeft + i * slot + slot / 2;
  const yBar = (v) => (v / tickMax) * half;
  const yNet = (v) => zero - (v / netExtent) * half;
  const active = cursor.index != null ? points[cursor.index] : null;
  const first = points[0]?.day ?? "";
  const last = points.at(-1)?.day ?? "";
  const netTotal = net.at(-1) ?? 0;
  return h(
    "figure",
    { className: "an-figure" },
    h(
      ChartReadout,
      { id: "an-membership-readout" },
      active
        ? [
            h("strong", { key: "d" }, shortDay(active.day)),
            h("span", { key: "j", className: "an-readout-item an-joins" }, h("i", { className: "an-swatch", "aria-hidden": "true" }), `Joins ${formatExact(active.joins)}`),
            h("span", { key: "l", className: "an-readout-item an-leaves" }, h("i", { className: "an-swatch", "aria-hidden": "true" }), `Leaves ${formatExact(active.leaves)}`),
            h("span", { key: "n", className: "an-readout-item an-net" }, h("i", { className: "an-swatch", "aria-hidden": "true" }), `Net so far ${formatDelta(net[cursor.index], 0).text}`),
          ]
        : [
            h("span", { key: "j", className: "an-readout-item an-joins" }, h("i", { className: "an-swatch", "aria-hidden": "true" }), "Joins"),
            h("span", { key: "l", className: "an-readout-item an-leaves" }, h("i", { className: "an-swatch", "aria-hidden": "true" }), "Leaves"),
            h("span", { key: "n", className: "an-readout-item an-net" }, h("i", { className: "an-swatch an-swatch-line", "aria-hidden": "true" }), "Net change"),
          ],
    ),
    h(
      "svg",
      {
        viewBox: `0 0 ${width} ${height}`,
        role: "img",
        tabIndex: 0,
        "aria-label": `Membership from ${first} to ${last}: joins ${formatExact(sumSeries(points, "joins"))}, leaves ${formatExact(sumSeries(points, "leaves"))}, net ${formatDelta(netTotal, 0).exact}. Use arrow keys to read each day.`,
        "aria-describedby": "an-membership-readout",
        onKeyDown: cursor.onKeyDown,
        onMouseLeave: () => cursor.setIndex(null),
        onBlur: () => cursor.setIndex(null),
      },
      ...ticks.flatMap((tick) =>
        (tick === 0 ? [0] : [tick, -tick]).map((signed) =>
          h(
            "g",
            { key: `g${signed}`, className: "an-gridline" },
            h("line", { x1: padLeft, x2: width - padRight, y1: (zero - (signed / tickMax) * half).toFixed(2), y2: (zero - (signed / tickMax) * half).toFixed(2) }),
            h("text", { x: padLeft - 6, y: (zero - (signed / tickMax) * half + 4).toFixed(2), textAnchor: "end" }, formatCount(Math.abs(signed))),
          ),
        ),
      ),
      points.every((p) => !(p.joins > 0) && !(p.leaves > 0))
        ? h("text", { className: "an-row-empty", x: padLeft + innerWidth / 2, y: zero - 6, textAnchor: "middle" }, "No joins or leaves in this range")
        : null,
      h("text", { className: "an-axis-net", x: width - padRight + 6, y: padTop + 8 }, `+${formatCount(netExtent)}`),
      h("text", { className: "an-axis-net", x: width - padRight + 6, y: zero + half }, formatDelta(-netExtent, 0).text),
      ...points.flatMap((p, i) => {
        const bars = [];
        const left = (padLeft + i * slot + gap / 2).toFixed(2);
        const barWidth = Math.max(0.8, slot - gap).toFixed(2);
        if (p.joins > 0) bars.push(h("rect", { key: `j${i}`, className: "an-bar an-joins", x: left, y: (zero - Math.max(1.5, yBar(p.joins))).toFixed(2), width: barWidth, height: Math.max(1.5, yBar(p.joins)).toFixed(2) }));
        if (p.leaves > 0) bars.push(h("rect", { key: `l${i}`, className: "an-bar an-leaves", x: left, y: zero.toFixed(2), width: barWidth, height: Math.max(1.5, yBar(p.leaves)).toFixed(2) }));
        return bars;
      }),
      points.length > 1
        ? h("polyline", { className: "an-net-line an-net", points: net.map((v, i) => `${x(i).toFixed(2)},${yNet(v).toFixed(2)}`).join(" ") })
        : null,
      ...net.map((v, i) => h("circle", { key: `n${i}`, className: "an-net-point an-net", cx: x(i).toFixed(2), cy: yNet(v).toFixed(2), r: points.length > 45 ? 1.4 : 2.4 })),
      cursor.index != null
        ? h("rect", { className: "an-cursor", x: (padLeft + cursor.index * slot).toFixed(2), y: padTop, width: slot.toFixed(2), height: half * 2 })
        : null,
      ...dayTickIndices(points.length).map((i) =>
        h("text", { key: `t${i}`, className: "an-axis-day", x: (padLeft + i * slot + [0, slot / 2, slot][["start", "middle", "end"].indexOf(dayAnchor(i, points.length))]).toFixed(2), y: height - 3, textAnchor: dayAnchor(i, points.length) }, shortDay(points[i].day)),
      ),
      ...points.map((p, i) =>
        h(
          "rect",
          { key: `hit${i}`, className: "an-hit", x: (padLeft + i * slot).toFixed(2), y: padTop, width: slot.toFixed(2), height: half * 2, onMouseEnter: () => cursor.setIndex(i) },
          h("title", {}, `${p.day}: joins ${formatExact(p.joins)}, leaves ${formatExact(p.leaves)}, net so far ${formatDelta(net[i], 0).exact}`),
        ),
      ),
    ),
  );
}

// Ranked breakdown as a list with proportional inline bars.
export function RankList({ rows, label, empty, color = "an-mod" }) {
  if (!rows.length) return h("p", { className: "an-empty muted" }, empty);
  const max = Math.max(1, ...rows.map((row) => row.count));
  return h(
    "ol",
    { className: `an-rank ${color}`, "aria-label": label },
    ...rows.map((row) =>
      h(
        "li",
        { key: row.key, className: "an-rank-row", title: `${row.title ?? row.label}: ${formatExact(row.count)}` },
        h("span", { className: "an-rank-label" }, row.label),
        h("span", { className: "an-rank-track", "aria-hidden": "true" }, h("span", { className: "an-rank-fill", style: { width: `${Math.max(2, (row.count / max) * 100).toFixed(1)}%` } })),
        // `text` replaces the count display (a share or signed change); `title` keeps the exact figures.
        h("span", { className: "an-rank-count", "aria-label": row.text ?? formatExact(row.count) }, row.text ?? formatCount(row.count)),
      ),
    ),
  );
}

const WEEKDAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

export function heatLevel(value, max) {
  if (!value) return 0;
  return Math.min(4, Math.max(1, Math.ceil((value / Math.max(1, max)) * 4)));
}

// Weekday x UTC hour grid of enforcement actions.
export function ActivityHeatmap({ cells }) {
  const [active, setActive] = useState(null);
  const grid = Array.from({ length: 7 }, () => Array(24).fill(0));
  for (const [weekday, hour, count] of cells) if (grid[weekday] && hour >= 0 && hour < 24) grid[weekday][hour] += count;
  const max = Math.max(0, ...grid.flat());
  let peak = null;
  grid.forEach((row, weekday) => row.forEach((count, hour) => {
    if (count > 0 && (!peak || count > peak.count)) peak = { weekday, hour, count };
  }));
  // Wide cells fill a full-width panel while keeping the grid short.
  const cell = 30;
  const cellHeight = 13;
  const gap = 2;
  const left = 30;
  const top = 14;
  const width = left + 24 * (cell + gap);
  const height = top + 7 * (cellHeight + gap);
  const hourLabel = (hour) => `${String(hour).padStart(2, "0")}:00`;
  const summary = peak
    ? `Busiest hour: ${WEEKDAYS[peak.weekday]} ${hourLabel(peak.hour)} UTC with ${formatExact(peak.count)} actions.`
    : "No enforcement actions in this range.";
  return h(
    "figure",
    { className: "an-figure" },
    h(ChartReadout, { id: "an-heatmap-readout" }, active ? `${WEEKDAYS[active.weekday]} ${hourLabel(active.hour)} UTC: ${formatExact(active.count)} actions` : summary),
    h(
      "svg",
      { className: "an-heatmap", viewBox: `0 0 ${width} ${height}`, role: "img", "aria-label": `Enforcement actions by weekday and UTC hour. ${summary}`, "aria-describedby": "an-heatmap-readout", onMouseLeave: () => setActive(null) },
      ...[0, 6, 12, 18].map((hour) => h("text", { key: `h${hour}`, className: "an-axis-day", x: left + hour * (cell + gap), y: 10 }, hourLabel(hour))),
      ...WEEKDAYS.map((day, weekday) => h("text", { key: day, className: "an-axis-day", x: 0, y: top + weekday * (cellHeight + gap) + cellHeight - 3 }, day)),
      ...grid.flatMap((row, weekday) =>
        row.map((count, hour) =>
          h(
            "rect",
            { key: `${weekday}-${hour}`, className: `an-heat an-heat-${heatLevel(count, max)}`, x: left + hour * (cell + gap), y: top + weekday * (cellHeight + gap), width: cell, height: cellHeight, rx: 2, onMouseEnter: () => setActive({ weekday, hour, count }) },
            h("title", {}, `${WEEKDAYS[weekday]} ${hourLabel(hour)} UTC: ${formatExact(count)}`),
          ),
        ),
      ),
    ),
  );
}

// ---------------------------------------------------------------------------
// Database capacity (#613). Plain proportional bars: the accessible label on
// each carries every value the bar draws, so the marks themselves stay hidden.
// ---------------------------------------------------------------------------

function sharePercent(value, total) {
  return total > 0 ? Math.min(100, Math.max(0, (value / total) * 100)) : 0;
}

// One database from zero to its hard limit. `used` null means the allocation
// is unknown: the bar stays empty and dashed, never drawn as free capacity.
export function CapacityGauge({ used, limit, marks, tone, summary }) {
  return h(
    "div",
    { className: "du-gauge", role: "img", "aria-label": summary },
    h(
      "div",
      { className: `du-gauge-track${used == null ? " du-gauge-unknown" : ""}` },
      used == null ? null : h("span", { className: `du-gauge-fill du-tone-${tone}`, style: { width: `${sharePercent(used, limit).toFixed(2)}%` } }),
      ...marks.map((mark) => h("span", { key: mark.key, className: "du-gauge-mark", style: { left: `${sharePercent(mark.bytes, limit).toFixed(2)}%` } })),
    ),
    h(
      "div",
      { className: "du-gauge-scale", "aria-hidden": "true" },
      h("span", { style: { left: "0%" } }, "0 MB"),
      ...marks.map((mark) => h("span", { key: mark.key, style: { left: `${sharePercent(mark.bytes, limit).toFixed(2)}%` } }, mark.short)),
    ),
  );
}

// Parts of one whole (used and reusable pages, snapshot footprint of its budget).
// An unknown whole draws an empty dashed bar, like an unknown gauge.
export function ShareBar({ parts, total, summary, unknown }) {
  return h(
    "div",
    { className: `du-share${unknown ? " du-share-unknown" : ""}`, role: "img", "aria-label": summary },
    ...parts
      .filter((part) => part.value > 0)
      .map((part) => h("span", { key: part.key, className: `du-share-part du-tone-${part.tone}`, style: { width: `${sharePercent(part.value, total).toFixed(2)}%` } })),
  );
}
