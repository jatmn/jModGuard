#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import { randomUUID } from 'node:crypto';

const source = fs.readFileSync(new URL('../frontend/app.js', import.meta.url), 'utf8');
// Minimal DOM boundary: assertions inspect the actual children/attributes that
// production el() sends to the browser, without duplicating its implementation.
function harness(fetchImpl = async () => {}, storage = new Map(), session = new Map(), confirm = () => true) {
  const elements = [];
  const location = {
    search: '', pathname: '/', hash: '', replaced: null,
    replace(value) { this.replaced = value; },
  };
  const opened = [];
  const historyUrls = [];
  const node = (tag) => {
    const classes = new Set();
    return {
      tag, tagName: String(tag).toUpperCase(), nodeType: 1, children: [], attrs: {}, dataset: {}, style: {}, listeners: {}, disabled: false,
      classList: {
        toggle(name, force) {
          const enabled = force === undefined ? !classes.has(name) : !!force;
          if (enabled) classes.add(name); else classes.delete(name);
          return enabled;
        },
        add(name) { classes.add(name); },
        remove(name) { classes.delete(name); },
        contains(name) { return classes.has(name); },
      },
      focus() {},
      replaceChildren(...children) { this.innerHTML = ""; this.append(...children); },
      append(...children) { children.forEach((child) => { child.parent = this; this.children.push(child); }); },
      set innerHTML(_) { this.children.forEach((child) => { child.parent = null; }); this.children = []; },
      remove() { if (this.parent) this.parent.children = this.parent.children.filter((child) => child !== this); this.parent = null; },
      replaceWith(replacement) {
        if (!this.parent) return;
        const index = this.parent.children.indexOf(this);
        if (index >= 0) this.parent.children.splice(index, 1, replacement);
        replacement.parent = this.parent;
        this.parent = null;
      },
      contains(target) { for (let p = target; p; p = p.parent) if (p === this) return true; return false; },
      closest(selector) {
        const wanted = selector.split(',').map((part) => part.trim().replace(/^\./, ''));
        for (let p = this; p; p = p.parent) {
          const classes = String(p.className || '').split(/\s+/);
          if (wanted.some((name) => classes.includes(name))) return p;
        }
        return null;
      },
      setAttribute(k, v) { this.attrs[k] = v; if (k === "value") this.value = String(v); if (k === "href") this.href = String(v); },
      getAttribute(k) { return this.attrs[k] ?? (k === "href" ? this.href : undefined); },
      removeAttribute(k) { delete this.attrs[k]; },
      addEventListener(k, v) { this.listeners[k] = v; },
      querySelectorAll(selector) {
        const descendants = [];
        const visit = (parent) => (parent.children || []).forEach((child) => {
          if (child?.nodeType === 1) { descendants.push(child); visit(child); }
        });
        visit(this);
        if (selector === 'input, select, textarea') return descendants.filter((n) => ['input', 'select', 'textarea'].includes(n.tag));
        if (selector === '.field') return descendants.filter((n) => String(n.className || '').split(/\s+/).includes('field'));
        return [];
      },
      querySelector(selector) {
        const keyed = selector.match(/^\[data-input-key="([^"]+)"\]$/);
        if (keyed) return this.querySelectorAll('input, select, textarea').find((n) => n.dataset.inputKey === keyed[1]) || null;
        if (selector === 'label') return findNode(this, (n) => n !== this && n.tag === 'label');
        if (selector === 'input, select, textarea') return findNode(this, (n) => n !== this && ['input', 'select', 'textarea'].includes(n.tag));
        return null;
      },
    };
  };
  const nodes = new Map(['#content', '#toast', '#workspace-name', '#workspace-status', '#owner-tools', '#startup-view', '#login-view', '#main-view', '#startup-status', '#preview-return-link', '#jmodguard-daisyui'].map((key) => [key, node('div')]));
  const daisy = nodes.get('#jmodguard-daisyui');
  daisy.tag = 'link';
  daisy.tagName = 'LINK';
  daisy.attrs.href = 'https://cdn.jsdelivr.net/npm/daisyui@5.7.43/daisyui.css';
  daisy.href = daisy.attrs.href;
  const win = { localStorage: { getItem: (k) => storage.get(k) ?? null, setItem: (k, v) => storage.set(k, v) }, sessionStorage: { getItem: k => session.get(k) ?? null, setItem: (k,v) => session.set(k,v), removeItem: k => session.delete(k) }, location, open(...args) { opened.push(args); } };
  for (const key of ['#startup-view', '#login-view', '#main-view', '#preview-return-link']) {
    nodes.get(key).classList.add('hidden');
  }
  const context = vm.createContext({
    document: {
      createElement(tag) { const n = node(tag); elements.push(n); return n; },
      createTextNode(text) { return { nodeType: 3, textContent: String(text) }; },
      getElementById: (id) => nodes.get('#' + id) || null,
      querySelector: (key) => nodes.get(key) || (key === '.modal-overlay' ? findNode(nodes.get('#content'), (n) => String(n.className).split(/\s+/).includes('modal-overlay')) : null), querySelectorAll: () => [],
    },
    window: win,
    location,
    history: { replaceState(_state, _unused, url) { historyUrls.push(url); } },
    fetch: fetchImpl, console, AbortController, URLSearchParams, setTimeout: () => 0, clearTimeout() {}, confirm, crypto: { randomUUID },
  });
  const app = vm.runInContext(`${source}\n({state, el, uiClass, api, apiRead, init, downloadPrivate, submitBotAction, applyQueuedLogChannel, reconcileQueuedLogChannel, actionReconcileDelays, tabActions, tabCases, tabTemplates, tabLogChannels, restoreWorkspace, persistWorkspace, workspaceKey, loadConfig, applyConfigPatch, reloadAfter, handleEvent, tabDashboard, tabSettings, renderTab, ruleCard, imageTestConfigured, spamConfigured, editRule, editCommand, currentDrafts, captureDrafts, retainDrafts, restoreDrafts, navigate, capturePreviewToken, currentPreviewToken, returnFromPreview, bindOwnerTools, isPreview, parseSnowflake, splitIds, handleAuthLoss, markDirty, isDirty: () => dirty, resetDirty: () => { dirty = false; }, isBusy: () => busy, VENDOR, vendorCssHref, vendorEsmBase, vendorWidgetsEnabled, shouldEnhanceSelect, vendorModule, applyVendorCss})`, context);
  app.state.me = { user: { id: '123456789012345678' } };
  app.state.guilds = [{ id: '987654321098765432', name: 'First' }, { id: '987654321098765433', name: 'Second' }];
  app.state.guildId = app.state.guilds[0].id;
  return { app, elements, nodes, storage, session, location, opened, historyUrls, window: win };
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
  const { app } = harness((url, options) => new Promise(resolve => pending.push({ url, method: options.method, resolve })));
  app.state.config = { settings: { prefix: '!', bot_name: '' }, rules: [], commands: [], log_channels: [], badwords: [] };
  const first = app.tabDashboard();
  app.state.guildId = app.state.guilds[1].id;
  const second = app.tabDashboard();
  assert.equal(pending[0].url, `/api/guilds/${app.state.guilds[0].id}/stats`);
  assert.equal(pending[1].url, `/api/guilds/${app.state.guilds[1].id}/stats`);
  assert.equal(pending[0].method, 'POST');
  assert.equal(pending[1].method, 'POST');
  const value = (root, label) => findNode(root, n => n.className === 'stat' && textOf(n).includes(label)).children.find((c) => c.className === 'stat-value').textContent;
  pending[1].resolve(response({ events: 7, warns: 2, bans: 1, cases: 4 }));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(value(second, 'Moderation records'), '7');
  assert.equal(value(second, 'Warns'), '2');
  assert.equal(value(second, 'Bans'), '1');
  assert.equal(value(second, 'Cases'), '4');
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
  for (const label of ['Moderation records', 'Warns', 'Bans', 'Cases']) {
    const card = findNode(dashboard, n => n.className === 'stat' && textOf(n).includes(label));
    assert.equal(card.children.find((c) => c.className === 'stat-value').textContent, 'Unavailable');
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

test('cases is a valid navigation and restored workspace tab', () => {
  const first = harness();
  first.app.state.config = null;
  first.app.navigate('cases');
  assert.equal(first.app.state.tab, 'cases');
  first.app.persistWorkspace();

  const second = harness(undefined, first.storage);
  second.app.restoreWorkspace();
  assert.equal(second.app.state.tab, 'cases');
});

test('template help describes bounded delete snippets without claiming history', () => {
  const { app } = harness();
  app.state.config = { templates: [] };
  const text = textOf(app.tabTemplates());
  assert.match(text, /single-message delete may supply \{content\}/);
  assert.match(text, /bounded recent in-process snippet/);
  assert.match(text, /otherwise those values are unavailable/);
  assert.match(text, /not persistent history or a Discord history lookup/);
  assert.doesNotMatch(text, /deleted content are not retained/);
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

test('an older refresh cannot roll back an acknowledged mutation representation', async () => {
  let resolveRefresh;
  const { app } = harness(() => new Promise((resolve) => { resolveRefresh = resolve; }));
  app.state.config = {
    rules: [{ rule_id: 'mention_spam', global: { enabled: true }, guild_override: null, effective: { enabled: true } }],
  };
  const old = app.loadConfig();
  assert.equal(app.applyConfigPatch({
    config_patch: {
      kind: 'rule', id: 'mention_spam', scope: 'guild', value: { enabled: false },
    },
  }), true);
  resolveRefresh(response({
    rules: [{ rule_id: 'mention_spam', global: { enabled: true }, guild_override: null, effective: { enabled: true } }],
  }));
  await old;
  assert.equal(app.state.config.rules[0].effective.enabled, false);
  assert.equal(app.state.loading, false);
});

test('default-backed rule and template resets fall back instead of applying null', () => {
  const { app } = harness();
  const rule = { enabled: true };
  app.state.config = {
    rules: [{ rule_id: 'mention_spam', global: null, guild_override: rule, effective: rule }],
    templates: [{ event: 'member_join', global: null, guild_override: 'Guild text', effective: 'Guild text' }],
  };
  assert.equal(app.applyConfigPatch({
    config_patch: { kind: 'rule', id: 'mention_spam', scope: 'guild', value: null },
  }), false);
  assert.equal(app.state.config.rules[0].effective, rule);
  assert.equal(app.applyConfigPatch({
    config_patch: { kind: 'template', event: 'member_join', scope: 'guild', value: null },
  }), false);
  assert.equal(app.state.config.templates[0].effective, 'Guild text');
});

test('bot-name settings patches replace stale nickname reconciliation status', () => {
  const { app } = harness();
  app.state.config = {
    settings: { prefix: '!', bot_name: 'Old' },
    settings_override: { bot_name: 'Old' },
    global_settings: { prefix: '/', bot_name: '' },
    nickname_reconcile: { state: 'terminal', last_error: 'old failure' },
  };
  assert.equal(app.applyConfigPatch({
    config_patch: {
      kind: 'settings',
      scope: 'guild',
      settings: { prefix: '!', bot_name: 'New' },
      prefix_present: false,
      bot_name_present: true,
      bot_name: 'New',
      sync_nickname: false,
      nickname_reconcile: {
        state: 'pending', attempt_count: 0, next_attempt_at: 0,
        last_error_code: null, last_error: null, last_error_at: null,
      },
    },
  }), true);
  assert.equal(app.state.config.settings.bot_name, 'New');
  assert.equal(app.state.config.nickname_reconcile.state, 'pending');
  assert.equal(app.state.config.nickname_reconcile.last_error, null);
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
    const body = options.body ? JSON.parse(options.body) : null;
    calls.push({ url, method: options.method, body });
    if (options.method === 'PUT' && url.endsWith('/settings')) {
      if (Object.hasOwn(body, 'prefix')) cfg.settings.prefix = body.prefix ?? cfg.global_settings.prefix;
      if (Object.hasOwn(body, 'bot_name')) cfg.settings.bot_name = body.bot_name;
      return response({
        nickname_queued: false,
        config_patch: {
          kind: 'settings', scope: 'guild', settings: cfg.settings,
          prefix_present: Object.hasOwn(body, 'prefix'), prefix: body.prefix,
          bot_name_present: Object.hasOwn(body, 'bot_name'), bot_name: body.bot_name,
          sync_nickname: false,
          nickname_reconcile: Object.hasOwn(body, 'bot_name')
            ? { state: 'pending', attempt_count: 0, next_attempt_at: 0, last_error_code: null, last_error: null, last_error_at: null }
            : null,
        },
      });
    }
    if (url.endsWith('/case-delete-roles')) return response({ roles: [], revision: '' });
    return response(cfg);
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
  const put = calls.find((c) => c.method === 'PUT' && c.url.endsWith('/settings'));
  assert.deepEqual(put, { url: '/api/guilds/987654321098765432/settings', method: 'PUT', body: { prefix: '?' } });
  assert.equal(calls.filter((c) => c.url.endsWith('/config')).length, 0);
  assert.ok(calls.some((c) => c.method === 'POST' && c.url.endsWith('/case-delete-roles')));
});

test('case-delete-role saves advance their revision token', async () => {
  const calls = [];
  let nextRevision = 2;
  const cfg = { settings: { prefix: '!', bot_name: 'Guard' }, settings_override: {}, global_settings: { prefix: '/' } };
  const h = harness(async (url, options) => {
    const body = options.body ? JSON.parse(options.body) : null;
    calls.push({ url, method: options.method, body });
    if (url.endsWith('/case-delete-roles') && options.method === 'POST') {
      return response({ roles: ['1'], revision: 'revision-1' });
    }
    if (url.endsWith('/case-delete-roles') && options.method === 'PUT') {
      return response({ revision: `revision-${nextRevision++}` });
    }
    return response(cfg);
  });
  h.app.state.config = cfg;
  h.app.state.tab = 'settings';
  h.app.renderTab();
  await new Promise(resolve => setImmediate(resolve));

  const roleInput = h.elements.find(n => n.tag === 'input' && /role id/i.test(n.attrs.placeholder || ''));
  const saveRoles = h.elements.find(n => n.tag === 'button' && n.textContent === 'Save case-removal roles');
  assert.equal(h.app.currentDrafts().length, 0, 'server-loaded roles must become the saved baseline');
  roleInput.value = '1, 2';
  h.app.markDirty();
  await saveRoles.listeners.click({});
  assert.equal(h.app.isDirty(), false, 'an acknowledged role save must not remain unsaved');
  const prefix = h.elements.find(n => n.tag === 'input' && n.value === '!');
  prefix.value = '?';
  h.app.markDirty();
  roleInput.value = '2';
  await saveRoles.listeners.click({});
  assert.equal(h.app.isDirty(), true, 'an unrelated settings draft must remain dirty');

  const puts = calls.filter(c => c.method === 'PUT' && c.url.endsWith('/case-delete-roles'));
  assert.equal(puts.length, 2);
  assert.equal(puts[0].body.revision, 'revision-1');
  assert.equal(puts[1].body.revision, 'revision-2');
});

test('queued log-channel saves clear dirty state and survive in-app navigation', async () => {
  let confirmations = 0;
  let appRef;
  const h = harness(async (url, options) => {
    if (options.method === 'PUT' && url.endsWith('/logchannels/member_join')) {
      return response({
        id: 'log-job', guild_id: appRef.state.guildId, actor_id: appRef.state.me.user.id,
        enrollment_id: 'membership', state: 'pending',
        action: { kind: 'set_log_channel', event: 'member_join', channel_id: '222' },
      });
    }
    return response([]);
  }, new Map(), new Map(), () => { confirmations += 1; return false; });
  appRef = h.app;
  h.app.state.config = {
    enrollment_id: 'membership',
    rules: [], commands: [], badwords: [],
    settings: { prefix: '!', bot_name: '' },
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  h.app.state.tab = 'logchannels';
  h.nodes.get('#content').append(h.app.tabLogChannels());
  const input = h.elements.find((node) => node.tag === 'input' && node.attrs.placeholder === 'channel id');
  const set = h.elements.find((node) => node.tag === 'button' && node.textContent === 'Set');
  input.value = '222';
  h.app.markDirty();
  await set.listeners.click({});
  assert.equal(h.app.state.config.log_channels[0].effective, '222');
  assert.equal(h.app.state.config.log_channels[0].pending_action.id, 'log-job');
  assert.equal(h.app.isDirty(), false);
  h.app.navigate('dashboard');
  assert.equal(confirmations, 0);
  h.app.navigate('logchannels');
  assert.match(textOf(h.nodes.get('#content')), /queued/);
  assert.equal(h.app.state.config.log_channels[0].effective, '222');
});

test('log channel ignore editor saves user and channel IDs as strings', async () => {
  const calls = [];
  const h = harness(async (url, opts) => {
    calls.push({ url, opts });
    return response({ ignore: { users: ['9007199254740993', '9007199254740995'], channels: ['9007199254740997'] } });
  });
  h.app.state.config = {
    enrollment_id: 'membership', rules: [], commands: [], badwords: [],
    settings: { prefix: '!', bot_name: '' },
    log_channels: [{ event: 'message_edit', effective: '111', guild_override: '111', global: null, ignore: { users: [], channels: [] } }],
  };
  h.app.state.tab = 'logchannels';
  h.nodes.get('#content').append(h.app.tabLogChannels());
  const fields = h.elements.filter((node) => node.tag === 'textarea');
  fields[0].value = '9007199254740993\n<@9007199254740995>';
  fields[1].value = '<#9007199254740997>';
  const save = h.elements.find((node) => node.tag === 'button' && node.textContent === 'Save ignores');
  await save.listeners.click({});
  assert.match(calls[0].url, /\/logfilters\/message_edit$/);
  assert.deepEqual(JSON.parse(calls[0].opts.body), { users: ['9007199254740993', '9007199254740995'], channels: ['9007199254740997'] });
  assert.deepEqual(JSON.parse(JSON.stringify(h.app.state.config.log_channels[0].ignore)), { users: ['9007199254740993', '9007199254740995'], channels: ['9007199254740997'] });
  fields[0].value = 'OtherBot';
  await save.listeners.click({});
  assert.equal(calls.length, 1, 'names must be rejected before calling the API');
});

test('queued log-channel receipts reconcile without a configuration refresh', async () => {
  const pending = {
    id: 'log-job', guild_id: '987654321098765432', actor_id: '123456789012345678',
    enrollment_id: 'membership', state: 'pending',
    action: { kind: 'set_log_channel', event: 'member_join', channel_id: '222' },
  };
  const h = harness(async (url, options) => {
    assert.match(url, /\/actions$/);
    assert.equal(options.method, 'POST', 'receipt reads must not use a cacheable GET');
    return response([{ ...pending, state: 'succeeded', outcome: 'log_channel_set' }]);
  });
  h.app.state.config = {
    enrollment_id: 'membership',
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  h.app.state.tab = 'dashboard';
  assert.equal(h.app.applyQueuedLogChannel(pending), true);
  await h.app.reconcileQueuedLogChannel(pending);
  assert.equal(h.app.state.config.log_channels[0].effective, '222');
  assert.equal(h.app.state.config.log_channels[0].pending_action, null);
  assert.equal(h.nodes.get('#toast').textContent, 'Log channel saved');
});

test('targeted log-channel watcher spans the default action lifetime and dispatch timeout', () => {
  const { app } = harness();
  assert.ok(app.actionReconcileDelays.reduce((sum, delay) => sum + delay, 0) >= 420_000);
  assert.ok(app.actionReconcileDelays[0] <= 1_000);
});

test('configuration reload preserves an admitted log-channel intent', async () => {
  const pending = {
    id: 'log-job', guild_id: '987654321098765432', actor_id: '123456789012345678',
    enrollment_id: 'membership', state: 'pending',
    action: { kind: 'set_log_channel', event: 'member_join', channel_id: '222' },
  };
  const h = harness(async (url) => {
    assert.match(url, /\/config$/);
    return response({
      enrollment_id: 'membership', rules: [], commands: [], badwords: [],
      settings: { prefix: '!', bot_name: '' },
      log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
    });
  });
  h.app.state.config = {
    enrollment_id: 'membership',
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  h.app.applyQueuedLogChannel(pending);
  assert.equal(await h.app.loadConfig(), true);
  assert.equal(h.app.state.config.log_channels[0].effective, '222');
  assert.equal(h.app.state.config.log_channels[0].pending_action.id, 'log-job');
});

test('terminal log receipt repaints while retaining an unrelated row draft', async () => {
  const pending = {
    id: 'log-job', guild_id: '987654321098765432', actor_id: '123456789012345678',
    enrollment_id: 'membership', state: 'pending',
    action: { kind: 'set_log_channel', event: 'member_join', channel_id: '222' },
  };
  const h = harness(async () => response([{ ...pending, state: 'succeeded', outcome: 'log_channel_set' }]));
  h.app.state.config = {
    enrollment_id: 'membership', rules: [], commands: [], badwords: [],
    settings: { prefix: '!', bot_name: '' },
    log_channels: [
      { event: 'member_join', effective: '111', guild_override: '111', global: null },
      { event: 'member_leave', effective: '444', guild_override: '444', global: null },
    ],
  };
  h.app.state.tab = 'logchannels';
  h.app.applyQueuedLogChannel(pending);
  h.app.renderTab();
  const inputs = h.nodes.get('#content').querySelectorAll('input, select, textarea')
    .filter((node) => node.attrs.placeholder === 'channel id');
  inputs[1].value = '555';
  h.app.markDirty();

  await h.app.reconcileQueuedLogChannel(pending);

  const renderedInputs = h.nodes.get('#content').querySelectorAll('input, select, textarea')
    .filter((node) => node.attrs.placeholder === 'channel id');
  assert.equal(renderedInputs[1].value, '555');
  assert.equal(h.app.isDirty(), true);
  assert.equal(findNode(h.nodes.get('#content'), (node) => node.tag === 'span' && node.textContent === 'queued'), null);
});

test('failed authoritative log read stays unresolved for retry', async () => {
  const pending = {
    id: 'log-job', guild_id: '987654321098765432', actor_id: '123456789012345678',
    enrollment_id: 'membership', state: 'pending',
    action: { kind: 'set_log_channel', event: 'member_join', channel_id: '222' },
  };
  const h = harness(async (url) => {
    if (url.endsWith('/actions')) return response([{ ...pending, state: 'failed', outcome: 'invalid_channel' }]);
    throw new Error('temporary mapping read failure');
  });
  h.app.state.config = {
    enrollment_id: 'membership',
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  h.app.applyQueuedLogChannel(pending);
  await h.app.reconcileQueuedLogChannel(pending);
  assert.equal(h.app.state.config.log_channels[0].effective, '222');
  assert.equal(h.app.state.config.log_channels[0].pending_action.id, 'log-job');
  assert.equal(h.app.state.config.log_channels[0].pending_action.state, 'failed');
});

test('terminal request-key replay clears dirty state and restores authoritative mapping', async () => {
  let appRef;
  const terminal = {
    id: 'log-job', guild_id: '987654321098765432', actor_id: '123456789012345678',
    enrollment_id: 'membership', state: 'failed', outcome: 'invalid_channel',
    action: { kind: 'set_log_channel', event: 'member_join', channel_id: '222' },
  };
  const h = harness(async (url, options) => {
    if (options.method === 'PUT') return response(terminal);
    if (url.endsWith('/actions')) throw new Error('terminal acknowledgment must not be rediscovered');
    return response({ effective: '111', guild_override: '111', global: null });
  });
  appRef = h.app;
  appRef.state.config = {
    enrollment_id: 'membership', rules: [], commands: [], badwords: [],
    settings: { prefix: '!', bot_name: '' },
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  appRef.state.tab = 'logchannels';
  appRef.renderTab();
  const input = h.nodes.get('#content').querySelectorAll('input, select, textarea')
    .find((node) => node.attrs.placeholder === 'channel id');
  input.value = '222';
  appRef.markDirty();
  const set = findNode(h.nodes.get('#content'), (node) => node.tag === 'button' && node.textContent === 'Set');
  await set.listeners.click({});
  assert.equal(appRef.isDirty(), false);
  assert.equal(appRef.state.config.log_channels[0].effective, '111');
  assert.equal(appRef.state.config.log_channels[0].pending_action, null);
});

test('failed queued clears restore the authoritative mapping', async () => {
  const pending = {
    id: 'clear-job', guild_id: '987654321098765432', actor_id: '123456789012345678',
    enrollment_id: 'membership', state: 'pending',
    action: { kind: 'set_log_channel', event: 'member_join', channel_id: null },
  };
  const h = harness(async (url, options) => {
    assert.equal(options.method, 'POST', 'receipt reads must not use a cacheable GET');
    if (url.endsWith('/actions')) {
      return response([{ ...pending, state: 'failed', outcome: 'invalid_channel' }]);
    }
    assert.match(url, /\/logchannels\/member_join$/);
    return response({ effective: '111', guild_override: '111', global: null });
  });
  h.app.state.config = {
    enrollment_id: 'membership',
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  assert.equal(h.app.applyQueuedLogChannel(pending), true);
  assert.equal(h.app.state.config.log_channels[0].effective, null);
  await h.app.reconcileQueuedLogChannel(pending);
  assert.equal(h.app.state.config.log_channels[0].effective, '111');
  assert.equal(h.app.state.config.log_channels[0].pending_action, null);
  assert.match(h.nodes.get('#toast').textContent, /failed: invalid_channel/);
});

test('failed queued sets preserve authoritative null mappings', async () => {
  const pending = {
    id: 'set-job', guild_id: '987654321098765432', actor_id: '123456789012345678',
    enrollment_id: 'membership', state: 'pending',
    action: { kind: 'set_log_channel', event: 'member_join', channel_id: '222' },
  };
  const h = harness(async (url) => {
    if (url.endsWith('/actions')) {
      return response([{ ...pending, state: 'failed', outcome: 'invalid_channel' }]);
    }
    assert.match(url, /\/logchannels\/member_join$/);
    return response({ effective: null, guild_override: null, global: null });
  });
  h.app.state.config = {
    enrollment_id: 'membership',
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  assert.equal(h.app.applyQueuedLogChannel(pending), true);
  assert.equal(h.app.state.config.log_channels[0].effective, '222');
  await h.app.reconcileQueuedLogChannel(pending);
  assert.equal(h.app.state.config.log_channels[0].effective, null);
  assert.equal(h.app.state.config.log_channels[0].guild_override, null);
  assert.equal(h.app.state.config.log_channels[0].pending_action, null);
});

test('an older terminal log receipt cannot roll back its queued replacement', async () => {
  const first = {
    id: 'first', guild_id: '987654321098765432', actor_id: '123456789012345678',
    enrollment_id: 'membership', state: 'pending',
    action: { kind: 'set_log_channel', event: 'member_join', channel_id: '222' },
  };
  const second = { ...first, id: 'second', action: { ...first.action, channel_id: '333' } };
  const h = harness(async () => response([{ ...first, state: 'cancelled', outcome: 'superseded_by_newer_request' }, second]));
  h.app.state.config = {
    enrollment_id: 'membership',
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  h.app.applyQueuedLogChannel(first);
  h.app.applyQueuedLogChannel(second);
  assert.equal(h.app.applyQueuedLogChannel({ ...first, state: 'cancelled', outcome: 'superseded_by_newer_request' }), true);
  assert.equal(h.app.state.config.log_channels[0].effective, '333');
  assert.equal(h.app.state.config.log_channels[0].pending_action.id, 'second');
  await h.app.reconcileQueuedLogChannel(first);
  assert.equal(h.app.state.config.log_channels[0].effective, '333');
  assert.equal(h.app.state.config.log_channels[0].pending_action.id, 'second');
  assert.equal(h.app.state.config.log_channels[0].pending_action.previous.effective, '111');
});

test('known terminal log results are not labeled queued while mapping reconciliation retries', async () => {
  const terminal = {
    id: 'log-job', guild_id: '987654321098765432', actor_id: '123456789012345678',
    enrollment_id: 'membership', state: 'failed', outcome: 'invalid_channel',
    action: { kind: 'set_log_channel', event: 'member_join', channel_id: '222' },
  };
  const h = harness(async (url) => {
    assert.doesNotMatch(url, /\/actions$/);
    throw new Error('temporary mapping read failure');
  });
  h.app.state.config = {
    enrollment_id: 'membership', rules: [], commands: [], badwords: [],
    settings: { prefix: '!', bot_name: '' },
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  h.app.state.tab = 'logchannels';
  h.app.applyQueuedLogChannel(terminal);
  h.app.renderTab();
  let status = findNode(h.nodes.get('#content'), (node) => node.tag === 'span' && String(node.className).split(/\s+/).includes('badge'));
  assert.equal(status.textContent, 'failed');

  await h.app.reconcileQueuedLogChannel(terminal);
  assert.equal(h.app.state.config.log_channels[0].pending_action.state, 'failed');
  status = findNode(h.nodes.get('#content'), (node) => node.tag === 'span' && String(node.className).split(/\s+/).includes('badge'));
  assert.equal(status.textContent, 'failed');
});

test('case-delete-role save rejects invalid IDs instead of clearing roles', async () => {
  const calls = [];
  const cfg = { settings: { prefix: '!', bot_name: 'Guard' }, settings_override: {}, global_settings: { prefix: '/' } };
  const h = harness(async (url, options) => {
    const body = options.body ? JSON.parse(options.body) : null;
    calls.push({ url, method: options.method, body });
    if (url.endsWith('/case-delete-roles') && options.method === 'POST') {
      return response({ roles: ['1'], revision: 'revision-1' });
    }
    if (url.endsWith('/case-delete-roles') && options.method === 'PUT') {
      return response({ revision: 'revision-2' });
    }
    return response(cfg);
  });
  h.app.state.config = cfg;
  h.app.state.tab = 'settings';
  h.app.tabSettings();
  await new Promise(resolve => setImmediate(resolve));

  const roleInput = h.elements.find(n => n.tag === 'input' && /role id/i.test(n.attrs.placeholder || ''));
  const saveRoles = h.elements.find(n => n.tag === 'button' && n.textContent === 'Save case-removal roles');
  roleInput.value = 'moderators';
  await saveRoles.listeners.click({});
  assert.equal(calls.filter(c => c.method === 'PUT' && c.url.endsWith('/case-delete-roles')).length, 0);
  assert.match(h.nodes.get('#toast').textContent, /digits, mentions, or channel URLs/);
});

test('late case-delete-role load cannot overwrite an acknowledged save', async () => {
  const calls = [];
  let resolveInitialRoles;
  const cfg = { settings: { prefix: '!', bot_name: 'Guard' }, settings_override: {}, global_settings: { prefix: '/' } };
  const h = harness(async (url, options) => {
    const body = options.body ? JSON.parse(options.body) : null;
    calls.push({ url, method: options.method, body });
    if (url.endsWith('/case-delete-roles') && options.method === 'POST') {
      return new Promise(resolve => { resolveInitialRoles = resolve; });
    }
    if (url.endsWith('/case-delete-roles') && options.method === 'PUT') {
      return response({ revision: calls.filter(c => c.method === 'PUT').length === 1 ? 'revision-2' : 'revision-3' });
    }
    return response(cfg);
  });
  h.app.state.config = cfg;
  h.app.state.tab = 'settings';
  h.app.tabSettings();

  const roleInput = h.elements.find(n => n.tag === 'input' && /role id/i.test(n.attrs.placeholder || ''));
  const saveRoles = h.elements.find(n => n.tag === 'button' && n.textContent === 'Save case-removal roles');
  roleInput.value = '2';
  await saveRoles.listeners.click({});

  resolveInitialRoles(response({ roles: ['1'], revision: 'revision-1' }));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(roleInput.value, '2');

  await saveRoles.listeners.click({});
  const puts = calls.filter(c => c.method === 'PUT' && c.url.endsWith('/case-delete-roles'));
  assert.equal(puts.length, 2);
  assert.equal(puts[1].body.revision, 'revision-2');
});

test('settings keep clear-to-inherit and empty nickname distinct', async () => {
  const { calls, inputs, save } = settingsHarness();
  inputs[0].value = '';
  inputs[1].value = '';
  await save.listeners.click({});
  const put = calls.find((c) => c.method === 'PUT' && c.url.endsWith('/settings'));
  assert.deepEqual(put.body, { prefix: null, bot_name: '' });
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
        : elements.find(n => n.tag === 'button' && (n.attrs['aria-label'] === 'Remove action 1' || n.textContent === '✕' || n.children.some(c => c.textContent === '✕')));
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
      return response({ ok: true, config_patch: { kind: 'rule', id: 'mention_spam', scope: body.scope, value: body.data } });
    } else if (options.method === 'DELETE') {
      if (failures.delete) throw new Error('cleanup unavailable');
      guildRule = null;
      return response({ ok: true, config_patch: { kind: 'rule', id: 'mention_spam', scope: 'guild', value: null } });
    } else {
      if (failures.refresh) throw new Error('refresh unavailable');
      return response(snapshot());
    }
  });
  h.app.state.me.is_bot_owner = true;
  h.app.state.config = structuredClone(snapshot());
  h.app.navigate('rules');
  h.app.editRule(h.app.state.config.rules[0]);
  const content = h.nodes.get('#content');
  const modal = findNode(content, (n) => String(n.className).split(/\s+/).includes('modal-overlay'));
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
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'POST']);
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
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'POST']);
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
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'POST', 'POST']);
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

test('a successful global rule save applies both acknowledgments without a config reload', async () => {
  const h = ruleSaveHarness({ override: true });
  await h.enabled.listeners.change({ target: { checked: false } });
  await h.save.listeners.click({});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE']);
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
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT']);
  assert.equal(h.calls[0].body.scope, 'guild');
  assert.equal(h.app.state.config.rules[0].global.enabled, true);
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
});

test('a refused command save retains its checkbox value for the next save', async () => {
  const calls = [];
  let refuse = true;
  const h = harness(async (url, options) => {
    if (String(url).includes('/config') || options.method === 'GET') return response({ commands: [] });
    calls.push(JSON.parse(options.body));
    if (refuse) throw new Error('write refused');
    return response({ ok: true });
  });
  h.app.state.config = { commands: [] };
  h.app.navigate('commands');
  h.app.editCommand(null);
  const content = h.nodes.get('#content');
  const modal = findNode(content, (n) => String(n.className).split(/\s+/).includes('modal-overlay'));
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

test('configuration mutations retain the loaded membership without refreshing or retrying', async () => {
  const calls = [];
  const { app } = harness(async (url, opts) => {
    calls.push({ url, opts });
    return { ok: false, status: 409, headers: { get: () => null }, text: async () => JSON.stringify({ error: 'Membership changed' }) };
  });
  app.state.config = { enrollment_id: 'old-membership' };
  for (const [method, suffix] of [['PUT', 'settings'], ['PUT', 'rules/flood'], ['DELETE', 'rules/flood'], ['POST', 'badwords'], ['DELETE', 'badwords/1'], ['POST', 'commands'], ['PUT', 'commands/test'], ['DELETE', 'commands/test'], ['PUT', 'templates/edit'], ['DELETE', 'templates/edit'], ['POST', 'copy'], ['PUT', 'case-delete-roles'], ['DELETE', 'cases/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa']]) {
    const count = calls.length;
    await assert.rejects(app.api(`/api/guilds/${app.state.guildId}/${suffix}`, method, {}), /Membership changed/);
    assert.equal(calls.length, count + 1);
    assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], 'old-membership');
  }
  for (const path of ['/api/guilds/0/settings', `/api/guilds/${app.state.guilds[1].id}/settings`]) {
    await assert.rejects(app.api(path, 'PUT', {}));
    assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], undefined);
  }
});

test('drafts survive retries only within the captured bot membership', () => {
  for (const next of ['original', 'replacement']) {
    const { app, nodes } = harness();
    const input = { value: 'Saved value' };
    app.state.config = null;
    app.retainDrafts(app.state.guildId, app.state.tab, [{ key: '0', value: 'Old draft' }], 'original');
    app.state.config = { enrollment_id: next };
    nodes.get('#content').querySelector = () => input;
    app.restoreDrafts();
    assert.equal(input.value, next === 'original' ? 'Old draft' : 'Saved value');
    assert.equal(app.isDirty(), next === 'original');
  }
});

test('an async action cannot restore drafts into a replacement membership', async () => {
  const { app, nodes } = harness();
  app.state.config = { enrollment_id: 'original' };
  const input = { value: 'Old draft', _initialValue: 'Saved', dataset: { inputKey: '0' } };
  nodes.get('#content').querySelectorAll = () => [input];
  nodes.get('#content').querySelector = () => input;
  await app.handleEvent({ closest: () => ({ contains: () => false }), isConnected: false }, async () => {
    app.state.config = { enrollment_id: 'replacement' };
    input.value = 'New membership value';
  }, {});
  assert.equal(input.value, 'New membership value');
  assert.equal(app.isDirty(), false);
});

test('copy captures source membership on selection and sends both page scopes', async () => {
  const calls = [];
  const cfg = { enrollment_id: 'destination', settings: { prefix: '!', bot_name: '' }, settings_override: {}, global_settings: { prefix: '!' } };
  const { app, elements } = harness(async (url, opts) => {
    calls.push({ url, opts });
    if (url.endsWith('/copy') && opts.method === 'POST') return response({ copied: ['settings'] });
    return response(url.includes('987654321098765433') ? { enrollment_id: 'selected-source' } : cfg);
  });
  app.state.config = cfg;
  app.state.tab = 'settings';
  app.tabSettings();
  const source = elements.find(n => n.tag === 'select' && n.listeners.change);
  const copy = elements.find(n => n.tag === 'button' && n.textContent === 'Copy');
  source.value = app.state.guilds[1].id;
  source.selectedOptions = [{ textContent: 'Second' }];
  await copy.listeners.click({});
  assert.equal(calls.filter((c) => c.url.endsWith('/copy')).length, 0, 'copy waits for an explicitly loaded source');
  await source.listeners.change({});
  assert.equal(calls.filter((c) => c.url.includes(app.state.guilds[1].id)).length, 1);
  await copy.listeners.click({});
  const sent = calls.find(c => c.opts.method === 'POST' && c.url.endsWith('/copy'));
  assert.equal(JSON.parse(sent.opts.body).from_enrollment_id, 'selected-source');
  assert.equal(sent.opts.headers['x-jmodguard-enrollment'], 'destination');
  assert.equal(calls.filter(c => c.url.includes('987654321098765433')).length, 1, 'copy must not refresh the selected source token');
});

function queuedAction(state = 'pending') {
  return { id: 'job', guild_id: '987654321098765432', actor_id: '123456789012345678', enrollment_id: 'membership', state, action: {kind:'unban',user_id:'99'} };
}
const actionPath = '/api/guilds/987654321098765432/unban';

test('lost action acknowledgment reuses request and membership across reload', async () => {
  const calls = [], session = new Map();
  const first = harness(async (_,opts) => { calls.push(opts); throw new Error('lost response'); }, new Map(), session);
  first.app.state.config = { enrollment_id:'membership' };
  await assert.rejects(first.app.submitBotAction(actionPath, 'POST', {user_id:'99',reason:'test'}), /lost response/);
  assert.equal(session.size,1);
  const next = harness(async (_,opts) => { calls.push(opts); return response(queuedAction()); },new Map(),session);
  next.app.state.config = { enrollment_id:'membership' };
  await next.app.submitBotAction(actionPath,'POST',{reason:'test',user_id:'99'});
  assert.equal(JSON.parse(calls[0].body).request_id, JSON.parse(calls[1].body).request_id);
  assert.equal(calls[1].headers['x-jmodguard-enrollment'],'membership');
  assert.equal(session.size,0);
  assert.match(next.nodes.get('#toast').textContent,/queued/);
  assert.doesNotMatch(next.nodes.get('#toast').textContent,/completed|Unbanned/);
});

test('saved action cannot move to replacement membership; clearing never submits', async () => {
  const session = new Map();
  const first = harness(async () => { throw new Error('lost'); },new Map(),session);
  first.app.state.config = { enrollment_id:'old' };
  await assert.rejects(first.app.submitBotAction(actionPath,'POST',{user_id:'99'}));
  for (const accept of [false,true]) {
    let calls=0;
    const next=harness(async()=>{calls++; return response(queuedAction());},new Map(),session,()=>accept);
    next.app.state.config={enrollment_id:'replacement'};
    await assert.rejects(next.app.submitBotAction(actionPath,'POST',{user_id:'99'}),/not submitted/);
    assert.equal(calls,0);
    assert.equal(session.size,accept?0:1);
  }
});

test('expired or invalid action recovery clears only after explicit acknowledgment', async () => {
  for (const [status,code] of [[410,'action_request_expired'],[400,'action_request_invalid'],[409,'action_enrollment_changed']]) {
    let calls=0;
    const h=harness(async()=>{calls++;return {ok:false,status,headers:{get:()=>null},text:async()=>JSON.stringify({error:'rejected',code})};});
    h.app.state.config={enrollment_id:'membership'};
    await assert.rejects(h.app.submitBotAction(actionPath,'POST',{user_id:'99'}),/Saved request cleared/);
    assert.equal(calls,1);
    assert.equal(h.session.size,0);
  }
});

test('invalid action acknowledgments retain request identity; unavailable storage prevents dispatch', async () => {
  for (const job of [{}, {...queuedAction(),enrollment_id:'other'}, {...queuedAction(),guild_id:'other'}, {...queuedAction(),actor_id:'other'}]) {
    const h=harness(async()=>response(job));
    h.app.state.config={enrollment_id:'membership'};
    await assert.rejects(h.app.submitBotAction(actionPath,'POST',{user_id:'99'}),/acknowledgment was invalid/);
    assert.equal(h.session.size,1);
  }
  let calls=0;
  const h=harness(async()=>{calls++;},new Map(),{get(){throw new Error('disabled');}});
  h.app.state.config={enrollment_id:'membership'};
  await assert.rejects(h.app.submitBotAction(actionPath,'POST',{user_id:'99'}),/session storage/);
  assert.equal(calls,0);
});

test('actions refresh only on demand and cancellation includes membership', async () => {
  const calls=[];
  const h=harness(async(url,opts)=>{calls.push({url,opts});return response(opts.method==='DELETE'?{ok:true}:[queuedAction()]);});
  h.app.state.config={enrollment_id:'membership'};
  h.app.tabActions();
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(calls.length,1);
  assert.equal(calls[0].opts.method,'POST');
  assert.equal(calls[0].opts.headers['x-jmodguard-enrollment'],'membership');
  assert.match(textOf(h.elements.find(n=>n.tag==='p')),/Force-cancel a stuck log-channel request/);
  const cancel=h.elements.find(n=>n.tag==='button'&&n.textContent==='Cancel');
  await cancel.listeners.click({});
  assert.equal(calls[1].opts.method,'DELETE');
  assert.equal(calls[1].opts.headers['x-jmodguard-enrollment'],'membership');
  assert.equal(calls[2].opts.method,'POST');
  const refresh=h.elements.find(n=>n.tag==='button'&&n.textContent==='Refresh');
  await refresh.listeners.click({});
  assert.equal(calls.length,4);
});

test('claimed and stuck log-channel actions expose recovery cancel', async () => {
  const jobs = [
    queuedAction('claimed'),
    { ...queuedAction('running'), id: 'unban-run', action: { kind: 'unban', user_id: '99' } },
    { ...queuedAction('running'), id: 'log-run', action: { kind: 'set_log_channel', event: 'member_join', channel_id: '1' } },
    { ...queuedAction('uncertain'), id: 'log-unk', action: { kind: 'set_log_channel', event: 'member_ban', channel_id: null } },
    { ...queuedAction('failed'), id: 'failed', action: { kind: 'set_log_channel', event: 'member_leave', channel_id: '1' } },
  ];
  let confirms = 0;
  const calls = [];
  const h = harness(
    async (url, opts) => { calls.push({ url, opts }); return response(opts.method === 'DELETE' ? { ok: true } : jobs); },
    new Map(),
    new Map(),
    () => { confirms += 1; return true; },
  );
  h.app.state.config = { enrollment_id: 'membership' };
  h.app.tabActions();
  await new Promise((resolve) => setImmediate(resolve));
  const controls = h.elements.filter((n) => n.tag === 'button' && (n.textContent === 'Cancel' || n.textContent === 'Force cancel'));
  assert.deepEqual(controls.map((n) => n.textContent), ['Cancel', 'Force cancel', 'Force cancel']);
  await controls[0].listeners.click({});
  assert.equal(confirms, 0);
  assert.equal(calls[1].opts.method, 'DELETE');
  assert.match(calls[1].url, /\/actions\/job$/);
  await controls[1].listeners.click({});
  assert.equal(confirms, 1);
  assert.equal(calls[3].opts.method, 'DELETE');
  assert.match(calls[3].url, /\/actions\/log-run$/);
});

test('moderation reads send the loaded page membership including query-string routes', async () => {
  const calls = [];
  const h = harness(async (url, opts) => { calls.push({url,opts}); return response({}); });
  h.app.state.config = { enrollment_id: 'page-membership' };
  for (const suffix of ['events?event=member_ban&user=123', 'bans?limit=100', 'stats', 'cases', 'cases/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'case-delete-roles']) {
    await h.app.api(`/api/guilds/${h.app.state.guildId}/${suffix}`);
    assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], 'page-membership');
  }
  await h.app.api('/api/guilds/987654321098765433/stats');
  assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], undefined);
  h.app.state.config = null;
  await h.app.api(`/api/guilds/${h.app.state.guildId}/stats`);
  assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], undefined);
  assert.equal(calls.length, 8, 'reads must not silently load a new membership');
});

test('individual configuration GETs retain loaded membership without rebinding global or foreign scopes', async () => {
  const calls = [];
  const h = harness(async (url, opts) => { calls.push({url, opts}); return response({}); });
  h.app.state.config = { enrollment_id: 'loaded-config-membership' };
  const routes = ['rules/spam?probe=1', 'badwords', 'commands', 'commands/test', 'templates', 'templates/message_edit', 'logchannels', 'logchannels/message_edit'];
  for (const route of routes) {
    await h.app.api(`/api/guilds/${h.app.state.guildId}/${route}`);
    assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], 'loaded-config-membership');
  }
  for (const guild of ['0', '987654321098765433']) {
    await h.app.api(`/api/guilds/${guild}/commands`);
    assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], undefined);
  }
  h.app.state.config = null;
  await h.app.api(`/api/guilds/${h.app.state.guildId}/commands`);
  assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], undefined);
  assert.equal(calls.length, routes.length + 3);
});

test('log filter saves use the loaded guild enrollment', async () => {
  const calls = [];
  const h = harness(async (url, opts) => { calls.push({ url, opts }); return response({ ignore: { users: ['123'], channels: ['456'] } }); });
  h.app.state.config = { enrollment_id: 'loaded-config-membership' };
  await h.app.api(`/api/guilds/${h.app.state.guildId}/logfilters/message_edit`, 'PUT', { users: ['123'], channels: ['456'] });
  assert.equal(calls[0].opts.headers['x-jmodguard-enrollment'], 'loaded-config-membership');
  assert.deepEqual(JSON.parse(calls[0].opts.body), { users: ['123'], channels: ['456'] });
});


test('image rule UI can save test mode or delete-and-ban', async () => {
  const calls=[];
  const h=harness(async (url, options) => {
    if (options.method === 'PUT') calls.push(JSON.parse(options.body));
    return response({ settings:{}, rules:[], commands:[], log_channels:[], badwords:[] });
  });
  const legacy={ name:'Images', rule_id:'attachment_flood', effective:{ enabled:true, dry_run:false, params:{min_channels:2,min_attachments:2,window_secs:60,min_messages:2}, actions:[{at:1,action:'ban'}] } };
  assert.match(textOf(h.app.ruleCard(legacy)), /inactive/);
  const modal=h.app.editRule(legacy);
  assert.doesNotMatch(textOf(modal), /Action escalation/);
  const dryRun = findNode(modal,n=>n.tag==='label' && textOf(n).includes('Dry-run (log only)'));
  assert.equal(findNode(dryRun,n=>n.tag==='input').checked,true);
  const save=findNode(modal,n=>n.textContent === 'Save');
  await save.listeners.click({});
  assert.equal(calls.length,1);
  assert.equal(calls[0].data.dry_run,true);
  assert.deepEqual(calls[0].data.params,{mode:'exact_image_test',min_channels:2,min_attachments:2,window_secs:60});
  assert.deepEqual(calls[0].data.actions.map(a=>a.action),['log']);
  assert.equal(h.app.spamConfigured('attachment_flood', calls[0].data),true);
  assert.match(textOf(h.app.ruleCard({name:'Images',rule_id:'attachment_flood',effective:calls[0].data})), /dry-run/);
  assert.doesNotMatch(textOf(h.app.ruleCard({name:'Images',rule_id:'attachment_flood',effective:calls[0].data})), /log-only test/);
  calls[0].data.params.min_channels=1;
  assert.equal(h.app.spamConfigured('attachment_flood', calls[0].data),false);
  const punish=h.app.editRule({name:'Images',rule_id:'attachment_flood',effective:{enabled:true,dry_run:true,params:{mode:'exact_image_test',min_channels:3,min_attachments:2,window_secs:60},actions:[{action:'log'}]}});
  const dryRunToggle = findNode(punish,n=>n.tag==='label' && textOf(n).includes('Dry-run (log only)'));
  findNode(dryRunToggle,n=>n.tag==='input' && n.attrs.type==='checkbox').checked = false;
  await findNode(punish,n=>n.textContent === 'Save').listeners.click({});
  assert.equal(calls.at(-1).data.dry_run,false);
  assert.equal(calls.at(-1).data.params.mode,'exact_image');
  assert.deepEqual(calls.at(-1).data.actions.map(a=>a.action),['delete','ban']);
  const back=h.app.editRule({name:'Images',rule_id:'attachment_flood',effective:calls.at(-1).data});
  const backToggle = findNode(back,n=>n.tag==='label' && textOf(n).includes('Dry-run (log only)'));
  assert.equal(findNode(backToggle,n=>n.tag==='input').checked,false);
  findNode(backToggle,n=>n.tag==='input').checked=true;
  await findNode(back,n=>n.textContent==='Save').listeners.click({});
  assert.equal(calls.at(-1).data.dry_run,true);
  assert.equal(calls.at(-1).data.params.mode,'exact_image_test');
  assert.deepEqual(calls.at(-1).data.actions.map(a=>a.action),['log']);
});

test('checkbox labels inside rows keep primary text color', () => {
  const css = fs.readFileSync(new URL('../frontend/style.css', import.meta.url), 'utf8')
    .replace(/\/\*[\s\S]*?\*\//g, '');
  const rules = [...css.matchAll(/([^{}]+)\{([^{}]*)\}/g)];
  const specificity = (selector) => {
    const ids = (selector.match(/#[\w-]+/g) || []).length;
    const classes = (selector.match(/\.[\w-]+/g) || []).length
      + (selector.match(/\[[^\]]+\]/g) || []).length
      + (selector.match(/:(?:not|is|where)\([^)]*\)/g) || []).length;
    const withoutPseudos = selector.replace(/:(?:not|is|where)\([^)]*\)/g, '');
    const elements = (withoutPseudos.match(/(^|[\s>+~])[a-zA-Z][\w-]*/g) || []).length;
    return ids * 100 + classes * 10 + elements;
  };
  const matches = (selector, checkbox) => {
    if (selector.includes(':not(.checkbox-label)') && checkbox) return false;
    const normalized = selector.replace(/:not\([^)]*\)/g, '').replace(/\s+/g, ' ').trim();
    if (checkbox) {
      return normalized === '.checkbox-label'
        || normalized === '.row > label'
        || normalized === '.row > label.checkbox-label'
        || normalized === '.row > .checkbox-label';
    }
    return normalized === '.row > label' || normalized === '.row label';
  };
  const winningColor = (checkbox) => {
    let winner = null;
    rules.forEach((match, index) => {
      const declared = match[2].match(/(?:^|;)\s*color\s*:\s*([^;]+)/);
      if (!declared) return;
      for (const raw of match[1].split(',')) {
        const selector = raw.trim();
        if (!matches(selector, checkbox)) continue;
        const rank = specificity(selector);
        if (!winner || rank > winner.rank || (rank === winner.rank && index >= winner.index)) {
          winner = { rank, index, color: declared[1].trim() };
        }
      }
    });
    return winner && winner.color;
  };
  assert.equal(winningColor(true), 'var(--text)');
  assert.equal(winningColor(false), 'var(--muted)');
});

test('text rule UI can save independent thresholds without generic escalation', async () => {
  const calls=[];
  const h=harness(async (url, options) => {
    if (options.method === 'PUT') calls.push(JSON.parse(options.body));
    return response({ settings:{}, rules:[], commands:[], log_channels:[], badwords:[] });
  });
  const modal=h.app.editRule({name:'Text',rule_id:'repeat_text',effective:{enabled:true,dry_run:true,params:{mode:'exact_text',min_channels:3,window_secs:60,min_length:20},actions:[{action:'log'}]}});
  assert.doesNotMatch(textOf(modal), /Action escalation/);
  assert.match(textOf(modal), /Minimum length/);
  await findNode(modal,n=>n.textContent === 'Save').listeners.click({});
  assert.equal(calls[0].data.params.mode,'exact_text');
  assert.equal(calls[0].data.params.min_length,20);
  assert.equal(h.app.spamConfigured('repeat_text', calls[0].data),true);
  assert.match(textOf(h.app.ruleCard({name:'Text',rule_id:'repeat_text',effective:calls[0].data})), /dry-run/);
  const toggle=findNode(modal,n=>n.tag==='label' && textOf(n).includes('Dry-run (log only)'));
  findNode(toggle,n=>n.tag==='input').checked=false;
  await findNode(modal,n=>n.textContent==='Save').listeners.click({});
  assert.equal(calls.at(-1).data.dry_run,false);
  assert.deepEqual(calls.at(-1).data.actions.map(a=>a.action),['delete','ban']);
  const back=h.app.editRule({name:'Text',rule_id:'repeat_text',effective:calls.at(-1).data});
  const backToggle=findNode(back,n=>n.tag==='label' && textOf(n).includes('Dry-run (log only)'));
  assert.equal(findNode(backToggle,n=>n.tag==='input').checked,false);
  findNode(backToggle,n=>n.tag==='input').checked=true;
  await findNode(back,n=>n.textContent==='Save').listeners.click({});
  assert.equal(calls.at(-1).data.dry_run,true);
  assert.deepEqual(calls.at(-1).data.actions.map(a=>a.action),['log']);
});

test('image test UI rejects invalid thresholds before the API request', async () => {
  let sent=0;
  const h=harness(async()=>{sent++;return response({});});
  const modal=h.app.editRule({name:'Images',rule_id:'attachment_flood',effective:{enabled:true,params:{min_channels:1},actions:[]}});
  await findNode(modal,n=>n.textContent === 'Save').listeners.click({});
  assert.equal(sent,0);
  assert.match(h.nodes.get('#toast').textContent,/Channels must be/);
});


test('case administration reaches later pages, all admitted history and inline evidence', async () => {
  const ids = Array.from({length: 51}, (_, i) => (i + 1).toString(16).padStart(32, '0'));
  const calls = [];
  const h = harness(async (url) => {
    calls.push(url);
    const u = new URL(url, 'https://fixture.test');
    if (u.pathname.endsWith('/history')) {
      const start = Number(u.searchParams.get('after'));
      return response({history: Array.from({length: 32}, (_, i) => ({sequence: String(start + i + 1), entry: {body: {text: `note ${start + i + 1}`}}}))});
    }
    if (u.pathname.endsWith('/evidence')) return response({evidence: []});
    if (u.pathname.endsWith('/cases')) return response({cases: u.searchParams.has('after') ? ids.slice(50) : ids.slice(0, 50), can_delete: true});
    return response({can_delete: true, case: {action: 'warning', evidence: {text: [{text: '<script>literal excerpt</script>', message_id: '9', channel_id: '8', author_id: '7'}]}}});
  });
  const root = h.app.tabCases();
  await new Promise(resolve => setImmediate(resolve));
  const next = findNode(root, n => n.tag === 'button' && n.textContent === 'Next page');
  assert.equal(next.disabled, false);
  next.listeners.click();
  await new Promise(resolve => setImmediate(resolve));
  assert.ok(calls.some(url => url.includes(`after=${ids[49]}`)));
  const caseButton = findNode(root, n => n.tag === 'button' && n.textContent === ids[50].slice(0, 8));
  caseButton.listeners.click();
  await new Promise(resolve => setImmediate(resolve));
  assert.ok(textOf(root).includes('note 33'));
  assert.ok(textOf(root).includes('note 64'));
  const excerpt = findNode(root, n => n.tag === 'pre');
  assert.equal(excerpt.textContent, '<script>literal excerpt</script>');
  assert.equal(next.disabled, true);
});

test('removed case recovery survives opening a new dashboard view and retries the exact identity', async () => {
  const id = 'a'.repeat(32);
  const calls = [];
  const h = harness(async (url, options) => {
    calls.push({url, method: options.method});
    if (options.method === 'DELETE') return response({removed: false, cleanup_pending: false});
    const u = new URL(url, 'https://fixture.test');
    return response({cases: u.searchParams.get('removed') === 'true' ? [id] : [], can_delete: true});
  });
  const root = h.app.tabCases();
  await new Promise(resolve => setImmediate(resolve));
  findNode(root, n => n.textContent === 'Removed records / retry cleanup').listeners.click();
  await new Promise(resolve => setImmediate(resolve));
  assert.ok(textOf(root).includes(id));
  await findNode(root, n => n.textContent === 'Retry cleanup').listeners.click();
  assert.ok(calls.some(c => c.method === 'DELETE' && c.url.endsWith('/cases/' + id)));
});

test('note retry retains intent after a lost response and blocks overlapping submits', async () => {
  const id = 'a'.repeat(32), posts = [];
  let resolveFirst;
  const h = harness(async (url, options) => {
    if (options?.method === 'POST' && options.body) {
      posts.push(JSON.parse(options.body));
      if (posts.length === 1) return await new Promise((resolve, reject) => { resolveFirst = () => reject(new Error('response lost')); });
      throw new Error('response lost');
    }
    if (url.includes('/history')) return response({history: []});
    if (url.includes('/evidence')) return response({evidence: []});
    if (url.endsWith('/cases?limit=50')) return response({cases: [id]});
    if (url.includes('/cases?')) return response({cases: [id]});
    return response({case: {action: 'warning', evidence: {}}, can_delete: true});
  });
  const root = h.app.tabCases();
  await new Promise(resolve => setImmediate(resolve));
  findNode(root, n => n.tag === 'button' && n.textContent === id.slice(0,8)).listeners.click();
  await new Promise(resolve => setImmediate(resolve));
  const note = findNode(root, n => n.tag === 'textarea');
  const submit = findNode(root, n => n.tag === 'button' && n.textContent === 'Add note');
  assert.ok(note, textOf(root));
  note.value = 'same note';
  const first = submit.listeners.click();
  await submit.listeners.click();
  assert.equal(posts.length,1);
  resolveFirst(); await first;
  await submit.listeners.click();
  assert.deepEqual(posts[1],posts[0]);
  note.value = 'different note';
  await submit.listeners.click();
  assert.notEqual(posts[2].entry_id,posts[0].entry_id);
});

test('acknowledged case note no longer leaves a false unsaved warning', async () => {
  const id = 'b'.repeat(32);
  const h = harness(async (url, options) => {
    if (options?.method === 'POST' && options.body) return response({ ok: true });
    if (url.includes('/history')) return response({ history: [] });
    if (url.includes('/evidence')) return response({ evidence: [] });
    if (url.includes('/cases?')) return response({ cases: [id] });
    return response({ case: { action: 'warning', evidence: {} }, can_delete: true });
  });
  h.app.state.tab = 'cases';
  const root = h.app.tabCases();
  h.nodes.get('#content').append(root);
  await new Promise(resolve => setImmediate(resolve));
  findNode(root, n => n.tag === 'button' && n.textContent === id.slice(0, 8)).listeners.click();
  await new Promise(resolve => setImmediate(resolve));
  const note = findNode(root, n => n.tag === 'textarea');
  note.value = 'saved note';
  h.app.markDirty();
  const submit = findNode(root, n => n.tag === 'button' && n.textContent === 'Add note');
  await submit.listeners.click({});
  assert.equal(h.app.isDirty(), false);
});

test('preview token is sent on API requests from this tab', async () => {
  let headers;
  const { app, session } = harness(async (_url, options) => {
    headers = options.headers;
    return { ok: true, headers: { get: () => null }, text: async () => '{}' };
  });
  session.set('jmodguard_preview', 'preview-token');
  await app.api('/api/me');
  assert.equal(headers['x-jmodguard-preview'], 'preview-token');
});

test('preview fragment remains fail-closed when session storage is blocked', async () => {
  let headers;
  const blocked = {
    get() { throw new Error('blocked'); },
    set() { throw new Error('blocked'); },
    delete() { throw new Error('blocked'); },
  };
  const h = harness(async (_url, options) => {
    headers = options.headers;
    return { ok: true, headers: { get: () => null }, text: async () => '{}' };
  }, new Map(), blocked);
  h.location.hash = '#preview=fragment-token';
  h.app.capturePreviewToken();
  assert.equal(h.app.currentPreviewToken(), 'fragment-token');
  await h.app.api('/api/me');
  assert.equal(headers['x-jmodguard-preview'], 'fragment-token');
  assert.equal(h.historyUrls.length, 0);

  let reloadHeaders;
  const reloaded = harness(async (_url, options) => {
    reloadHeaders = options.headers;
    return { ok: true, headers: { get: () => null }, text: async () => '{}' };
  }, new Map(), blocked);
  reloaded.location.hash = h.location.hash;
  reloaded.app.capturePreviewToken();
  await reloaded.app.api('/api/me');
  assert.equal(reloadHeaders['x-jmodguard-preview'], 'fragment-token');
});

test('preview fragment survives later storage loss and is removed on explicit return', async () => {
  const session = new Map();
  const first = harness(async () => ({
    ok: true,
    headers: { get: () => null },
    text: async () => '{}',
  }), new Map(), session);
  first.location.hash = '#preview=durable-token&tab=cases';
  first.app.capturePreviewToken();
  assert.equal(session.get('jmodguard_preview'), 'durable-token');
  assert.equal(first.historyUrls.length, 0);

  session.clear();
  let headers;
  const reloaded = harness(async (_url, options) => {
    headers = options.headers;
    return { ok: true, headers: { get: () => null }, text: async () => '{}' };
  }, new Map(), session);
  reloaded.location.hash = first.location.hash;
  reloaded.app.capturePreviewToken();
  await reloaded.app.api('/api/me');
  assert.equal(headers['x-jmodguard-preview'], 'durable-token');

  reloaded.app.returnFromPreview();
  assert.equal(reloaded.app.currentPreviewToken(), null);
  assert.deepEqual(reloaded.historyUrls, ['/#tab=cases']);
});

test('preview bearer is not accepted from a query string', async () => {
  let headers;
  const h = harness(async (_url, options) => {
    headers = options.headers;
    return { ok: true, headers: { get: () => null }, text: async () => '{}' };
  });
  h.location.search = '?preview=query-token';
  h.app.capturePreviewToken();
  assert.equal(h.app.currentPreviewToken(), null);
  await h.app.api('/api/me');
  assert.equal(headers['x-jmodguard-preview'], undefined);
});

test('expired preview clears the bearer and leaves the preview tab', async () => {
  const h = harness(async () => ({
    ok: false,
    status: 410,
    headers: { get: () => null },
    text: async () => JSON.stringify({ error: 'preview expired', code: 'preview_expired' }),
  }));
  h.session.set('jmodguard_preview', 'expired-token');
  await assert.rejects(h.app.api('/api/me'), /preview expired/);
  assert.equal(h.session.has('jmodguard_preview'), false);
  assert.equal(h.location.replaced, '/');
});

test('View as hands the preview bearer through a URL fragment', async () => {
  const h = harness(async () => ({
    ok: true,
    headers: { get: () => null },
    text: async () => JSON.stringify({ token: 'new-preview-token' }),
  }));
  h.app.state.me.is_bot_owner = true;
  h.app.bindOwnerTools();
  const button = findNode(h.nodes.get('#owner-tools'), n => n.textContent === 'View as…');
  await button.listeners.click();
  assert.equal(h.opened[0][0], '/#preview=new-preview-token');
  assert.equal(h.opened[0][1], '_blank');
  assert.equal(h.opened[0][2], 'noopener');
});

test('unauthenticated blocked-storage preview never exposes owner login', async () => {
  const blocked = {
    get() { throw new Error('blocked'); },
    set() { throw new Error('blocked'); },
    delete() { throw new Error('blocked'); },
  };
  const h = harness(async () => ({
    ok: false,
    status: 401,
    headers: { get: () => null },
    text: async () => JSON.stringify({ error: 'not authenticated' }),
  }), new Map(), blocked);
  h.location.hash = '#preview=still-read-only';
  h.app.capturePreviewToken();

  await h.app.init();

  assert.equal(h.nodes.get('#startup-view').classList.contains('hidden'), false);
  assert.equal(h.nodes.get('#login-view').classList.contains('hidden'), true);
  assert.equal(h.nodes.get('#preview-return-link').classList.contains('hidden'), false);
  assert.match(h.nodes.get('#startup-status').textContent, /preview is no longer signed in/i);
  assert.equal(h.app.currentPreviewToken(), 'still-read-only');

  h.app.returnFromPreview();
  assert.equal(h.app.currentPreviewToken(), null);
});

test('post-boot preview authentication loss enters the unavailable state', async () => {
  const session = new Map([['jmodguard_preview', 'preview-token']]);
  const h = harness(async () => ({
    ok: false,
    status: 401,
    headers: { get: () => null },
    text: async () => JSON.stringify({ error: 'not authenticated' }),
  }), new Map(), session);

  await assert.rejects(h.app.api('/api/guilds/10/cases'), /not authenticated/);

  assert.equal(h.nodes.get('#startup-view').classList.contains('hidden'), false);
  assert.equal(h.nodes.get('#login-view').classList.contains('hidden'), true);
  assert.equal(h.nodes.get('#preview-return-link').classList.contains('hidden'), false);
});

test('production session-expired response enters preview unavailable state', async () => {
  const session = new Map([['jmodguard_preview', 'preview-token']]);
  const h = harness(async () => ({
    ok: false,
    status: 400,
    headers: { get: () => null },
    text: async () => JSON.stringify({ error: 'session expired' }),
  }), new Map(), session);

  await h.app.init();

  assert.equal(h.nodes.get('#startup-view').classList.contains('hidden'), false);
  assert.equal(h.nodes.get('#login-view').classList.contains('hidden'), true);
  assert.equal(h.nodes.get('#preview-return-link').classList.contains('hidden'), false);
});

test('private downloads share preview authentication-loss handling', async () => {
  for (const [status, message] of [[401, 'not authenticated'], [400, 'session expired']]) {
    const session = new Map([['jmodguard_preview', 'preview-token']]);
    const h = harness(async () => ({
      ok: false,
      status,
      text: async () => JSON.stringify({ error: message }),
    }), new Map(), session);

    await assert.rejects(h.app.downloadPrivate('/api/guilds/10/cases/1/evidence/2'), new RegExp(message));

    assert.equal(h.nodes.get('#startup-view').classList.contains('hidden'), false);
    assert.equal(h.nodes.get('#login-view').classList.contains('hidden'), true);
    assert.equal(h.nodes.get('#preview-return-link').classList.contains('hidden'), false);
  }
});

test('parseSnowflake keeps IDs as digit strings', () => {
  const h = harness();
  assert.equal(h.app.parseSnowflake('1329331947876479006'), '1329331947876479006');
  assert.equal(h.app.parseSnowflake('<#1329331947876479006>'), '1329331947876479006');
  assert.equal(h.app.parseSnowflake('https://discord.com/channels/10/1329331947876479006'), '1329331947876479006');
  assert.equal(h.app.parseSnowflake('general'), '');
  assert.equal(h.app.parseSnowflake('<@&1329331947876479006>'), '1329331947876479006');
  assert.equal(h.app.parseSnowflake('<@!1329331947876479006>'), '1329331947876479006');
  const roles = h.app.splitIds('<@&1329331947876479006>, 11');
  assert.equal(roles.length, 2);
  assert.equal(roles[0], '1329331947876479006');
  assert.equal(roles[1], '11');
  const empty = h.app.splitIds('');
  assert.equal(empty.length, 0);
  assert.throws(
    () => h.app.splitIds('not-an-id, 22'),
    /digits, mentions, or channel URLs/,
  );
});

test('loadConfig reads guild config with POST', async () => {
  const calls = [];
  const { app } = harness(async (url, opts) => {
    calls.push({ url, method: opts.method });
    return response({ settings: { prefix: '!' } });
  });
  await app.loadConfig();
  assert.deepEqual(calls[0], { url: `/api/guilds/${app.state.guildId}/config`, method: 'POST' });
});

test('unauthenticated settings save returns to login', async () => {
  const h = harness(async () => ({
    ok: false,
    status: 400,
    headers: { get: () => null },
    text: async () => JSON.stringify({ error: 'not authenticated' }),
  }));
  h.app.state.config = { settings: { prefix: '!', bot_name: 'Guard' }, settings_override: {}, global_settings: { prefix: '/' } };
  h.app.state.tab = 'settings';
  h.nodes.get('#login-view').classList.add('hidden');
  h.nodes.get('#main-view').classList.remove('hidden');
  h.app.tabSettings();
  const inputs = h.elements.filter(n => n.tag === 'input');
  const save = h.elements.find(n => n.tag === 'button' && n.textContent === 'Save');
  inputs[0].value = '?';
  await save.listeners.click({});
  assert.equal(h.nodes.get('#login-view').classList.contains('hidden'), false);
});

test('vendor widgets stay off in the vm harness so tests do not fetch CDN modules', () => {
  const { app } = harness();
  assert.equal(app.vendorWidgetsEnabled(), false);
  assert.equal(app.shouldEnhanceSelect({ tag: 'select', id: 'guild-select', className: '', classList: { contains: () => false } }), true);
  assert.equal(app.shouldEnhanceSelect({ tag: 'select', id: '', className: 'searchable', classList: { contains: (n) => n === 'searchable' } }), true);
  assert.equal(app.shouldEnhanceSelect({ tag: 'select', id: '', className: 'theme-select', classList: { contains: (n) => n === 'theme-select' } }), false);
  assert.equal(app.shouldEnhanceSelect({ tag: 'select', id: '', className: '', classList: { contains: () => false }, options: { length: 20 } }), false);
});

test('Guild WebUI pulls daisyUI and Headless from pinned CDNs and keeps pale-blue tokens', () => {
  const html = fs.readFileSync(new URL('../frontend/index.html', import.meta.url), 'utf8');
  const js = fs.readFileSync(new URL('../frontend/app.js', import.meta.url), 'utf8');
  const css = fs.readFileSync(new URL('../frontend/style.css', import.meta.url), 'utf8');
  const { app } = harness();
  assert.match(html, /cdn\.jsdelivr\.net\/npm\/daisyui@5\.7\.43\/daisyui\.css/);
  assert.match(html, /link\.id = "jmodguard-daisyui"/);
  assert.match(html, /link\.href = window\.JMODGUARD_DAISYUI_HREF/);
  assert.doesNotMatch(html, /<link[^>]*id="jmodguard-daisyui"/);
  assert.ok(html.indexOf('jmodguard-daisyui') < html.indexOf('href="/style.css"'));
  assert.equal(app.VENDOR.daisyuiCss, 'https://cdn.jsdelivr.net/npm/daisyui@5.7.43/daisyui.css');
  assert.equal(app.VENDOR.esm, 'https://esm.sh');
  assert.equal(app.VENDOR.react, '19.3.0');
  assert.equal(app.VENDOR.headless, '2.2.10');
  assert.equal(app.vendorCssHref(), app.VENDOR.daisyuiCss);
  assert.equal(app.vendorEsmBase(), 'https://esm.sh');
  const names = ["Combobox", "ComboboxInput", "ComboboxOptions", "ComboboxOption"];
  const incomplete = { Combobox() {} };
  assert.equal(app.vendorModule(incomplete, names), null);
  const complete = { Combobox() {}, ComboboxInput() {}, ComboboxOptions() {}, ComboboxOption() {} };
  assert.equal(app.vendorModule(complete, names), complete);
  const asObjects = { Combobox: { $$typeof: 1 }, ComboboxInput: { $$typeof: 1 }, ComboboxOptions: { $$typeof: 1 }, ComboboxOption: { $$typeof: 1 } };
  assert.equal(app.vendorModule(asObjects, names), asObjects);
  const fromDefault = { Combobox() {}, ComboboxInput: undefined, default: complete };
  const merged = app.vendorModule(fromDefault, names);
  assert.equal(typeof merged.Combobox, "function");
  assert.equal(typeof merged.ComboboxInput, "function");
  assert.equal(typeof merged.ComboboxOptions, "function");
  assert.equal(typeof merged.ComboboxOption, "function");
  const { app: cssApp, window: cssWindow, nodes: cssNodes } = harness();
  cssWindow.JMODGUARD_DAISYUI_HREF = "https://cdn.example.test/daisyui.css";
  assert.equal(cssApp.vendorCssHref(), "https://cdn.example.test/daisyui.css");
  cssApp.applyVendorCss();
  assert.equal(cssNodes.get("#jmodguard-daisyui").getAttribute("href"), "https://cdn.example.test/daisyui.css");
  assert.match(js, /function applyVendorCss/);
  assert.match(js, /vendorModule\(headlessMod, \["Combobox", "ComboboxInput", "ComboboxOptions", "ComboboxOption"\]\)/);
  assert.match(js, /@headlessui\/react@/);
  assert.match(js, /esm\.sh/);
  assert.doesNotMatch(js, /node_modules/);
  assert.match(js, /function loadSearchSelectRuntime/);
  assert.match(js, /function restoreNativeSelect/);
  assert.match(js, /searchSelectId/);
  assert.match(js, /anchor:\s*"bottom start"/);
  assert.doesNotMatch(js, /^import /m);
  assert.match(css, /--bg:\s*#f0f7ff/);
  assert.match(css, /--accent:\s*#2266c2/);
  assert.match(css, /\[data-theme="dark"\][\s\S]*--bg:\s*#0d1727/);
  assert.match(css, /\[data-theme="dark"\][\s\S]*--accent:\s*#8bc1ff/);
  assert.match(css, /--color-primary:\s*var\(--accent\)/);
  assert.match(css, /--page-overflow:\s*visible/);
  assert.match(css, /#main-view\s*\{[^}]*display:\s*flex/);
  assert.match(css, /\.sidebar\s*\{[^}]*overflow:\s*visible/);
  assert.match(css, /#content\s*\{[^}]*max-width:\s*none/);
  assert.doesNotMatch(css, /\.sidebar\s*\{[^}]*position:\s*fixed/);
  assert.doesNotMatch(css, /\.hero\s*\{[^}]*display:\s*block/);
  assert.doesNotMatch(css, /\.hero\s*\{[^}]*background:/);
  assert.doesNotMatch(css, /\.stat-grid/);
  assert.doesNotMatch(css, /\.btn\.primary/);
  assert.match(css, /\.search-select-options\s*\{[^}]*z-index:\s*1100/);
  assert.match(css, /--color-base-100:\s*var\(--panel\)/);
  assert.match(js, /hero-content/);
  assert.match(js, /stat-title/);
  assert.match(js, /btn-primary/);
  assert.match(js, /dashboard-hero/);
  assert.match(js, /search-select-control/);
  assert.doesNotMatch(js, /btn-neutral/);
  assert.match(js, /class: "stats /);
  assert.match(html, /btn btn-primary/);
  assert.match(html, /class="navbar /);
  assert.match(css, /\.workspace-header\s*\{[^}]*justify-content:\s*space-between/);
  assert.match(html, /sidebar-footer[\s\S]*theme-picker[\s\S]*logout-btn/);
  assert.match(html, /class="menu w-full"/);
  assert.equal(app.uiClass("btn primary"), "btn btn-primary");
  assert.equal(app.uiClass("btn small danger"), "btn btn-sm btn-error");
  assert.equal(app.uiClass("btn outline"), "btn btn-outline");
  assert.match(app.uiClass("card"), /card-border/);
  assert.equal(fs.existsSync(new URL('../frontend/package.json', import.meta.url)), false);
  assert.equal(fs.existsSync(new URL('../frontend/node_modules', import.meta.url)), false);
});

test('unauthenticated log-channel mutation returns to login', async () => {
  const h = harness(async () => ({
    ok: false,
    status: 400,
    headers: { get: () => null },
    text: async () => JSON.stringify({ error: 'not authenticated' }),
  }));
  h.nodes.get('#login-view').classList.add('hidden');
  h.nodes.get('#main-view').classList.remove('hidden');
  const err = Object.assign(new Error('not authenticated'), { status: 400 });
  assert.equal(h.app.handleAuthLoss(err), true);
  assert.equal(h.nodes.get('#login-view').classList.contains('hidden'), false);
});

test('rule badges separate status from heading and preserve distinct semantic variants', () => {
  const { app } = harness();
  for (const [enabled, dry_run, variant] of [[true, false, 'badge-success'], [true, true, 'badge-warning'], [false, false, 'badge-error']]) {
    const card = app.ruleCard({ name: 'Mention spam', rule_id: 'mention_spam', global: {}, guild_override: {}, effective: { enabled, dry_run } });
    assert.equal(textOf(findNode(card, n => n.tag === 'h3')), 'Mention spam');
    const badge = findNode(card, n => String(n.className).split(/\s+/).includes(variant));
    assert.ok(badge, variant);
    assert.doesNotMatch(badge.className, /badge-ghost/);
    assert.ok(findNode(card, n => String(n.className).split(/\s+/).includes('scope-tag')));
    assert.ok(findNode(card, n => String(n.className).split(/\s+/).includes('override-tag')));
  }
});

test('escalation editor labels numeric fields and saves edited threshold, duration and reason values', async () => {
  let submitted;
  const h = harness(async (url, options) => {
    if (options.method === 'PUT') submitted = JSON.parse(options.body);
    return response({ settings: {}, rules: [], commands: [], log_channels: [], badwords: [] });
  });
  const modal = h.app.editRule({ name: 'Mention spam', rule_id: 'mention_spam', effective: { enabled: true, dry_run: true, params: {}, actions: [{ at: 1, action: 'ban', duration_minutes: 0, delete_days: 1, reason: 'spam' }] } });
  for (const [label, value] of [['Strike count', '3'], ['Timeout (minutes)', '15'], ['Delete history (days)', '2'], ['Reason template', 'Updated reason']]) {
    const wrapper = findNode(modal, n => n.tag === 'label' && textOf(n).startsWith(label));
    assert.ok(wrapper, label);
    const input = findNode(wrapper, n => n.tag === 'input');
    input.value = value;
    input.onchange();
  }
  await findNode(modal, n => n.textContent === 'Save').listeners.click({});
  assert.equal(submitted.data.actions[0].at, 3);
  assert.equal(submitted.data.actions[0].duration_minutes, 15);
  assert.equal(submitted.data.actions[0].delete_days, 2);
  assert.equal(submitted.data.actions[0].reason, 'Updated reason');
});
