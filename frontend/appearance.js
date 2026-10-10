import React, { useEffect, useState } from "react";

export function useAppearance() {
  const [preference, setPreference] = useState(() => {
    let saved;
    try { saved = window.localStorage.getItem("jmodguard.theme"); } catch (_) {}
    return ["system", "light", "dark"].includes(saved) ? saved : "system";
  });
  useEffect(() => {
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const apply = () => {
      const dark = preference === "dark" || (preference === "system" && media.matches);
      document.documentElement.dataset.theme = dark ? "dark" : "light";
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
