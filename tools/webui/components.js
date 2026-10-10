// Use the pinned component entries so unrelated Headless component initializers
// do not enter the browser assets. Package layout changes fail at build time.
export { Dialog, DialogPanel } from "./node_modules/@headlessui/react/dist/components/dialog/dialog.js";
export { Field } from "./node_modules/@headlessui/react/dist/components/field/field.js";
export { Label } from "./node_modules/@headlessui/react/dist/components/label/label.js";
export { Input as HeadlessInput } from "./node_modules/@headlessui/react/dist/components/input/input.js";
export { Select as HeadlessSelect } from "./node_modules/@headlessui/react/dist/components/select/select.js";
export { Textarea as HeadlessTextarea } from "./node_modules/@headlessui/react/dist/components/textarea/textarea.js";
