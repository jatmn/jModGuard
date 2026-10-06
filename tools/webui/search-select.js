// Import only the exports consumed by the existing search-select adapter.
import { createElement, useState } from "react";
import { createRoot } from "react-dom/client";
import { Combobox, ComboboxButton, ComboboxInput, ComboboxOption, ComboboxOptions } from "@headlessui/react";

export const React = { createElement, useState };
export const ReactDOM = { createRoot };
export const Headless = { Combobox, ComboboxButton, ComboboxInput, ComboboxOption, ComboboxOptions };
