// Sign-in and token fields should set their own autocomplete value.
export function noAutofill(props) {
  return {
    ...props,
    autoComplete: "off",
    "data-1p-ignore": "",
    "data-lpignore": "true",
    "data-bwignore": "",
    "data-form-type": "other",
  };
}
