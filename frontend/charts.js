import React, { useState } from "react";

const h = React.createElement;

const MAX_PLOT_HEIGHT = 280;

// Without a viewBox, one SVG unit is one CSS pixel. Measure the available width
// and the inherited font so the plots can reserve room for the rendered labels.
function useChartLayout(initialWidth) {
  const [layout, setLayout] = useState({ width: initialWidth, gap: 5, lineHeight: 20, measureLabel: (text) => text.length * 7 });
  const ref = React.useCallback((element) => {
    if (!element || !window.ResizeObserver) return;
    const canvas = element.ownerDocument.createElement("canvas").getContext("2d");
    const measure = () => {
      const width = element.clientWidth;
      if (!width) return;
      const style = window.getComputedStyle(element);
      const fontSize = parseFloat(style.fontSize);
      const font = `${style.fontSize} ${style.fontFamily}`;
      if (canvas) canvas.font = font;
      const metrics = canvas?.measureText("0");
      const fontHeight = (metrics?.fontBoundingBoxAscent ?? 0) + (metrics?.fontBoundingBoxDescent ?? 0);
      const lineHeight = Math.ceil(Math.max(fontSize * 1.5, fontHeight));
      const measureLabel = canvas
        ? (text) => Math.ceil(canvas.measureText(text).width)
        : (text) => Math.ceil(text.length * fontSize * 0.65);
      setLayout((previous) => previous.width === width && previous.font === font
        ? previous : { width, font, gap: Math.ceil(fontSize / 3), lineHeight, fontHeight: fontHeight || lineHeight, measureLabel });
    };
    measure();
    const observer = new window.ResizeObserver(measure);
    observer.observe(element);
    observer.observe(element.firstElementChild);
    return () => observer.disconnect();
  }, []);
  return { ...layout, ref };
}

function ChartPlot({ children, className = "", ...props }) {
  // The hidden em-square changes size with the font even when the viewport is stable.
  return h("svg", { ...props, className: `chart-plot ${className}` },
    h("rect", { width: "1em", height: "1em", visibility: "hidden", "aria-hidden": "true" }), children);
}

function axisText(text, props) {
  return h("text", { dominantBaseline: "central", ...props }, text);
}

function fitLabel(text, available, measureLabel) {
  if (measureLabel(text) <= available) return text;
  for (let length = text.length - 1; length > 0; length--) {
    const shorter = `${text.slice(0, length).trimEnd()}…`;
    if (measureLabel(shorter) <= available) return shorter;
  }
  return "";
}

const ANCHOR_FRACTIONS = { start: 0, middle: 0.5, end: 1 };

function tickPlacements({ width, measureLabel }, { text, x, anchor: preferred = "start", key }) {
  if (!text) return [];
  const size = measureLabel(text);
  // Keep the tick at x while trying the text on either side or centered.
  return [...new Set([preferred, "middle", "start", "end"])].map((anchor) => {
    const left = x - size * ANCHOR_FRACTIONS[anchor];
    return { key, text, x, anchor, left, right: left + size };
  }).filter(({ left, right }) => left >= 0 && right <= width);
}

function tickVariants(layout, tick) {
  const variants = [tickPlacements(layout, tick)];
  if (tick.shortText) variants.push(tickPlacements(layout, { ...tick, text: tick.shortText }));
  return variants;
}

function layoutTicks(layout, labels, maxRows, reserved = []) {
  const { gap } = layout;
  const placed = [...reserved];
  const rows = Array.from({ length: maxRows }, (_, row) => reserved.filter((tick) => tick.row === row));
  for (const tick of labels) {
    const candidates = tickVariants(layout, tick).flat();
    for (const [row, ticks] of rows.entries()) {
      const fit = candidates.find(({ left, right }) => ticks.every((tick) => right + gap <= tick.left || left >= tick.right + gap));
      if (!fit) continue;
      ticks.push(fit);
      placed.push({ ...fit, row });
      break;
    }
  }
  return placed;
}

function fitEndpointPair(layout, first, last) {
  const starts = tickVariants(layout, first);
  const ends = tickVariants(layout, last);
  // Prefer the normal gap, then allow a tighter pair with positive clearance.
  for (const gap of new Set([layout.gap, Math.min(layout.gap, 2)])) {
    for (const startForm of starts) {
      for (const endForm of ends) {
        for (const start of startForm) {
          const end = endForm.find((tick) => start.right + gap <= tick.left);
          if (end) return [{ ...start, row: 0 }, { ...end, row: 0 }];
        }
      }
    }
  }
  return [];
}

function horizontalTickLayout(layout, labels, rows = 1) {
  const first = labels[0];
  const last = labels.at(-1);
  const interior = labels.slice(1, -1).reverse();
  const endpoints = labels.length > 1 ? fitEndpointPair(layout, first, last) : [];
  const priority = endpoints.length ? interior : labels.length > 1 && rows === 1 ? [last, first, ...interior] : labels;
  return layoutTicks(layout, priority, rows, endpoints).sort((a, b) => a.x - b.x);
}

function renderHorizontalTicks(ticks, { y, axisY, origin = 0, className = "an-axis-day", fontSize, rowStep = 0 }) {
  const tickEnd = axisY + (y < axisY ? -3 : 3);
  return ticks.map(({ key, text, x, anchor, row }) =>
    h("g", { key },
      h("line", { className: "chart-axis-tick", x1: origin + x, x2: origin + x, y1: axisY, y2: tickEnd }),
      axisText(text, { className, x: origin + x, y: y + row * rowStep, textAnchor: anchor, fontSize }),
    ));
}

function horizontalTicks(layout, labels, options) {
  return renderHorizontalTicks(horizontalTickLayout(layout, labels), options);
}

function staggeredRowLabels({ width, lineHeight, gap, measureLabel }, rowHeight, variants) {
  // Reserve at least half the SVG for the data, even with enlarged labels.
  const budget = width / 2;
  const columns = Math.min(variants[0].length, Math.ceil(lineHeight / rowHeight));
  for (const [variant, labels] of variants.entries()) {
    const padding = variant === 0 ? gap : Math.max(1, gap / 3);
    const sizes = Array(columns).fill(0);
    labels.forEach((text, row) => {
      const column = row % columns;
      sizes[column] = Math.max(sizes[column], measureLabel(text));
    });
    let gutter = padding;
    const starts = sizes.map((size) => {
      const x = gutter;
      gutter += size + padding;
      return x;
    });
    if (gutter <= budget) return { width: gutter, labels: labels.map((text, row) => ({ text, x: starts[row % columns] })) };
  }

  // If compact labels cannot fit at the inherited size, use one fitted column.
  const labels = variants[0];
  const widest = Math.max(...labels.map(measureLabel));
  const padding = Math.min(gap, budget / 4);
  const scale = Math.min(rowHeight / lineHeight, (budget - 2 * padding) / widest);
  return { width: 2 * padding + widest * scale, fontSize: `${scale}em`, labels: labels.map((text) => ({ text, x: padding })) };
}

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
  const layout = useChartLayout(720);
  const { width, lineHeight, gap, measureLabel, ref } = layout;
  const height = 240;
  const values = points.map((point) => point[metric] ?? 0);
  const count = points.length;
  const max = Math.max(1, ...values);
  const padLeft = measureLabel(String(Math.round(max))) + 2 * gap;
  const padTop = lineHeight / 2 + 2;
  const innerWidth = Math.max(1, width - padLeft - gap);
  const innerHeight = Math.max(1, height - 2 * padTop - lineHeight - gap);
  const x = (index) => padLeft + (count <= 1 ? innerWidth / 2 : (index * innerWidth) / (count - 1));
  const y = (value) => padTop + innerHeight - scale(value, max) * innerHeight;
  const linePoints = values
    .map((value, index) => `${x(index).toFixed(1)},${y(value).toFixed(1)}`)
    .join(" ");
  const area = `M${x(0).toFixed(1)},${y(0).toFixed(1)} L${linePoints.split(" ").join(" L ")} L${x(count - 1).toFixed(1)},${y(0).toFixed(1)} Z`;
  const gridValues = innerHeight / 2 >= lineHeight ? [max, max / 2, 0] : [max, 0];
  const latest = values[count - 1] ?? 0;
  const first = points[0]?.day ?? "";
  const last = points[count - 1]?.day ?? "";
  return h(
    "figure",
    { className: "trend-chart" },
    h(
      ChartPlot,
      {
        ref,
        height,
        role: "img",
        "aria-label": `${title} per day from ${first} to ${last}; latest day ${latest}.`,
      },
      // Horizontal gridlines with their value labels.
      ...gridValues.map((value) =>
        h("g", { key: `grid-${value}`, className: "trend-grid" },
          h("line", { x1: padLeft, x2: width - gap, y1: y(value).toFixed(1), y2: y(value).toFixed(1) }),
          axisText(String(Math.round(value)), { x: padLeft - gap, y: y(value), textAnchor: "end" }),
        ),
      ),
      h("path", { className: "trend-area", d: area }),
      h("polyline", { className: "trend-line", points: linePoints }),
      // Day labels under both ends of the window.
      ...horizontalTicks(layout, [
        { key: "first", text: dayLabel(first), x: padLeft },
        { key: "last", text: dayLabel(last), x: width - gap, anchor: "end" },
      ], { y: height - padTop, axisY: y(0), className: "trend-day" }),
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

// Keep the last date and space candidate ticks across the selected range.
export function dayTickIndices(count) {
  const step = count <= 8 ? 1 : count <= 31 ? 7 : 15;
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

function daySlots(left, plotWidth, count) {
  const step = plotWidth / Math.max(1, count);
  const maxGap = count > 45 ? 0.6 : 3;
  return Array.from({ length: count }, (_, index) => {
    const x = left + index * step;
    const end = left + (index + 1) * step;
    const width = end - x;
    // Keep the bar centered and inside its hover column, even below one pixel.
    const gap = Math.min(maxGap, width / 4);
    return { x, end, width, center: x + width / 2, bar: { x: x + gap / 2, width: width - gap } };
  });
}

function dayTicks(layout, points, slots, y) {
  return horizontalTicks(layout, dayTickIndices(points.length).map((index) => {
    const anchor = dayAnchor(index, points.length);
    const { x, center, end } = slots[index];
    return { key: `t${index}`, anchor, x: anchor === "start" ? x : anchor === "end" ? end : center, text: shortDay(points[index].day) };
  }), { y, axisY: y - layout.lineHeight / 2 - 4 });
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
    { className: `chart-plot an-spark ${className ?? ""}`, height, viewBox: `0 0 ${width} ${height}`, "aria-hidden": "true", focusable: "false", preserveAspectRatio: "none" },
    ...values.map((value, i) => {
      const barHeight = value > 0 ? Math.max(1.5, (value / max) * height) : 0.75;
      return h("rect", { key: i, x: (i * slot).toFixed(2), y: (height - barHeight).toFixed(2), width: bar.toFixed(2), height: barHeight.toFixed(2), className: value > 0 ? "an-spark-bar" : "an-spark-zero" });
    }),
  );
}

// Compact KPI tile: label, short number (exact in the title), change against
// the previous window and an optional daily sparkline.
export function KpiTile({ label, value, delta, values, color, badge, action, loading, unavailable, rangeLabel }) {
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
      action,
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
  const layout = useChartLayout(640);
  const { width, lineHeight, gap: labelGap, measureLabel, ref } = layout;
  const inset = lineHeight / 2 + 2;
  const axisHeight = lineHeight + labelGap;
  const height = Math.min(MAX_PLOT_HEIGHT, Math.max(220, 2 * inset + axisHeight + metrics.length * (lineHeight + 4)));
  const rowHeight = (height - 2 * inset - axisHeight) / Math.max(1, metrics.length);
  const series = metrics.map(([key, label, color]) => {
    const values = points.map((point) => point[key] ?? 0);
    return { key, label, color, values, max: Math.max(0, ...values) };
  });
  const padRight = measureLabel("0") + 2 * labelGap;
  const maxTickWidth = Math.max(0, ...series.map(({ max }) => measureLabel(formatCount(max))));
  const titleBudget = width - maxTickWidth - padRight - 3 * labelGap - Math.min(48, width / 5);
  const titles = series.map(({ label }) => fitLabel(label, titleBudget, measureLabel));
  const titleWidth = Math.max(0, ...titles.map(measureLabel));
  const labelWidth = titleWidth + maxTickWidth + 3 * labelGap;
  const innerWidth = Math.max(1, width - labelWidth - padRight);
  const emptyLabel = fitLabel("None in this range", innerWidth - labelGap, measureLabel);
  const slots = daySlots(labelWidth, innerWidth, points.length);
  const cursorSlot = slots[cursor.index];
  const first = points[0]?.day ?? "";
  const last = points.at(-1)?.day ?? "";
  const active = cursor.index != null ? points[cursor.index] : null;
  const rows = series.map(({ key, label, color, values, max }, row) => {
    const top = inset + row * rowHeight;
    const usable = Math.max(1, rowHeight - labelGap / 2);
    const baseline = top + usable;
    const middle = top + usable / 2;
    return h(
      "g",
      { key, className: `an-row ${color}` },
      axisText(titles[row], { className: "an-axis-label", x: labelGap / 2, y: middle, "aria-label": label }),
      // An empty row says so instead of drawing a scale for values it does not have.
      max > 0 ? [
        axisText(formatCount(max), { key: "max", className: "an-axis-value", x: labelWidth - labelGap, y: top, textAnchor: "end" }),
        // Opposite gutters keep adjacent rows' maximum and zero labels apart.
        axisText("0", { key: "min", className: "an-axis-min", x: width - labelGap, y: baseline, textAnchor: "end" }),
      ] : axisText(emptyLabel, { className: "an-row-empty", x: labelWidth + labelGap / 2, y: middle }),
      h("line", { className: "an-baseline", x1: labelWidth, x2: width - padRight, y1: baseline, y2: baseline }),
      ...values.map((value, i) => {
        const barHeight = value > 0 ? Math.min(usable, Math.max(2, (value / max) * usable)) : 0;
        return barHeight
          ? h("rect", { key: i, className: "an-bar", ...slots[i].bar, y: (baseline - barHeight).toFixed(2), height: barHeight.toFixed(2) })
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
      ChartPlot,
      {
        ref,
        height,
        role: "img",
        tabIndex: 0,
        "aria-label": `Daily enforcement from ${first} to ${last}: ${metrics.map(([key, label]) => `${label} ${formatExact(sumSeries(points, key))}`).join(", ")}. Use arrow keys to read each day.`,
        "aria-describedby": "an-enforcement-readout",
        onKeyDown: cursor.onKeyDown,
        onMouseLeave: () => cursor.setIndex(null),
        onBlur: () => cursor.setIndex(null),
      },
      ...rows,
      cursorSlot
        ? h("rect", { className: "an-cursor", x: cursorSlot.x, y: 0, width: cursorSlot.width, height: height - axisHeight })
        : null,
      ...dayTicks(layout, points, slots, height - inset),
      // Transparent day columns carry the pointer and native tooltips.
      ...points.map((p, i) =>
        h(
          "rect",
          { key: `hit${i}`, className: "an-hit", x: slots[i].x, y: 0, width: slots[i].width, height: height - axisHeight, onMouseEnter: () => cursor.setIndex(i) },
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
  const layout = useChartLayout(440);
  const { width, lineHeight, gap: labelGap, measureLabel, ref } = layout;
  const height = Math.min(MAX_PLOT_HEIGHT, Math.max(196, 9 * lineHeight));
  const inset = lineHeight / 2 + 2;
  const axisHeight = inset + lineHeight + labelGap;
  const half = Math.max(1, (height - inset - axisHeight) / 2);
  const zero = inset + half;
  const maxBar = Math.max(1, ...points.map((p) => Math.max(p.joins ?? 0, p.leaves ?? 0)));
  const ticks = niceTicks(maxBar);
  const tickMax = ticks.at(-1);
  let running = 0;
  const net = points.map((p) => (running += (p.joins ?? 0) - (p.leaves ?? 0)));
  const netExtent = Math.max(1, ...net.map((v) => Math.abs(v)));
  const netLabels = [`+${formatCount(netExtent)}`, formatDelta(-netExtent, 0).text];
  const padLeft = Math.max(...ticks.map((tick) => measureLabel(formatCount(tick)))) + 2 * labelGap;
  const padRight = Math.max(...netLabels.map(measureLabel)) + 2 * labelGap;
  const innerWidth = Math.max(1, width - padLeft - padRight);
  const tickStride = Math.ceil((lineHeight * (ticks.length - 1)) / half);
  const labeledTicks = new Set(ticks.filter((tick, i) => tick === 0 || tick === tickMax
    || ((ticks.length - 1 - i) % tickStride === 0 && (tick / tickMax) * half >= lineHeight)));
  const slots = daySlots(padLeft, innerWidth, points.length);
  const cursorSlot = slots[cursor.index];
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
      ChartPlot,
      {
        ref,
        height,
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
            labeledTicks.has(tick) && axisText(formatCount(tick), { x: padLeft - labelGap, y: zero - (signed / tickMax) * half, textAnchor: "end" }),
          ),
        ),
      ),
      points.every((p) => !(p.joins > 0) && !(p.leaves > 0))
        ? axisText(fitLabel("No joins or leaves in this range", innerWidth, measureLabel), { className: "an-row-empty", x: padLeft + innerWidth / 2, y: zero - half / 2, textAnchor: "middle" })
        : null,
      ...netLabels.map((text, i) => axisText(text, { key: i, className: "an-axis-net", x: width - padRight + labelGap, y: i === 0 ? inset : zero + half })),
      ...points.flatMap((p, i) => {
        const bars = [];
        if (p.joins > 0) bars.push(h("rect", { key: `j${i}`, className: "an-bar an-joins", ...slots[i].bar, y: (zero - Math.max(1.5, yBar(p.joins))).toFixed(2), height: Math.max(1.5, yBar(p.joins)).toFixed(2) }));
        if (p.leaves > 0) bars.push(h("rect", { key: `l${i}`, className: "an-bar an-leaves", ...slots[i].bar, y: zero.toFixed(2), height: Math.max(1.5, yBar(p.leaves)).toFixed(2) }));
        return bars;
      }),
      points.length > 1
        ? h("polyline", { className: "an-net-line an-net", points: net.map((v, i) => `${slots[i].center},${yNet(v).toFixed(2)}`).join(" ") })
        : null,
      ...net.map((v, i) => h("circle", { key: `n${i}`, className: "an-net-point an-net", cx: slots[i].center, cy: yNet(v).toFixed(2), r: points.length > 45 ? 1.4 : 2.4 })),
      cursorSlot
        ? h("rect", { className: "an-cursor", x: cursorSlot.x, y: inset, width: cursorSlot.width, height: half * 2 })
        : null,
      ...dayTicks(layout, points, slots, height - inset),
      ...points.map((p, i) =>
        h(
          "rect",
          { key: `hit${i}`, className: "an-hit", x: slots[i].x, y: inset, width: slots[i].width, height: half * 2, onMouseEnter: () => cursor.setIndex(i) },
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

const WEEKDAY_NAMES = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const WEEKDAYS = WEEKDAY_NAMES.map((day) => day.slice(0, 3));
// Single-letter schedules use U for Sunday and R for Thursday.
const WEEKDAY_LABELS = [WEEKDAYS, WEEKDAY_NAMES.map((day) => day.slice(0, 2)), ["U", "M", "T", "W", "R", "F", "S"]];

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
  const layout = useChartLayout(798);
  const { width, lineHeight, gap: labelGap, measureLabel, ref } = layout;
  const top = Math.min(lineHeight + labelGap, MAX_PLOT_HEIGHT / 3);
  const height = Math.min(MAX_PLOT_HEIGHT, Math.max(167, top + WEEKDAYS.length * (lineHeight + 1) + labelGap));
  const bottom = Math.min(height / 4, Math.max(labelGap, lineHeight / 2));
  const rowHeight = (height - top - bottom) / WEEKDAYS.length;
  const weekdays = staggeredRowLabels(layout, rowHeight, WEEKDAY_LABELS);
  const left = weekdays.width;
  const plotWidth = width - left;
  const slot = plotWidth / 24;
  const gap = Math.min(2, slot / 4);
  const hourLabel = (hour) => `${String(hour).padStart(2, "0")}:00`;
  const hourScale = Math.min(1, (top - 2) / lineHeight);
  const measureHour = (text) => Math.ceil(measureLabel(text) * hourScale);
  const hourGap = Math.min(labelGap, plotWidth / 12);
  const hourTicks = [0, 6, 12, 18].map((hour) => ({
    key: hour, x: hour * slot, text: hourLabel(hour), shortText: hourLabel(hour).slice(0, 2),
  }));
  const summary = peak
    ? `Busiest hour: ${WEEKDAYS[peak.weekday]} ${hourLabel(peak.hour)} UTC with ${formatExact(peak.count)} actions.`
    : "No enforcement actions in this range.";
  return h(
    "figure",
    { className: "an-figure" },
    h(ChartReadout, { id: "an-heatmap-readout" }, active ? `${WEEKDAYS[active.weekday]} ${hourLabel(active.hour)} UTC: ${formatExact(active.count)} actions` : summary),
    h(
      ChartPlot,
      { ref, height, className: "an-heatmap", role: "img", "aria-label": `Enforcement actions by weekday and UTC hour. Rows run Sunday through Saturday. ${summary}`, "aria-describedby": "an-heatmap-readout", onMouseLeave: () => setActive(null) },
      ...horizontalTicks({ width: plotWidth, gap: hourGap, measureLabel: measureHour }, hourTicks,
        { y: top / 2, axisY: top, origin: left, fontSize: hourScale < 1 ? `${hourScale}em` : undefined }),
      ...weekdays.labels.map(({ text, x }, weekday) => h("g", { key: WEEKDAYS[weekday] },
        h("title", {}, WEEKDAY_NAMES[weekday]),
        axisText(text, { className: "an-axis-day", x, y: top + (weekday + 0.5) * rowHeight,
          fontSize: weekdays.fontSize, "aria-label": WEEKDAY_NAMES[weekday] }),
      )),
      ...grid.flatMap((row, weekday) =>
        row.map((count, hour) =>
          h(
            "rect",
            { key: `${weekday}-${hour}`, className: `an-heat an-heat-${heatLevel(count, max)}`, x: left + hour * slot, y: top + weekday * rowHeight, width: slot - gap, height: rowHeight - 3, rx: 2, onMouseEnter: () => setActive({ weekday, hour, count }) },
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
  const layout = useChartLayout(480);
  const ticks = [{ key: "zero", text: "0 MB", x: 0 }, ...marks.map((mark, index) => ({
    key: mark.key,
    text: mark.short,
    x: layout.width * sharePercent(mark.bytes, limit) / 100,
    anchor: index === marks.length - 1 ? "end" : "middle",
  }))];
  const labels = layoutTicks(layout, [...ticks].reverse(), ticks.length).reverse();
  const labelRows = Math.max(1, ...labels.map(({ row }) => row + 1));
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
      { className: "du-gauge-scale", "aria-hidden": "true", ref: layout.ref, style: { height: `${labelRows}lh` } },
      h("i", { className: "chart-font-probe" }),
      ...labels.map(({ key, text, x, anchor, row }) => h("span", { key,
        style: { left: x, top: `${row}lh`, transform: `translateX(${-100 * ANCHOR_FRACTIONS[anchor]}%)` } }, text)),
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

// ---------------------------------------------------------------------------
// Bot Resources (#652). Time lines on a fixed grid of buckets. A bucket with
// no sample is null: it breaks the line and is never drawn as zero.
// ---------------------------------------------------------------------------

function utcClock(seconds, withDay) {
  const date = new Date(seconds * 1000);
  const time = `${String(date.getUTCHours()).padStart(2, "0")}:${String(date.getUTCMinutes()).padStart(2, "0")}`;
  if (!withDay) return time;
  const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  return `${months[date.getUTCMonth()]} ${date.getUTCDate()} ${time}`;
}

// `series` entries: { key, label, tone, value(point) -> number | null }.
// `max` fixes the top of the scale (for example 100 for a percentage of a limit).
export function ResourceChart({ points, series, max, format, label, from, to }) {
  const layout = useChartLayout(480);
  const { width, lineHeight, fontHeight = lineHeight, gap, measureLabel, ref } = layout;
  const lines = series.map((line) => ({ ...line, values: points.map((point) => {
    const value = line.value(point);
    return value == null || !Number.isFinite(value) ? null : value;
  }) }));
  const known = lines.flatMap((line) => line.values.filter((value) => value != null));
  const withDay = to - from >= 86400;
  const range = `${utcClock(from, withDay)} to ${utcClock(to, withDay)} UTC`;
  if (!known.length) {
    return h(
      "figure",
      { className: "br-chart br-chart-empty" },
      h("p", { className: "muted", role: "img", "aria-label": `${label}: no samples ${range}.` }, "No samples in this range"),
    );
  }
  const top = max ?? (Math.max(...known) * 1.1 || 1);
  const ticks = [top, 0];
  const maxDateRows = 2;
  const labelRows = ticks.length + maxDateRows;
  const edgePadding = 2;
  // Fit both value labels and both possible date rows within the height cap.
  const minDateGap = Math.min(gap, 2);
  const rowStep = Math.min(lineHeight, (MAX_PLOT_HEIGHT - 2 * edgePadding - minDateGap) / labelRows);
  const height = Math.min(MAX_PLOT_HEIGHT, Math.max(120, (labelRows + 1) * lineHeight));
  const dateGap = Math.min(gap, height - 2 * edgePadding - labelRows * rowStep);
  const inset = rowStep / 2 + edgePadding;
  // Keep the inherited font whenever its rendered box fits the tighter rows.
  const axisScale = Math.min(1, rowStep / fontHeight);
  const axisFontSize = axisScale < 1 ? `${axisScale}em` : undefined;
  const axisGap = Math.ceil(gap * axisScale);
  const measureAxisLabel = (text) => Math.ceil(measureLabel(text) * axisScale);
  const padRight = axisGap;
  const dates = [utcClock(from, withDay), utcClock(to, withDay)].map((text) => ({
    text, shortText: withDay ? text.slice(0, -6) : undefined,
  }));
  const valueWidth = Math.max(...ticks.map((value) => measureAxisLabel(format(value))));
  let fittedWidth = valueWidth;
  if (valueWidth + 2 * axisGap >= width - padRight) {
    // Make room for data. The compact start date can extend left of its
    // tick, or center on it when the plot starts too far left.
    const startDate = dates[0];
    const dateWidth = measureAxisLabel(startDate.shortText ?? startDate.text);
    const plotStartLimit = width - padRight - 2 * axisGap;
    const dateAnchorLimit = dateWidth <= plotStartLimit ? plotStartLimit : width - Math.ceil(dateWidth / 2);
    fittedWidth = Math.max(1, Math.min(plotStartLimit, dateAnchorLimit) - 2 * axisGap);
  }
  const valueScale = axisScale * Math.min(1, fittedWidth / valueWidth);
  const valueFontSize = valueScale < 1 ? `${valueScale}em` : undefined;
  const padLeft = fittedWidth + 2 * axisGap;
  const innerWidth = Math.max(1, width - padLeft - padRight);
  const dateTicks = dates.map((date, i) => ({ key: i, ...date,
    x: i === 0 ? padLeft : width - padRight, anchor: i === 0 ? "start" : "end" }));
  // Large text and a wide value axis can leave room for only one date per row.
  const dateLabels = horizontalTickLayout({ width, gap: axisGap, measureLabel: measureAxisLabel }, dateTicks, maxDateRows);
  const firstDateY = height - inset - (dateLabels.some(({ row }) => row === 1) ? rowStep : 0);
  const innerHeight = firstDateY - rowStep - dateGap - inset;
  const count = points.length;
  const x = (index) => padLeft + (count <= 1 ? innerWidth / 2 : (index * innerWidth) / (count - 1));
  const y = (value) => inset + innerHeight - Math.min(1, Math.max(0, value / top)) * innerHeight;
  const runs = (values) => {
    const out = [];
    let run = [];
    values.forEach((value, index) => {
      if (value == null) {
        if (run.length) out.push(run);
        run = [];
      } else run.push([index, value]);
    });
    if (run.length) out.push(run);
    return out;
  };
  const latest = (values) => [...values].reverse().find((value) => value != null);
  const summary = lines
    .map((line) => {
      const values = line.values.filter((value) => value != null);
      return values.length ? `${line.label} latest ${format(latest(line.values))}, peak ${format(Math.max(...values))}` : `${line.label} no samples`;
    })
    .join("; ");
  return h(
    "figure",
    { className: "br-chart" },
    h(
      ChartPlot,
      { ref, height, role: "img", "aria-label": `${label}, ${range}: ${summary}.` },
      ...ticks.map((value) =>
        h("g", { key: `g${value}`, className: "an-gridline" },
          h("line", { x1: padLeft, x2: width - padRight, y1: y(value).toFixed(1), y2: y(value).toFixed(1) }),
          axisText(format(value), { x: padLeft - axisGap, y: y(value), textAnchor: "end", fontSize: valueFontSize }),
        ),
      ),
      ...lines.flatMap((line) =>
        runs(line.values).map((run, i) =>
          run.length === 1
            ? h("circle", { key: `${line.key}-${i}`, className: `br-dot ${line.tone}`, cx: x(run[0][0]).toFixed(1), cy: y(run[0][1]).toFixed(1), r: 1.8 })
            : h("polyline", { key: `${line.key}-${i}`, className: `br-line ${line.tone}`, points: run.map(([index, value]) => `${x(index).toFixed(1)},${y(value).toFixed(1)}`).join(" ") }),
        ),
      ),
      ...renderHorizontalTicks(dateLabels, { y: firstDateY, axisY: y(0), rowStep, fontSize: axisFontSize }),
    ),
    lines.length > 1 &&
      h(
        "figcaption",
        { className: "br-legend" },
        ...lines.map((line) => h("span", { key: line.key, className: `br-legend-item ${line.tone}` }, h("i", { className: "br-swatch", "aria-hidden": "true" }), line.label)),
      ),
  );
}

// Range choices for Bot Resources, matching the API's `range` values.
export const RESOURCE_RANGES = [
  ["1h", "Last hour"],
  ["24h", "Last 24 hours"],
  ["7d", "Last 7 days"],
  ["30d", "Last 30 days"],
];

export function ResourceRangeSelector({ value, onChange }) {
  return h(
    "div",
    { className: "an-range", role: "group", "aria-label": "Resource range" },
    ...RESOURCE_RANGES.map(([range, label]) =>
      h(
        "button",
        {
          key: range,
          type: "button",
          className: `btn btn-sm an-range-btn${value === range ? " an-range-active" : ""}`,
          "aria-pressed": value === range ? "true" : "false",
          "aria-label": label,
          onClick: () => onChange(range),
        },
        range,
      ),
    ),
  );
}
