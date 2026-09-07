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
    focus() {},
    append(...children) { children.forEach((child) => { child.parent = this; this.children.push(child); }); },
    set innerHTML(_) { this.children.forEach((child) => { child.parent = null; }); this.children = []; },
    remove() { if (this.parent) this.parent.children = this.parent.children.filter((child) => child !== this); this.parent = null; },
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
      querySelector: (key) => nodes.get(key) || (key === '.modal-overlay' ? findNode(nodes.get('#content'), (n) => n.className === 'modal-overlay') : null), querySelectorAll: () => [],
    },
    window: { localStorage: { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, v) } },
    fetch: fetchImpl, console, AbortController, setTimeout: () => 0, clearTimeout() {}, confirm: () => true,
  });
  const app = vm.runInContext(`${source}\n({state, el, restoreWorkspace, persistWorkspace, workspaceKey, loadConfig, handleEvent, tabDashboard, tabSettings, editRule, editCommand, captureDrafts, retainDrafts, restoreDrafts, navigate, isDirty: () => dirty, resetDirty: () => { dirty = false; }, isBusy: () => busy})`, context);
  app.state.me = { user: { id: '123456789012345678' } };
  app.state.guilds = [{ id: '987654321098765432', name: 'First' }, { id: '987654321098765433', name: 'Second' }];
  app.state.guildId = app.state.guilds[0].id;
  return { app, elements, nodes, storage };
}
const response = (data) => ({ ok: true, text: async () => JSON.stringify(data) });

function findNode(root, predicate) {
  if (predicate(root)) return root;
  for (const child of root.children || []) {
    const found = findNode(child, predicate);
    if (found) return found;
  }
  return null;
}

function textOf(root) {
  return (root.textContent || '') + (root.children || []).map(textOf).join('');
}

test('dashboard moderation counts stay with their requested server', async () => {
  const pending = [];
  const { app } = harness((url) => new Promise(resolve => pending.push({ url, resolve })));
  app.state.config = { settings: { prefix: '!', bot_name: '' }, rules: [], commands: [], log_channels: [], badwords: [] };
  const first = app.tabDashboard();
  app.state.guildId = app.state.guilds[1].id;
  const second = app.tabDashboard();
  assert.equal(pending[0].url, `/api/guilds/${app.state.guilds[0].id}/stats`);
  assert.equal(pending[1].url, `/api/guilds/${app.state.guilds[1].id}/stats`);
  const value = (root, label) => findNode(root, n => n.className === 'stat' && textOf(n).includes(label)).children[0].textContent;
  pending[1].resolve(response({ events: 7, warns: 2, bans: 1 }));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(value(second, 'Moderation records'), '7');
  assert.equal(value(second, 'Warns'), '2');
  assert.equal(value(second, 'Bans'), '1');
  pending[0].resolve(response({ events: 900, warns: 8, bans: 3 }));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(value(first, 'Moderation records'), '900');
  assert.equal(value(second, 'Moderation records'), '7');
});

test('dashboard does not show zero moderation counts when stats are unavailable', async () => {
  const { app } = harness(async () => { throw new Error('offline'); });
  app.state.config = { settings: { prefix: '!', bot_name: '' }, rules: [], commands: [], log_channels: [], badwords: [] };
  const dashboard = app.tabDashboard();
  await new Promise(resolve => setImmediate(resolve));
  for (const label of ['Moderation records', 'Warns', 'Bans']) {
    const card = findNode(dashboard, n => n.className === 'stat' && textOf(n).includes(label));
    assert.equal(card.children[0].textContent, 'Unavailable');
  }
});

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

test('saved Members tab falls back to the dashboard after history removal', () => {
  const { app, storage } = harness();
  const guildId = app.state.guilds[1].id;
  storage.set(app.workspaceKey(), JSON.stringify({ guildId, tab: 'members' }));
  app.restoreWorkspace();
  assert.equal(app.state.guildId, guildId);
  assert.equal(app.state.tab, 'dashboard');
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

test('settings keep clear-to-inherit and empty nickname distinct', async () => {
  const { calls, inputs, save } = settingsHarness();
  inputs[0].value = '';
  inputs[1].value = '';
  await save.listeners.click({});
  assert.deepEqual(calls[0].body, { prefix: null, bot_name: '' });
});

test('settings do not expose historical chat-retention controls', () => {
  const { elements, inputs } = settingsHarness();
  assert.equal(inputs.filter(n => n.attrs.type === 'number').length, 0);
  assert.ok(!elements.some(n => /retention|keep forever/i.test(n.textContent || '')));
});

test('partial settings actions preserve dirty sibling fields in the same card', () => {
  const { app, nodes } = harness();
  const prefix = { value: '?', _initialValue: '!', dataset: { inputKey: '0' } };
  const nickname = { value: 'Draft nickname', _initialValue: 'Guard', dataset: { inputKey: '1' } };
  nodes.get('#content').querySelectorAll = () => [prefix, nickname];
  const form = { contains: () => true };
  const resetPrefix = { closest: () => form, _savedInputs: [prefix] };
  const resync = { closest: () => form, _savedInputs: [] };
  assert.deepEqual(JSON.parse(JSON.stringify(app.captureDrafts(resetPrefix))), [{ key: '1', value: 'Draft nickname' }]);
  assert.equal(app.captureDrafts(resync).length, 2);
  assert.equal(app.captureDrafts({ closest: () => form }).length, 0);
});

test('unrelated drafts survive the unavailable screen and restore after Retry', () => {
  const { app, nodes } = harness();
  const input = { value: 'Saved text', dataset: { inputKey: '0' } };
  app.state.config = null;
  app.retainDrafts(app.state.guildId, app.state.tab, [{ key: '0', value: 'Unsaved text' }]);
  assert.equal(app.isDirty(), true);
  app.restoreDrafts(); // Loading/error screen still has no input.
  app.state.config = {};
  nodes.get('#content').querySelector = () => input;
  app.restoreDrafts();
  assert.equal(input.value, 'Unsaved text');
  assert.equal(nodes.get('#workspace-status').textContent, 'Unsaved changes');
});

test('confirmed navigation clears pending drafts and prevents later replay', () => {
  const { app, nodes } = harness();
  const originalTab = app.state.tab;
  app.state.config = null;
  app.retainDrafts(app.state.guildId, originalTab, [{ key: '0', value: 'Old draft' }]);
  app.navigate('settings');
  app.state.tab = originalTab;
  app.state.config = {};
  const input = { value: 'Server value' };
  nodes.get('#content').querySelector = () => input;
  app.restoreDrafts();
  assert.equal(input.value, 'Server value');
  assert.equal(app.isDirty(), false);
});

for (const kind of ['rule', 'command']) {
  for (const action of ['add', 'remove']) {
    test(`${kind} structural ${action} marks the editor dirty`, async () => {
      const { app, elements } = harness();
      if (kind === 'rule') app.editRule({ name: 'Flood', rule_id: 'flood', effective: { params: {}, actions: [{ at: 1, action: 'warn' }] } });
      else app.editCommand(null);
      assert.equal(app.isDirty(), false, 'opening the editor does not modify it');
      const button = action === 'add'
        ? elements.find(n => n.tag === 'button' && (n.textContent === '+ add step' || n.children.some(c => c.textContent === '+ add action')))
        : elements.find(n => n.tag === 'button' && (n.textContent === '✕' || n.children.some(c => c.textContent === '✕')));
      assert.ok(button);
      await button.listeners.click({});
      assert.equal(app.isDirty(), true);
    });
  }
}

function ruleSaveHarness({ scope = 'global', override = false } = {}) {
  const calls = [];
  const failures = { put: false, delete: false, refresh: false };
  let globalRule = { enabled: true, dry_run: true, params: {}, actions: [{ at: 1, action: 'warn' }] };
  let guildRule = override ? structuredClone(globalRule) : null;
  const snapshot = () => ({ rules: [{
    rule_id: 'mention_spam', name: 'Mention spam', global: globalRule,
    guild_override: guildRule, effective: guildRule || globalRule,
  }] });
  const h = harness(async (url, options) => {
    const body = options.body && JSON.parse(options.body);
    calls.push({ url, method: options.method, body });
    if (options.method === 'PUT') {
      if (failures.put) throw new Error('write refused');
      if (body.scope === 'global') globalRule = body.data;
      else guildRule = body.data;
    } else if (options.method === 'DELETE') {
      if (failures.delete) throw new Error('cleanup unavailable');
      guildRule = null;
    } else {
      if (failures.refresh) throw new Error('refresh unavailable');
      return response(snapshot());
    }
    return response({ ok: true });
  });
  h.app.state.me.is_bot_owner = true;
  h.app.state.config = structuredClone(snapshot());
  h.app.navigate('rules');
  h.app.editRule(h.app.state.config.rules[0]);
  const content = h.nodes.get('#content');
  const modal = findNode(content, (n) => n.className === 'modal-overlay');
  const scopeInput = findNode(modal, (n) => n.tag === 'select' && n.children.some((c) => c.attrs?.value === 'global'));
  scopeInput.value = scope;
  const enabled = findNode(modal, (n) => n.attrs?.type === 'checkbox');
  const save = findNode(modal, (n) => n.textContent === 'Save');
  const cancel = findNode(modal, (n) => n.textContent === 'Cancel');
  return { ...h, calls, failures, content, modal, enabled, save, cancel, snapshot };
}

test('global rule partial success reconciles the committed value before navigation', async () => {
  const h = ruleSaveHarness();
  h.failures.delete = true;
  await h.enabled.listeners.change({ target: { checked: false } });
  await h.save.listeners.click({});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'GET']);
  assert.equal(h.snapshot().rules[0].effective.enabled, false);
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
  assert.match(h.nodes.get('#toast').textContent, /Global rule saved/);
  assert.match(h.nodes.get('#toast').textContent, /cleanup unavailable/);
  assert.match(h.nodes.get('#toast').className, /error/);
  await h.cancel.listeners.click({});
  h.app.navigate('rules');
  assert.match(textOf(h.content), /disabled/);
  assert.equal(h.app.isBusy(), false);
});

test('global rule partial success with failed reconciliation stays unavailable through Retry', async () => {
  const h = ruleSaveHarness();
  h.failures.delete = h.failures.refresh = true;
  await h.enabled.listeners.change({ target: { checked: false } });
  await h.save.listeners.click({});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'GET']);
  assert.equal(h.app.state.config, null);
  assert.match(textOf(h.content), /Configuration unavailable/);
  assert.match(h.nodes.get('#toast').textContent, /Global rule saved/);
  assert.match(h.nodes.get('#toast').textContent, /cleanup unavailable/);
  assert.match(h.nodes.get('#toast').textContent, /Retry/);
  assert.equal(h.app.isBusy(), false);
  h.app.navigate('rules');
  assert.match(textOf(h.content), /Configuration unavailable/);
  h.failures.refresh = false;
  const retry = findNode(h.content, (n) => n.textContent === 'Retry');
  await retry.listeners.click({});
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
  assert.match(textOf(h.content), /disabled/);
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'GET', 'GET']);
});

test('a refused rule PUT retains the checkbox draft and never deletes an override', async () => {
  const h = ruleSaveHarness();
  h.failures.put = true;
  await h.enabled.listeners.change({ target: { checked: false } });
  await h.save.listeners.click({});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT']);
  assert.equal(findNode(h.content, (n) => n === h.modal), h.modal);
  assert.equal(h.app.state.config.rules[0].effective.enabled, true);
  assert.equal(h.app.isBusy(), false);
  h.failures.put = false;
  await h.save.listeners.click({});
  assert.equal(h.calls.filter((c) => c.method === 'PUT')[1].body.data.enabled, false);
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
});

test('a successful global rule save clears the guild override and refreshes', async () => {
  const h = ruleSaveHarness({ override: true });
  await h.enabled.listeners.change({ target: { checked: false } });
  await h.save.listeners.click({});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'GET']);
  assert.equal(h.calls[0].body.scope, 'global');
  assert.equal(h.calls[1].body.scope, 'guild');
  assert.equal(h.app.state.config.rules[0].guild_override, null);
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
  assert.equal(findNode(h.content, (n) => n === h.modal), null);
  assert.equal(h.nodes.get('#toast').textContent, 'Rule saved');
});

test('a guild-only rule save does not run global override cleanup', async () => {
  const h = ruleSaveHarness({ scope: 'guild' });
  await h.enabled.listeners.change({ target: { checked: false } });
  await h.save.listeners.click({});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'GET']);
  assert.equal(h.calls[0].body.scope, 'guild');
  assert.equal(h.app.state.config.rules[0].global.enabled, true);
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
});

test('a refused command save retains its checkbox value for the next save', async () => {
  const calls = [];
  let refuse = true;
  const h = harness(async (url, options) => {
    if (options.method === 'GET') return response({ commands: [] });
    calls.push(JSON.parse(options.body));
    if (refuse) throw new Error('write refused');
    return response({ ok: true });
  });
  h.app.state.config = { commands: [] };
  h.app.navigate('commands');
  h.app.editCommand(null);
  const content = h.nodes.get('#content');
  const modal = findNode(content, (n) => n.className === 'modal-overlay');
  const name = findNode(modal, (n) => n.attrs?.placeholder === 'command name');
  name.value = 'hello';
  const enabled = findNode(modal, (n) => n.attrs?.type === 'checkbox');
  const save = findNode(modal, (n) => n.textContent === 'Save');
  await enabled.listeners.change({ target: { checked: false } });
  await save.listeners.click({});
  assert.equal(findNode(content, (n) => n === modal), modal);
  assert.equal(h.app.isBusy(), false);
  refuse = false;
  await save.listeners.click({});
  assert.equal(calls.length, 2);
  assert.equal(calls[0].data.enabled, false);
  assert.equal(calls[1].data.enabled, false);
  assert.equal(findNode(content, (n) => n === modal), null);
});
