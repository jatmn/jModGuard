#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = fs.readFileSync(new URL('../frontend/app.js', import.meta.url), 'utf8');
// Minimal DOM boundary: assertions inspect the actual children/attributes that
// production el() sends to the browser, without duplicating its implementation.
function harness(fetchImpl = async () => {}, storage = new Map()) {
  const elements = [];
  const node = (tag) => ({
    tag, nodeType: 1, children: [], attrs: {}, dataset: {}, listeners: {}, disabled: false,
    classList: { toggle() {} },
    append(...children) { this.children.push(...children); },
    setAttribute(k, v) { this.attrs[k] = v; if (k === "value") this.value = String(v); },
    removeAttribute(k) { delete this.attrs[k]; },
    addEventListener(k, v) { this.listeners[k] = v; },
    querySelectorAll() { return []; },
    querySelector() { return null; },
  });
  const nodes = new Map(['#content', '#toast', '#workspace-name', '#workspace-status'].map((key) => [key, node('div')]));
  const context = vm.createContext({
    document: {
      createElement(tag) { const n = node(tag); elements.push(n); return n; },
      createTextNode(text) { return { nodeType: 3, textContent: String(text) }; },
      querySelector: (key) => nodes.get(key), querySelectorAll: () => [],
    },
    window: { localStorage: { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, v) } },
    fetch: fetchImpl, console, AbortController, setTimeout: () => 0, clearTimeout() {}, confirm: () => true,
  });
  const app = vm.runInContext(`${source}\n({state, el, restoreWorkspace, persistWorkspace, workspaceKey, loadConfig, handleEvent, tabSettings, isBusy: () => busy})`, context);
  app.state.me = { user: { id: '123456789012345678' } };
  app.state.guilds = [{ id: '987654321098765432', name: 'First' }, { id: '987654321098765433', name: 'Second' }];
  app.state.guildId = app.state.guilds[0].id;
  return { app, elements, nodes, storage };
}
const response = (data) => ({ ok: true, text: async () => JSON.stringify(data) });

test('workspace survives a fresh JS context and keeps snowflakes exact', () => {
  const first = harness();
  first.app.state.guildId = first.app.state.guilds[1].id;
  first.app.state.tab = 'settings';
  first.app.persistWorkspace();
  const second = harness(undefined, first.storage);
  second.app.restoreWorkspace();
  assert.equal(second.app.state.guildId, '987654321098765433');
  assert.equal(second.app.state.tab, 'settings');
});

test('workspace validates membership, tab, user identity and malformed storage', () => {
  const { app, storage } = harness();
  storage.set(app.workspaceKey(), JSON.stringify({ guildId: '0', tab: 'bogus' }));
  app.restoreWorkspace();
  assert.equal(app.state.guildId, app.state.guilds[0].id);
  assert.equal(app.state.tab, 'dashboard');
  app.state.guildId = app.state.guilds[1].id;
  app.persistWorkspace();
  app.state.me.user.id = 'someone-else';
  app.restoreWorkspace();
  assert.equal(app.state.guildId, app.state.guilds[0].id);
  storage.set(app.workspaceKey(), '{');
  assert.doesNotThrow(() => app.restoreWorkspace());
  app.state.guilds = [];
  app.restoreWorkspace();
  assert.equal(app.state.guildId, null);
});

test('blocked browser storage does not block navigation or restoration', () => {
  const { app } = harness(undefined, { get() { throw new Error('blocked'); }, set() { throw new Error('blocked'); } });
  assert.doesNotThrow(() => app.restoreWorkspace());
  assert.doesNotThrow(() => app.persistWorkspace());
  assert.equal(app.state.guildId, app.state.guilds[0].id);
});

test('dropdown option arrays become real nodes and boolean false stays false', () => {
  const { app } = harness();
  const options = ['log', 'warn', 'ban'].map((value) => app.el('option', { value, selected: value === 'ban' }));
  const select = app.el('select', {}, options);
  assert.equal(select.children.length, 3);
  assert.equal(select.children[2], options[2]);
  assert.equal(select.children[2].selected, true);
  const button = app.el('button', { hidden: false, disabled: false });
  assert.equal(button.hidden, false);
  assert.equal(button.disabled, false);
  assert.equal(button.attrs.hidden, undefined);
});

test('out-of-order server responses never overwrite the active server', async () => {
  const pending = [];
  const { app } = harness((url) => new Promise((resolve) => pending.push({ url, resolve })));
  app.state.config = { sentinel: 'old' };
  const first = app.loadConfig();
  app.state.guildId = app.state.guilds[1].id;
  const second = app.loadConfig();
  pending[1].resolve(response({ sentinel: 'second' }));
  await second;
  pending[0].resolve(response({ sentinel: 'first' }));
  await first;
  assert.equal(app.state.config.sentinel, 'second');
  assert.equal(app.state.loading, false);
});

test('an older same-server refresh cannot roll back a newer saved snapshot', async () => {
  const pending = [];
  const { app } = harness(() => new Promise((resolve) => pending.push(resolve)));
  app.state.config = {};
  const old = app.loadConfig(), fresh = app.loadConfig();
  pending[1](response({ settings: { prefix: '?' } }));
  await fresh;
  pending[0](response({ settings: { prefix: '!' } }));
  await old;
  assert.equal(app.state.config.settings.prefix, '?');
});

test('failed refresh clears stale settings and exposes a retryable error', async () => {
  const { app } = harness(async () => { throw new Error('offline'); });
  app.state.config = { settings: { prefix: 'stale' } };
  assert.equal(await app.loadConfig(), undefined);
  assert.equal(app.state.config, null);
  assert.equal(app.state.error, 'offline');
  assert.equal(app.state.loading, false);
});

test('async actions block duplicate submissions and release after errors', async () => {
  const { app, nodes } = harness();
  let reject;
  let calls = 0;
  const run = () => { calls++; return new Promise((_, fail) => { reject = fail; }); };
  const pending = app.handleEvent({}, run, {});
  assert.equal(app.isBusy(), true);
  await app.handleEvent({}, run, {});
  assert.equal(calls, 1);
  reject(new Error('save refused'));
  await pending;
  assert.equal(app.isBusy(), false);
  assert.equal(nodes.get('#toast').textContent, 'save refused');
});

function settingsHarness() {
  const calls = [];
  const cfg = { settings: { prefix: '!', bot_name: 'Guard', retention_days: 30 }, settings_override: { prefix: '!', retention_days: 30 }, global_settings: { prefix: '/' } };
  const h = harness(async (url, options) => {
    calls.push({ url, method: options.method, body: options.body ? JSON.parse(options.body) : null });
    return response(options.method === 'GET' ? cfg : { nickname_queued: false });
  });
  h.app.state.config = cfg;
  h.app.state.tab = 'settings';
  h.app.tabSettings();
  const inputs = h.elements.filter(n => n.tag === 'input');
  const save = h.elements.find(n => n.tag === 'button' && n.textContent === 'Save');
  return { ...h, calls, inputs, save };
}

test('settings patches send only changed fields to the selected server', async () => {
  const { calls, inputs, save } = settingsHarness();
  inputs[0].value = '?';
  await save.listeners.click({});
  assert.deepEqual(calls[0], { url: '/api/guilds/987654321098765432/settings', method: 'PUT', body: { prefix: '?' } });
  assert.equal(calls[1].method, 'GET');
});

test('settings keep clear-to-inherit, empty nickname, and zero retention distinct', async () => {
  const { calls, inputs, save } = settingsHarness();
  inputs[0].value = '';
  inputs[1].value = '';
  inputs[2].value = '0';
  await save.listeners.click({});
  assert.deepEqual(calls[0].body, { prefix: null, bot_name: '', retention_days: 0 });
});

test('fractional retention is rejected before a save request', async () => {
  const { calls, inputs, save } = settingsHarness();
  inputs[2].value = '1.5';
  await save.listeners.click({});
  assert.equal(calls.length, 0);
});
