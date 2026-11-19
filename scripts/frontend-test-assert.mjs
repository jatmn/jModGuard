// node:assert/strict for the React DOM test files, with bounded object comparison failures.
// node:assert reports a failed object comparison by inspecting both values 1000 levels deep
// and diffing the lines; Node 24 and later do so even when a message is given. A mounted
// DOM node reaches React's whole fiber tree, so one failed `assert.equal(node, null)` grew
// to gigabytes outside the JS heap without returning (#709). Object comparisons here fail
// with a short preview instead.
import assert from 'node:assert/strict';
import { inspect, isDeepStrictEqual, types } from 'node:util';

const isReference = value => (value !== null && typeof value === 'object') || typeof value === 'function';
const maxProperties = 4;

function truncate(value, limit = 80) {
  return value.length > limit ? `${value.slice(0, limit)}…` : value;
}

function previewProperty(property) {
  if (!property) return '<empty>';
  return 'value' in property ? preview(property.value, false) : '[Accessor]';
}

function preview(value, expand = true) {
  if (types.isProxy(value)) return '[Proxy]';
  if (typeof value === 'function') {
    const name = Object.getOwnPropertyDescriptor(value, 'name')?.value;
    return typeof name === 'string' && name ? `[Function ${truncate(name)}]` : '[Function]';
  }
  if (typeof value === 'string') return JSON.stringify(truncate(value));
  if (typeof value === 'symbol') return `Symbol(${truncate(value.description ?? '')})`;
  if (!isReference(value)) return inspect(value);

  // Expand only the outer object. Descriptors keep getters and inspection hooks out of the preview.
  const array = Array.isArray(value);
  if (!expand) return array ? '[Array]' : '[Object]';
  const Element = globalThis.window?.Element;
  if (Element && value instanceof Element) {
    const className = typeof value.className === 'string' && value.className ? ` class="${truncate(value.className)}"` : '';
    return `<${truncate(value.localName)}${className}> ${JSON.stringify(truncate(value.textContent))}`;
  }

  const fields = [];
  if (array) {
    for (let index = 0; index < Math.min(value.length, maxProperties); index++) {
      fields.push(previewProperty(Object.getOwnPropertyDescriptor(value, index)));
    }
    if (value.length > maxProperties) fields.push('…');
    return `[${fields.join(', ')}]`;
  }
  for (const key in value) {
    const property = Object.getOwnPropertyDescriptor(value, key);
    if (!property) break;
    if (fields.length === maxProperties) {
      fields.push('…');
      break;
    }
    fields.push(`${truncate(key)}: ${previewProperty(property)}`);
  }
  return `{${fields.join(', ')}}`;
}

function bounded(name, matches, relation) {
  const original = assert[name];
  return function compare(actual, expected, message) {
    if (!isReference(actual) && !isReference(expected)) return original(actual, expected, message);
    if (matches(actual, expected)) return;
    if (message instanceof Error) throw message;
    const summary = `Expected ${truncate(preview(actual), 300)} ${relation} ${truncate(preview(expected), 300)}`;
    throw new assert.AssertionError({ message: message ? `${message}\n${summary}` : summary, stackStartFn: compare });
  };
}

const notSame = (actual, expected) => !Object.is(actual, expected);
const notDeep = (actual, expected) => !isDeepStrictEqual(actual, expected);
const comparisons = {
  equal: bounded('equal', Object.is, 'to be'),
  strictEqual: bounded('strictEqual', Object.is, 'to be'),
  deepEqual: bounded('deepEqual', isDeepStrictEqual, 'to deeply equal'),
  deepStrictEqual: bounded('deepStrictEqual', isDeepStrictEqual, 'to deeply equal'),
  notEqual: bounded('notEqual', notSame, 'not to be'),
  notStrictEqual: bounded('notStrictEqual', notSame, 'not to be'),
  notDeepEqual: bounded('notDeepEqual', notDeep, 'not to deeply equal'),
  notDeepStrictEqual: bounded('notDeepStrictEqual', notDeep, 'not to deeply equal'),
};

// Calling assert(value) itself keeps node:assert's own source-based message.
export default new Proxy(assert, {
  get: (target, key, receiver) => (Object.hasOwn(comparisons, key) ? comparisons[key] : Reflect.get(target, key, receiver)),
});
