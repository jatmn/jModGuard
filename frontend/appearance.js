import React, { useEffect, useState } from "react";

// Preference and resolved theme differ while following the system.
/** @typedef {{kind: "system"} | {kind: "fixed", theme: string}} AppearanceSelection */
function appearanceSelection(preference) {
  return preference === "system" ? { kind: "system" } : { kind: "fixed", theme: preference };
}
function resolvedAppearance(selection, systemDark) {
  return (selection.kind === "system" ? systemDark : selection.theme === "dark") ? "dark" : "light";
}

export function useAppearance() {
  const [preference, setPreference] = useState(() => {
    let saved;
    try { saved = window.localStorage.getItem("jmodguard.theme"); } catch (_) {}
    return ["system", "light", "dark"].includes(saved) ? saved : "system";
  });
  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => {
      const theme = resolvedAppearance(appearanceSelection(preference), media.matches);
      const dark = theme === "dark";
      document.documentElement.dataset.theme = theme;
      document.querySelector('meta[name="theme-color"]').content = dark ? "#101e33" : "#e0eeff";
    };
    apply();
    media.addEventListener("change", apply);
    return () => media.removeEventListener("change", apply);
  }, [preference]);
  return { preference, setPreference };
}

export function Appearance({ preference, setPreference }) {
  return React.createElement("label", { className: "theme-picker" }, "Appearance ",
    React.createElement("select", {
      className: "select select-sm", "aria-label": "Appearance", value: preference,
      onChange: event => {
        const value = event.target.value;
        setPreference(value);
        try { window.localStorage.setItem("jmodguard.theme", value); } catch (_) {}
      },
    }, ...["system", "light", "dark"].map(value =>
      React.createElement("option", { key: value, value }, value[0].toUpperCase() + value.slice(1)))));
}
