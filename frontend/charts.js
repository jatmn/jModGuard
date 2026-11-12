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
  // YYYY-MM-DD → "Nov 12" without locale-dependent output.
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
