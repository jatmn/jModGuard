// Use 1Password's documented field opt-out on non-login controls.
// Sign-in and token fields should set their own autocomplete value.
export function noAutofill(props) {
  return {
    ...props,
    autoComplete: "off",
    "data-1p-ignore": "",
    // Best-effort LastPass compatibility hint.
    "data-lpignore": "true",
    // Bitwarden honors this best-effort hint only with its autofill exclusion
    // setting enabled. The setting is off by default.
    "data-bwignore": "",
    // Dashlane's documented field autofill opt-out.
    "data-form-type": "other",
  };
}
