// node:assert/strict for the React DOM test files, with bounded object comparison failures.
// node:assert reports a failed object comparison by inspecting both values 1000 levels deep
// and diffing the lines; Node 24 and later do so even when a message is given. A mounted
// DOM node reaches React's whole fiber tree, so one failed `assert.equal(node, null)` grew
// to gigabytes outside the JS heap without returning (#709). Object comparisons here fail
// with a short preview instead.
import assert from 'node:assert/strict';
import { inspect, isDeepStrictEqual } from 'node:util';

const isObject = value => value !== null && typeof value === 'object';

function preview(value) {
  if (isObject(value) && value.nodeType === 1 && typeof value.localName === 'string') {
    const className = typeof value.className === 'string' && value.className ? ` class="${value.className}"` : '';
    return `<${value.localName}${className}> ${JSON.stringify(String(value.textContent).slice(0, 80))}`;
  }
  return inspect(value, { depth: 1, maxArrayLength: 10, maxStringLength: 80, breakLength: Infinity }).slice(0, 300);
}

function bounded(name, matches, relation) {
  const original = assert[name];
  return function compare(actual, expected, message) {
    if (!isObject(actual) && !isObject(expected)) return original(actual, expected, message);
    if (matches(actual, expected)) return;
    if (message instanceof Error) throw message;
    const summary = `Expected ${preview(actual)} ${relation} ${preview(expected)}`;
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
