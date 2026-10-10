#!/usr/bin/env node
import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';
import { randomUUID } from 'node:crypto';

import { JSDOM } from '../tools/webui/node_modules/jsdom/lib/api.js';
import React, { useSyncExternalStore, useState, useEffect, useRef, createContext, useContext } from '../tools/webui/node_modules/react/index.js';


const bootDom = new JSDOM('<html><body></body></html>', {pretendToBeVisual: true});
activate(bootDom.window);
const { createRoot: reactRoot } = await import('../tools/webui/node_modules/react-dom/client.js');
const { flushSync } = await import('../tools/webui/node_modules/react-dom/index.js');
const { Dialog, DialogPanel, Field, Label, HeadlessInput, HeadlessSelect, HeadlessTextarea } = await import('../tools/webui/components.js');
import { afterEach, after } from 'node:test';
const stateSource = fs.readFileSync(new URL("../frontend/state.js", import.meta.url), "utf8")
  .replace(/^import .*;$/gm, "").replaceAll("export function", "function");
const appearanceSource = fs.readFileSync(new URL('../frontend/appearance.js', import.meta.url), 'utf8')
  .replace(/^import .*;$/gm, '').replaceAll('export function', 'function');
const source = fs.readFileSync(new URL('../frontend/app.js', import.meta.url), 'utf8')
  .replace(/^import[\s\S]*?;$/gm, '').replace(/\ncapturePreviewToken\(\);\nroot.render\(h\(App\)\);\ninit\(\);\n/, '\n');
const pages = [];
after(() => bootDom.window.close());
function activate(window) {
  globalThis.window = window; globalThis.document = window.document;
  for (const name of ['HTMLElement', 'HTMLButtonElement', 'HTMLInputElement', 'HTMLSelectElement', 'HTMLTextAreaElement', 'Element', 'Node', 'NodeFilter', 'MutationObserver', 'Event', 'MouseEvent', 'KeyboardEvent']) globalThis[name] = window[name];
  globalThis.ResizeObserver = class { observe() {} unobserve() {} disconnect() {} };
  globalThis.getComputedStyle = window.getComputedStyle.bind(window);
  globalThis.requestAnimationFrame = window.requestAnimationFrame.bind(window);
  globalThis.cancelAnimationFrame = window.cancelAnimationFrame.bind(window);
}
afterEach(async () => {
  for (const { dom, roots } of pages.splice(0)) {
    activate(dom.window);
    for (const root of roots) flushSync(() => root.unmount());
    dom.window.close();
  }
});
async function settle() { for (let i = 0; i < 3; i++) { await new Promise(resolve => setImmediate(resolve)); flushSync(() => {}); } }
function fire(element, type, props = {}) {
  assert.ok(element, `event ${type} needs a mounted element`);
  activate(element.ownerDocument.defaultView);
  // React's value tracker must observe a browser edit, not a scripted setter.
  if (['input', 'change'].includes(type) && element._valueTracker) element._valueTracker.setValue('previous browser value');
  if (type === 'change' && element.type === 'checkbox') {
    const desired = props.target?.checked ?? element.checked;
    element.checked = !desired;
    type = 'click';
  }
  const EventType = type === 'click' ? window.MouseEvent : type.startsWith('key') ? window.KeyboardEvent : window.Event;
  flushSync(() => element.dispatchEvent(new EventType(type, { bubbles: true, cancelable: true, ...props })));
  return settle();
}
function harness(fetchImpl = async () => {}, storage = new Map(), session = new Map(), confirm = () => true, timerImpl = null, asyncRoot = false) {
  const dom = new JSDOM('<!doctype html><html><head><meta name="theme-color"></head><body><div id="app"></div><main id="content"></main></body></html>', {url:'https://ui.example.test', pretendToBeVisual: true});
  const win = dom.window, document = win.document;
  activate(win);
  Object.defineProperty(document, 'visibilityState', {value: 'visible', writable: true});
  Object.defineProperty(win, 'localStorage', {value: { getItem: k => storage.get(k) ?? null, setItem: (k,v) => storage.set(k,v) }, configurable: true});
  Object.defineProperty(win, 'sessionStorage', {value: { getItem: k => session.get(k) ?? null, setItem: (k,v) => session.set(k,v), removeItem: k => session.delete(k) }, configurable: true});
  win.matchMedia = () => ({matches: false, addEventListener() {}, removeEventListener() {}});
  const roots = [], timers = new Map(), documentListeners = new Map(), opened = [], historyUrls = [];
  let nextTimer = 1, clockOffsetMs = 0, rendering = false;
  class TestDate extends Date { static now() { return Date.now() + clockOffsetMs; } }
  const location = { search: '', pathname: '/', hash: '', replaced: null, replace(value) { this.replaced = value; } };
  win.open = (...args) => opened.push(args);
  const addEventListener = document.addEventListener.bind(document), removeEventListener = document.removeEventListener.bind(document);
  document.addEventListener = (name, fn, ...args) => { documentListeners.set(name, fn); addEventListener(name, fn, ...args); };
  document.removeEventListener = (name, fn, ...args) => { if (documentListeners.get(name) === fn) documentListeners.delete(name); removeEventListener(name, fn, ...args); };
  const timerApi = timerImpl ?? { setTimeout(fn, delay) { const id = nextTimer++; timers.set(id, { fn: (...args) => { let result; flushSync(() => { result = fn(...args); }); return Promise.resolve(result).then(settle); }, delay }); return id; }, clearTimeout(id) { timers.delete(id); } };
  function createRoot(container) {
    const root = reactRoot(container); roots.push(root);
    return { render(element) {
      activate(win);
      if (rendering || asyncRoot) { root.render(element); return; }
      rendering = true; try { flushSync(() => root.render(element)); } finally { rendering = false; }
    } };
  }
  const appWindow = new Proxy(win, {
    get(target, key) { return key === 'location' ? location : Reflect.get(target, key); },
    set(target, key, value) { if (key === 'location') { location.href = value; return true; } return Reflect.set(target, key, value); },
  });
  const context = vm.createContext({ document, window: appWindow, location, history: { replaceState(_state, _unused, url) { historyUrls.push(url); } },
    React, useSyncExternalStore, useState, useEffect, useRef, createContext, useContext, createRoot,
    Dialog, DialogPanel, Field, Label, HeadlessInput, HeadlessSelect, HeadlessTextarea,
    fetch: fetchImpl, console, AbortController, URLSearchParams, Event: win.Event, Date: TestDate,
    setTimeout: timerApi.setTimeout, clearTimeout: timerApi.clearTimeout, confirm, crypto: {randomUUID}, navigator: win.navigator,
  });
  const app = vm.runInContext(`${stateSource}\n${appearanceSource}\n${source}\n({state, api, init, downloadPrivate, submitBotAction, applyQueuedLogChannel, reconcileQueuedLogChannel, actionReconcileDelays, tabActions, tabBans, tabMembers, tabCases, tabTemplates, tabLogChannels, restoreWorkspace, persistWorkspace, workspaceKey, loadConfig, applyConfigPatch, handleEvent, tabDashboard, tabSettings, renderTab, ruleCard, spamConfigured, editRule, editCommand, currentDrafts, navigate, capturePreviewToken, currentPreviewToken, returnFromPreview, parseSnowflake, splitIds, handleAuthLoss, markDirty, isDirty: () => ui.getSnapshot().dirty, resetDirty: () => { ui.set({ dirty: false }); }, isBusy: () => ui.getSnapshot().busy, OwnerTools, buildCustomRuleSaveBody, newCustomRuleDraft, isCustomRuleId})`, context);
  app.state.me = { user: { id: '123456789012345678' } };
  app.state.guilds = [{ id: '987654321098765432', name: 'First' }, { id: '987654321098765433', name: 'Second' }];
  app.state.guildId = app.state.guilds[0].id;
  const fixtureContent = document.querySelector('#content');
  function mount(Component, props) {
    const container = document.createElement('div'); container.className = 'test-panel'; fixtureContent.append(container);
    createRoot(container).render(React.createElement(Component, props));
    return container;
  }
  for (const name of ['tabActions','tabBans','tabMembers','tabCases','tabTemplates','tabLogChannels','tabDashboard','tabSettings']) {
    const Component = app[name];
    app[name] = () => mount(Component);
  }
  const card = app.ruleCard; app.ruleCard = record => mount(() => card(record));
  app.bindOwnerTools = () => mount(app.OwnerTools);
  for (const name of ['renderTab', 'navigate']) {
    const fn = app[name]; app[name] = (...args) => {
      fixtureContent.remove();
      flushSync(() => { vm.runInContext('ui.set({ view: "main-view" });', context); fn(...args); });
      return document.querySelector('.modal-overlay');
    };
  }
  for (const name of ['editRule', 'editCommand']) {
    const fn = app[name]; app[name] = async (...args) => {
      fixtureContent.remove();
      flushSync(() => { vm.runInContext('ui.set({ view: "main-view" });', context); fn(...args); }); await settle();
      return document.querySelector('.modal-overlay');
    };
  }
  const nodes = { get: selector => document.querySelector(selector) };
  const collect = () => Array.from(document.querySelectorAll('#content *, .modal-overlay *, #owner-tools *, .test-panel *'));
  const elements = { find: predicate => collect().find(predicate), filter: predicate => collect().filter(predicate), some: predicate => collect().some(predicate), [Symbol.iterator]: function* () { yield* collect(); } };
  vm.runInContext('root.render(h(App));', context);
  pages.push({dom, roots});
  return { app, elements, nodes, storage, session, location, opened, historyUrls, window: win, timers, document, documentListeners, advanceTime(ms) { clockOffsetMs += ms; } };
}

const response = (data) => ({ ok: true, text: async () => JSON.stringify(data) });

function findNode(root, predicate) {
  if (!root) return null;
  if (predicate(root)) return root;
  for (const child of root.children || []) {
    const found = findNode(child, predicate);
    if (found) return found;
  }
  return null;
}

function textOf(root) {
  return root.textContent;
}

async function runMemberPoll(h) {
  const pending = [...h.timers].find(([, timer]) => timer.delay === 10000);
  assert(pending, 'expected a visible Members status poll');
  h.timers.delete(pending[0]);
  await pending[1].fn();
  await settle();
}

test('Bans shows known names and actor with exact IDs and honest unknown fallbacks', async () => {
  const h = harness(async () => response([
    { user_id: '172697277964615681', user_name: 'wavingwalker', user_display_name: 'Wavingwalker', moderator_id: '1800882749273538599', moderator_name: 'ikaika44', active: true, reason: 'No reason provided', created_at: '2026-09-22T23:16:28Z' },
    { user_id: '823826232020959242', user_name: null, moderator_id: null, moderator_name: null, active: true, reason: null, created_at: '2026-09-22T07:43:34Z' },
  ]));
  const panel = h.app.tabBans();
  await settle();
  const text = textOf(panel);
  assert.match(text, /Wavingwalker \(wavingwalker, 172697277964615681\)/);
  assert.match(text, /ikaika44 \(1800882749273538599\)/);
  assert.match(text, /823826232020959242/);
  assert.match(text, /Unavailable/);
  assert.match(text, /Reason unavailable/);
});

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
  const value = (root, label) => Array.from(findNode(root, n => n.className === 'stat' && textOf(n).includes(label)).children).find((c) => c.className === 'stat-value').textContent;
  pending[1].resolve(response({ events: 7, warns: 2, bans: 1, cases: 4 }));
  await settle();
  assert.equal(value(second, 'Moderation records'), '7');
  assert.equal(value(second, 'Warns'), '2');
  assert.equal(value(second, 'Bans'), '1');
  assert.equal(value(second, 'Cases'), '4');
  pending[0].resolve(response({ events: 900, warns: 8, bans: 3 }));
  await settle();
  assert.equal(value(first, 'Moderation records'), '900');
  assert.equal(value(second, 'Moderation records'), '7');
});

test('dashboard does not show zero moderation counts when stats are unavailable', async () => {
  const { app } = harness(async () => { throw new Error('offline'); });
  app.state.config = { settings: { prefix: '!', bot_name: '' }, rules: [], commands: [], log_channels: [], badwords: [] };
  const dashboard = app.tabDashboard();
  await settle();
  for (const label of ['Moderation records', 'Warns', 'Bans', 'Cases']) {
    const card = findNode(dashboard, n => n.className === 'stat' && textOf(n).includes(label));
    assert.equal(Array.from(card.children).find((c) => c.className === 'stat-value').textContent, 'Unavailable');
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

test('saved Members tab restores for the selected guild', () => {
  const { app, storage } = harness();
  const guildId = app.state.guilds[1].id;
  storage.set(app.workspaceKey(), JSON.stringify({ guildId, tab: 'members' }));
  app.restoreWorkspace();
  assert.equal(app.state.guildId, guildId);
  assert.equal(app.state.tab, 'members');
});

const flushMembers = settle;
function memberTimers() {
  const active = new Map();
  let next = 0;
  return { active, setTimeout(fn, delay) { const id = ++next; active.set(id, { fn, delay }); return id; }, clearTimeout(id) { active.delete(id); } };
}
function memberView(h) {
  h.app.state.config = { enrollment_id: 'membership' };
  h.app.state.tab = 'members';
  return h.app.tabMembers();
}

for (const completed of [false, true]) test(`Members reads status after success before releasing controls (${completed ? 'already completed' : 'pause applied'})`, async () => {
  const requests = [];
  const timers = memberTimers();
  const h = harness((url, opts) => new Promise(resolve => requests.push({ url, opts, resolve })), new Map(), new Map(), undefined, timers);
  const panel = memberView(h);
  const syncReads = () => requests.filter(r => r.url.endsWith('/member-sync'));
  syncReads()[0].resolve(response({ run_id: 'run', status: 'running', phase: 'members', progress_revision: 10 }));
  requests.find(r => r.url.includes('/members?')).resolve(response({ members: [] }));
  await flushMembers();
  const click = fire(findNode(panel, n => n.textContent === 'Pause'), 'click', {});
  requests.find(r => r.url.endsWith('/member-sync/actions')).resolve(response(queuedAction('pending')));
  await click;
  // This primary read began before the worker completed; its old status is legitimate.
  syncReads()[1].resolve(response({ run_id: 'run', status: 'running', phase: 'members', progress_revision: 10 }));
  await flushMembers();
  requests.find(r => r.url === `/api/guilds/${h.app.state.guildId}/actions`).resolve(response([{ ...queuedAction('succeeded'), outcome: completed ? 'member_sync_already_completed' : 'member_sync_updated' }]));
  await flushMembers();
  assert.equal(syncReads().length, 3, 'success must issue a fresh status request');
  assert.match(textOf(panel), /Waiting for the import status to reflect it/);
  assert.equal(findNode(panel, n => n.textContent === 'Pause').disabled, true);
  assert.ok([...timers.active.values()].some(t => t.delay === 10000));
  // A completed-import no-op succeeds without incrementing progress_revision.
  syncReads()[2].resolve(response({ run_id: 'run', status: completed ? 'complete' : 'paused', phase: completed ? 'complete' : 'members', progress_revision: completed ? 10 : 11 }));
  await flushMembers();
  assert.doesNotMatch(textOf(panel), /Waiting for the import status to reflect it/);
  assert.match(textOf(panel), completed ? /complete · complete/ : /members · paused/);
  assert.equal([...timers.active.values()].filter(t => t.delay === 10000).length, 0);
  for (const request of requests) assert.equal(request.opts.headers['x-jmodguard-enrollment'], 'membership');
});

for (const code of ['member_sync_missing_job', 'member_sync_invalid_operation']) test(`Members shows ${code} inline and refreshes rejected controls`, async () => {
  const calls = [];
  const message = code === 'member_sync_missing_job' ? 'Import job is unavailable.' : 'This control is no longer available.';
  const h = harness(async (url, opts) => {
    calls.push({ url, opts });
    if (url.endsWith('/member-sync/actions')) return { ok: false, status: 409, headers: { get: () => null }, text: async () => JSON.stringify({ code, error: message }) };
    if (url.endsWith('/member-sync')) return response(calls.some(c => c.url.endsWith('/member-sync/actions'))
      ? { run_id: code === 'member_sync_missing_job' ? null : 'run', status: code === 'member_sync_missing_job' ? 'needs_attention' : 'paused' }
      : { run_id: 'run', status: 'running', phase: 'members' });
    return response({ members: [] });
  });
  const panel = memberView(h);
  await flushMembers();
  await fire(findNode(panel, n => n.textContent === 'Pause'), 'click', {});
  await flushMembers();
  assert.match(textOf(panel), new RegExp(message.replace(/\./g, '\\.')));
  assert.equal(findNode(panel, n => n.textContent === 'Pause').disabled, true);
  assert.equal(calls.filter(c => c.url.endsWith('/member-sync')).length, 2);
  assert.equal(calls.filter(c => c.url.endsWith('/member-sync/actions')).length, 1);
  assert.equal(h.session.size, 1, 'denial must preserve the submitted request identity');
});

for (const route of ['same guild', 'late callback', 'cancelled navigation']) test(`Members case handoff: ${route}`, async () => {
  const calls = [];
  const h = harness(async url => {
    calls.push(url);
    if (url.endsWith('/member-sync')) return response({ status: 'complete' });
    if (url.includes('/members?')) return response({ members: [{ user_id: '7' }] });
    if (url.includes('/members/7/history')) return response({ entries: [{ kind: 'ban', case_id: 'case-one' }] });
    return response({ cases: [], history: [] });
  }, new Map(), new Map(), () => false);
  const panel = memberView(h);
  const originalGuild = h.app.state.guildId;
  await flushMembers();
  await fire(findNode(panel, n => n.localName === 'button' && n.textContent === 'View history'), 'click', {});
  await flushMembers();
  const jump = findNode(panel, n => n.localName === 'button' && n.textContent === 'Case case-one');
  if (route === 'late callback') h.app.state.guildId = h.app.state.guilds[1].id;
  if (route === 'cancelled navigation') h.app.markDirty();
  await fire(jump, 'click', {});
  await flushMembers();
  if (route === 'same guild') {
    assert.ok(calls.includes(`/api/guilds/${originalGuild}/cases/case-one`));
    assert.equal(h.app.state.tab, 'cases');
  } else {
    h.app.state.guildId = h.app.state.guilds[1].id;
    h.app.tabCases();
    await flushMembers();
    assert.equal(calls.filter(url => url.includes('/cases/case-one')).length, 0, 'old case intent must not follow a changed/cancelled view');
  }
});

test('Members empty copy follows status completion without refetching the directory', async () => {
  const timers = memberTimers();
  const calls = [];
  let sync = { run_id: 'run', status: 'running', phase: 'members', progress_revision: 1 };
  const h = harness(async url => { calls.push(url); return response(url.endsWith('/member-sync') ? sync : { members: [] }); }, new Map(), new Map(), undefined, timers);
  const panel = memberView(h);
  await flushMembers();
  assert.match(textOf(panel.querySelector('tbody')), /Import may still be incomplete/);
  const [id, poll] = [...timers.active].find(([, timer]) => timer.delay === 10000);
  timers.active.delete(id);
  sync = { run_id: 'run', status: 'complete', phase: 'complete', progress_revision: 2 };
  await poll.fn();
  await settle();
  assert.match(textOf(panel), /complete · complete/);
  assert.equal(textOf(panel.querySelector('tbody')), 'No members match these filters.');
  assert.equal(calls.filter(url => url.includes('/members?')).length, 1);
});

test('Members uses saved identity, keeps large IDs exact and labels partial import', async () => {
  const calls = [];
  const h = harness(async (url, opts) => {
    calls.push({ url, opts });
    if (url.endsWith('/member-sync')) return response({ status: 'running', phase: 'members', run_id: 'one', progress_revision: 2, processed: 42, total_estimate: 100, percent: 42, as_of: 1790000000000 });
    return response({ members: [{ user_id: '1800882749273538599', display_name: '🌻 Willow', username: 'willow', is_current: true, is_banned: false, ever_banned: false, joined_at_ms: null }], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  assert.match(textOf(panel), /🌻 Willow/);
  assert.match(textOf(panel), /1800882749273538599/);
  assert.match(textOf(panel), /Results are partial/);
  assert.match(textOf(panel), /Unknown/);
  assert.equal(calls.length, 2);
  assert(calls.every(call => call.opts.method === 'POST' && call.opts.headers['x-jmodguard-enrollment'] === 'member-enrollment'));
});

test('Members separates current ban reason from an unresolved Gateway action', async () => {
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync')) return response({ status: 'complete', phase: 'complete', progress_revision: 3, initial_completed_at: 1790000000000 });
    if (url.includes('/history')) return response({ entries: [{ kind: 'ban', occurred_at_ms: 1790000000000, source_kind: 'gateway_observation_unlinked', case_id: 'case-1' }], next_cursor: null });
    return response({ members: [{ user_id: '1800882749273538599', display_name: 'Willow', is_current: false, is_banned: true, ever_banned: true, current_ban_reason: 'Current rule' }], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  assert.match(textOf(panel), /Current reason: Current rule/);
  fire(findNode(panel, n => n.localName === 'button' && textOf(n) === 'View history'), 'click');
  await settle();
  assert.match(textOf(panel), /Gateway observation; action time unconfirmed/);
  assert.match(textOf(panel), /Case case-1/);
});

test('Members renders a dated unresolved Discord audit entry, then its exact case link', async () => {
  let linked = false;
  const reason = '<img src=x onerror=alert(1)>';
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync')) return response({ status: 'complete', phase: 'complete', progress_revision: 3, initial_completed_at: 1790000000000 });
    if (url.includes('/history')) return response({ entries: [linked
      ? { kind: 'ban', occurred_at_ms: 1790000000000, source_kind: 'application_audit_linked', case_id: 'case-linked', observation_id: null, moderator_id: '42', reason, outcome: 'succeeded' }
      : { kind: 'ban', occurred_at_ms: 1790000000000, source_kind: 'discord_audit_unresolved', case_id: null, observation_id: '123456789012345678', moderator_id: '42', reason, outcome: 'succeeded' }], next_cursor: null });
    return response({ members: [{ user_id: '1800882749273538599', display_name: 'Willow', is_current: false, is_banned: false, ever_banned: true }], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  const historyButton = findNode(panel, n => n.localName === 'button' && textOf(n) === 'View history');
  fire(historyButton, 'click');
  await settle();
  assert.match(textOf(panel), /Ban audit entry · /);
  assert.doesNotMatch(textOf(panel), /Ban audit entry · Unknown/);
  assert.match(textOf(panel), /Confirmed Discord audit action; local case association unresolved/);
  assert.match(textOf(panel), /Audit entry 123456789012345678/);
  assert.match(textOf(panel), /moderator 42/);
  assert.match(textOf(panel), /<img src=x onerror=alert\(1\)>/);
  assert.equal(findNode(panel, n => n.localName === 'button' && textOf(n).startsWith('Case ')), null);
  assert.equal(findNode(panel, n => n.localName === 'img'), null, 'audit reason is text, not markup');

  linked = true;
  fire(historyButton, 'click');
  await settle();
  assert.doesNotMatch(textOf(panel), /local case association unresolved/);
  assert.doesNotMatch(textOf(panel), /Audit entry 123456789012345678/);
  assert.match(textOf(panel), /Bot action linked to Discord audit/);
  assert.match(textOf(panel), /Case case-linked/);
});

test('Members labels verified state changes with unknown event time and separate check time', async () => {
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync')) return response({ status: 'complete', phase: 'complete', progress_revision: 3, initial_completed_at: 1790000000000, reconciliation_needed: true, audit_recovery_status: 'waiting', audit_recovery_phase: 'unban', audit_recovery_processed: 3, audit_recovery_reason: 'rate_limited', audit_recovery_next_attempt_at: 1790000000000, audit_recovery_last_progress_at: 1789999990000 });
    if (url.includes('/history')) return response({ entries: [
      { kind: 'member_state_observation', occurred_at_ms: null, observed_at_ms: 1790000000000, source_kind: 'state_reconciliation', case_id: null },
      { kind: 'ban_absent_state_observation', occurred_at_ms: null, observed_at_ms: 1790000000000, source_kind: 'state_reconciliation', case_id: null },
      { kind: 'ban_present_state_observation', occurred_at_ms: null, observed_at_ms: 1790000000000, source_kind: 'state_reconciliation', case_id: null },
      { kind: 'membership_state_observation', occurred_at_ms: null, observed_at_ms: 1790000000000, source_kind: 'state_reconciliation', case_id: null },
    ], next_cursor: null });
    return response({ members: [{ user_id: '1800882749273538599', display_name: 'Willow', is_current: false, is_banned: false, ever_banned: true }], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  fire(findNode(panel, n => n.localName === 'button' && textOf(n) === 'View history'), 'click');
  await settle();
  assert.match(textOf(panel), /Membership absence · Unknown/);
  assert.match(textOf(panel), /Membership state check · Unknown/);
  assert.match(textOf(panel), /Ban absent \(time unknown\) · Unknown/);
  assert.match(textOf(panel), /Ban present \(time unknown\) · Unknown/);
  assert.doesNotMatch(textOf(panel), /Ban present \(time unknown\) · \d{4}/);
  assert.match(textOf(panel), /Checked /);
  assert.match(textOf(panel), /Bounded audit recovery: Unban actions · waiting · 3 entries checked · rate_limited/);
  assert.match(textOf(panel), /Audit recovery next attempt:/);
  assert.match(textOf(panel), /Audit recovery last progressed:/);
  assert.doesNotMatch(textOf(panel), /Case /);
});

test('Members keeps polling audit-only recovery while visible and stops when it completes or hides', async () => {
  let statusReads = 0;
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync')) {
      statusReads++;
      return response({ status: 'complete', phase: 'complete', progress_revision: 7, initial_completed_at: 1790000000000,
        audit_recovery_status: statusReads < 3 ? 'waiting' : 'complete', audit_recovery_phase: 'ban' });
    }
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  h.app.tabMembers();
  await settle();
  assert.equal(statusReads, 1);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 1);
  h.document.visibilityState = 'hidden';
  h.documentListeners.get('visibilitychange')();
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 0);
  h.document.visibilityState = 'visible';
  h.documentListeners.get('visibilitychange')();
  await settle();
  assert.equal(statusReads, 2);
  await runMemberPoll(h);
  assert.equal(statusReads, 3);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 0);
});

test('Members follows a succeeded Resume receipt with a fresh primary status read', async () => {
  const guildId = '987654321098765432';
  const action = { id: 'sync-action-1', guild_id: guildId, actor_id: '123456789012345678', enrollment_id: 'member-enrollment', state: 'pending' };
  const statuses = [
    { status: 'paused', progress_revision: 5 },
    { status: 'paused', progress_revision: 5 },
    { status: 'queued', progress_revision: 6 },
    { status: 'complete', progress_revision: 7 },
  ];
  let statusReads = 0;
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync/actions')) return response(action);
    if (url.endsWith('/member-sync')) return response({ phase: 'members', run_id: 'initial-1', initial_completed_at: null, ...statuses[statusReads++] });
    if (url.endsWith('/actions')) return response([action]);
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  assert.equal(statusReads, 1);
  await fire(findNode(panel, n => n.localName === 'button' && textOf(n) === 'Resume'), 'click');
  await settle();
  assert.match(textOf(panel), /Resume requested · bot action pending/);
  assert.equal(statusReads, 2);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 1);
  action.state = 'succeeded';
  await runMemberPoll(h);
  assert.equal(statusReads, 4);
  assert.doesNotMatch(textOf(panel), /Resume requested/);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 0);
});

test('Members clears a Pause that lands after the import already completed', async () => {
  const guildId = '987654321098765432';
  const action = { id: 'sync-action-pause-late', guild_id: guildId, actor_id: '123456789012345678', enrollment_id: 'member-enrollment', state: 'pending' };
  let statusReads = 0;
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync/actions')) return response(action);
    if (url.endsWith('/member-sync')) {
      statusReads++;
      return response(statusReads === 1
        ? { status: 'running', phase: 'members', run_id: 'initial-1', progress_revision: 4 }
        : { status: 'complete', phase: 'complete', run_id: 'initial-1', progress_revision: 4, initial_completed_at: 1790000000000 });
    }
    if (url.endsWith('/actions')) return response([{ ...action, outcome: 'member_sync_already_completed' }]);
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  await fire(findNode(panel, n => n.localName === 'button' && textOf(n) === 'Pause'), 'click');
  await settle();
  assert.match(textOf(panel), /Pause requested/);
  action.state = 'succeeded';
  await runMemberPoll(h);
  assert.doesNotMatch(textOf(panel), /Pause requested/);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 0);
});

test('Members releases a reused successful Retry after reading current status', async () => {
  const guildId = '987654321098765432';
  const session = new Map();
  const receipt = { id: 'earlier-retry', guild_id: guildId, actor_id: '123456789012345678', enrollment_id: 'member-enrollment', state: 'succeeded' };
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync/actions')) return response(receipt);
    if (url.endsWith('/member-sync')) return response({ status: 'waiting', phase: 'members', run_id: 'initial-1', progress_revision: 8 });
    if (url.endsWith('/actions')) return response([receipt]);
    return response({ members: [], next_cursor: null });
  }, new Map(), session);
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const key = `${h.app.workspaceKey()}:action:POST:/api/guilds/${guildId}/member-sync/actions:{"operation":"retry"}`;
  session.set(key, JSON.stringify({ request_id: '1000_previous', enrollment_id: 'member-enrollment' }));
  const panel = h.app.tabMembers();
  await settle();
  const retry = findNode(panel, n => n.localName === 'button' && textOf(n) === 'Retry');
  assert(retry && !retry.disabled);
  await fire(retry, 'click');
  await settle();
  assert.doesNotMatch(textOf(panel), /Retry requested/);
  assert.equal(findNode(panel, n => n.localName === 'button' && textOf(n) === 'Retry').disabled, false);
});

test('Members stops pending-control polling when the bot action fails', async () => {
  const action = { id: 'sync-action-failed', guild_id: '987654321098765432', actor_id: '123456789012345678', enrollment_id: 'member-enrollment' };
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync/actions')) return response({ ...action, state: 'pending' });
    if (url.endsWith('/member-sync')) return response({ status: 'paused', phase: 'members', run_id: 'initial-1', progress_revision: 5 });
    if (url.endsWith('/actions')) return response([{ ...action, state: 'failed' }]);
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  await fire(findNode(panel, n => n.localName === 'button' && textOf(n) === 'Resume'), 'click');
  await settle();
  assert.match(textOf(panel), /resume action failed. Check Bot actions for details/);
  assert.doesNotMatch(textOf(panel), /Resume requested/);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 0);
});

test('Members retains an acknowledged control and retries after a status refresh error', async () => {
  let statusReads = 0;
  const action = { id: 'sync-action-delayed', guild_id: '987654321098765432', actor_id: '123456789012345678', enrollment_id: 'member-enrollment', state: 'pending' };
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync/actions')) return response(action);
    if (url.endsWith('/member-sync')) {
      statusReads++;
      if (statusReads > 1) throw new Error('temporary outage');
      return response({ status: 'paused', phase: 'members', run_id: 'initial-1', progress_revision: 5 });
    }
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  await fire(findNode(panel, n => n.localName === 'button' && textOf(n) === 'Resume'), 'click');
  await settle();
  assert.match(textOf(panel), /Resume requested · bot action pending/);
  assert.match(textOf(panel), /Could not refresh import status/);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 1);
});

test('Members retries its first failed visible status read and renders the recovered status', async () => {
  let statusReads = 0;
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync')) {
      statusReads++;
      if (statusReads === 1) throw new Error('temporary outage');
      return response({ status: 'complete', phase: 'complete', run_id: 'restored-run', progress_revision: 2,
        initial_completed_at: 1790000000000, processed: 12, total_estimate: 12, total_is_estimated: false, percent: 100 });
    }
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  assert.match(textOf(panel), /Could not load import status. Retrying shortly while visible/);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 1);
  await runMemberPoll(h);
  assert.equal(statusReads, 2);
  assert.match(textOf(panel), /12 records committed in this run/);
  assert.doesNotMatch(textOf(panel), /Could not load import status/);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 0);
});

test('Members accepts a later primary restore status but rejects an older in-flight response', async () => {
  let statusReads = 0;
  let resolveOldRequest;
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync')) {
      statusReads++;
      if (statusReads === 2) return new Promise(resolve => { resolveOldRequest = resolve; });
      return response(statusReads === 1
        ? { status: 'complete', phase: 'complete', run_id: 'before-restore', progress_revision: 20, processed: 99 }
        : { status: 'complete', phase: 'complete', run_id: 'restored-run', progress_revision: 3, processed: 3,
            initial_completed_at: 1790000000000, reconciliation_needed: true });
    }
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  const refresh = findNode(panel, n => n.localName === 'button' && textOf(n) === 'Refresh');
  fire(refresh, 'click');
  fire(refresh, 'click');
  await settle();
  assert.equal(statusReads, 3);
  assert.match(textOf(panel), /3 records committed in this run/);
  assert.match(textOf(panel), /Coverage may be stale after an outage, restore, bot restart, or Gateway session replacement/);
  resolveOldRequest(response({ status: 'complete', phase: 'complete', run_id: 'before-restore', progress_revision: 21, processed: 777 }));
  await settle();
  assert.match(textOf(panel), /3 records committed in this run/);
  assert.doesNotMatch(textOf(panel), /777 records committed/);
});

test('Members keeps a restore-interrupted control unconfirmed rather than claiming it applied', async () => {
  const action = { id: 'sync-action-before-restore', guild_id: '987654321098765432', actor_id: '123456789012345678', enrollment_id: 'member-enrollment', state: 'pending' };
  let statusReads = 0;
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync/actions')) return response(action);
    if (url.endsWith('/member-sync')) return response(statusReads++ === 0
      ? { status: 'paused', phase: 'members', run_id: 'before-restore', progress_revision: 10 }
      : { status: 'paused', phase: 'members', run_id: 'restored-run', progress_revision: 2, reconciliation_needed: true });
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  await fire(findNode(panel, n => n.localName === 'button' && textOf(n) === 'Resume'), 'click');
  await settle();
  assert.match(textOf(panel), /Import status changed after a restore. Check Bot actions/);
  assert.doesNotMatch(textOf(panel), /Resume requested/);
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 10000).length, 0);
});

test('Members shows 100 percent on completed status and labels a live member ETA preliminary', async () => {
  let statusReads = 0;
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync')) return response(statusReads++ === 0
      ? { status: 'queued', phase: 'members', run_id: 'initial-1', progress_revision: 1, processed: 10,
          total_estimate: 100, total_is_estimated: true, percent: 10, phase_remaining_seconds: 900,
          phase_finish_at: 1790000900000, estimate_reason: 'preliminary member-stage estimate; later stages have unknown totals' }
      : { status: 'complete', phase: 'complete', run_id: 'initial-1', progress_revision: 2, processed: 100,
          total_estimate: 100, total_is_estimated: false, percent: 100 });
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  assert.match(textOf(panel), /Preliminary member-stage estimate; later stages have unknown totals/);
  assert.match(textOf(panel), /10% of estimated member phase/);
  await runMemberPoll(h);
  const bar = findNode(panel, n => n.localName === 'progress');
  assert.equal(bar.getAttribute("value"), '100');
  assert.equal(bar.getAttribute('aria-valuetext'), 'Complete');
  assert.match(textOf(panel), /100% complete/);
  assert.doesNotMatch(textOf(panel), /99%/);
});

test('Members keeps completed phases done while local case history finishes and shows phase counts', async () => {
  const h = harness(async (url) => url.endsWith('/member-sync')
    ? response({ status: 'waiting', phase: 'complete', run_id: 'initial-1', progress_revision: 12,
        status_reason: 'finishing local case history', processed: 24, member_processed: 20,
        ban_processed: 3, verify_processed: 1, audit_processed: 2,
        local_history_complete: false, total_estimate: 20, total_is_estimated: false,
        percent: null, as_of: 1790000000000 })
    : response({ members: [], next_cursor: null }));
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  const copy = textOf(panel);
  assert.match(copy, /Members: done · 20 committed/);
  assert.match(copy, /Current bans: done · 3 committed/);
  assert.match(copy, /Candidate checks: done · 1 committed/);
  assert.match(copy, /Available audit history: done · 2 committed/);
  assert.match(copy, /Local case history: waiting/);
  assert.doesNotMatch(copy, /100% complete/);
  const bar = findNode(panel, n => n.localName === 'progress');
  assert.equal(bar.getAttribute("value"), null);
});

test('Members shows an unknown ETA without calling an unconfirmed paced worker offline', async () => {
  const h = harness(async (url) => url.endsWith('/member-sync')
    ? response({ status: 'queued', phase: 'bans', progress_revision: 9, processed: 80, total_estimate: 80, total_is_estimated: false,
        estimate_reason: 'worker_unconfirmed', phase_remaining_seconds: null, phase_finish_at: 1790999999999, as_of: 1790000000000 })
    : response({ members: [], next_cursor: null }));
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  const copy = textOf(panel);
  assert.match(copy, /80 members counted/);
  assert.match(copy, /Current bans: queued · committed count unknown/);
  assert.match(copy, /Remaining: Unknown until worker activity is confirmed · Expected finish: Unknown/);
  assert.match(copy, /bot may be between paced pages/);
  assert.doesNotMatch(copy, /worker is offline/i);
});

test('Members keeps controls unavailable until a real first job exists and after corrupt metadata loss', async () => {
  const statuses = [
    { status: 'queued', phase: 'members', run_kind: 'initial', run_id: null, progress_revision: 0,
      status_reason: 'awaiting first bot initialization' },
    { status: 'needs_attention', phase: null, run_id: null, progress_revision: 0,
      status_reason: 'member import metadata is missing' },
    { status: 'queued', phase: 'members', run_kind: 'initial', run_id: 'real-first-run', progress_revision: 1 },
  ];
  let reads = 0, actionPosts = 0;
  const h = harness(async (url) => {
    if (url.endsWith('/member-sync/actions')) { actionPosts++; return response({ id: 'action', state: 'pending' }); }
    if (url.endsWith('/member-sync')) return response(statuses[reads++]);
    return response({ members: [], next_cursor: null });
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  const controls = () => ['Pause', 'Resume', 'Retry', 'Reconcile from Discord']
    .map(label => findNode(panel, n => n.localName === 'button' && textOf(n) === label));
  assert.match(textOf(panel), /bot will create the first import job when it is online/);
  assert(controls().every(button => button.disabled));
  await fire(controls()[0], 'click');
  assert.equal(actionPosts, 0, 'even a stale programmatic click cannot submit without a job');
  const refresh = findNode(panel, n => n.localName === 'button' && textOf(n) === 'Refresh');
  fire(refresh, 'click');
  await settle();
  assert.match(textOf(panel), /Import metadata is unavailable. Controls are disabled/);
  assert(controls().every(button => button.disabled));
  await fire(controls()[2], 'click');
  assert.equal(actionPosts, 0);
  fire(refresh, 'click');
  await settle();
  assert.equal(controls()[0].disabled, false, 'Pause becomes available once the first run exists');
});

test('Members enables Retry when audit recovery pagination stalls after initial complete', async () => {
  const h = harness(async (url) => url.endsWith('/member-sync')
    ? response({
      status: 'complete',
      phase: 'complete',
      run_id: 'initial-done',
      progress_revision: 7,
      initial_completed_at: 1790000000000,
      audit_recovery_status: 'needs_attention',
      audit_recovery_reason: 'pagination_stalled',
    })
    : response({ members: [], next_cursor: null }));
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  const retry = findNode(panel, n => n.localName === 'button' && textOf(n) === 'Retry');
  assert.equal(retry.disabled, false);
  const reconcile = findNode(panel, n => n.localName === 'button' && textOf(n) === 'Reconcile from Discord');
  assert.equal(reconcile.disabled, false);
});

test('Members keeps Retry disabled after initial complete when audit recovery is healthy', async () => {
  const h = harness(async (url) => url.endsWith('/member-sync')
    ? response({
      status: 'complete',
      phase: 'complete',
      run_id: 'initial-done',
      progress_revision: 7,
      initial_completed_at: 1790000000000,
      audit_recovery_status: 'complete',
    })
    : response({ members: [], next_cursor: null }));
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  const retry = findNode(panel, n => n.localName === 'button' && textOf(n) === 'Retry');
  assert.equal(retry.disabled, true);
});

test('Members shows a live rate-limit countdown, separate progress time, and a prior successful run', async () => {
  const asOf = 1790000000000;
  const statuses = [
    { status: 'waiting', status_reason: 'rate_limited', estimate_reason: 'rate_limited',
      next_attempt_at: asOf + 90000, phase_remaining_seconds: 240, phase_finish_at: asOf + 240000,
      processed: 0, last_progress_at: asOf - 60000, as_of: asOf,
      last_success_at: asOf - 120000, last_success_processed: 11000, last_success_inserted: 10900,
      last_success_updated: 50, last_success_unchanged: 50 },
    { status: 'waiting', status_reason: 'discord_unavailable', estimate_reason: 'discord_unavailable',
      next_attempt_at: asOf + 90000, phase_remaining_seconds: 240, phase_finish_at: asOf + 240000,
      last_progress_at: asOf - 60000, as_of: asOf },
    { status: 'paused', estimate_reason: 'paused', phase_remaining_seconds: 240,
      phase_finish_at: asOf + 240000, as_of: asOf },
    { status: 'queued', estimate_reason: 'worker_offline', phase_remaining_seconds: 240,
      phase_finish_at: asOf + 240000, as_of: asOf },
  ];
  let reads = 0;
  const h = harness(async (url) => url.endsWith('/member-sync')
    ? response({ phase: 'members', progress_revision: reads + 1, ...statuses[reads++] })
    : response({ members: [], next_cursor: null }));
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  let copy = textOf(panel);
  assert.match(copy, /Rate limit: retry in 1m 30s/);
  assert.match(copy, /Remaining: about 4 minutes for this phase · Expected finish: (?!Unknown)/);
  assert.match(copy, /0 records committed in this run/);
  assert.match(copy, /Last successful run: .*11,000 processed · 10,900 inserted · 50 updated · 50 unchanged/);
  assert.match(copy, /Last committed progress:/);
  assert.match(copy, /Status read at/);
  assert.doesNotMatch(copy, /Status updated/);
  const countdown = [...h.timers].find(([, timer]) => timer.delay === 1000);
  assert(countdown, 'known rate limit schedules a visible countdown');
  h.advanceTime(1500);
  h.timers.delete(countdown[0]);
  countdown[1].fn();
  assert.match(textOf(panel), /Rate limit: retry in 1m 29s/);
  h.advanceTime(60000);
  const laterCountdown = [...h.timers].find(([, timer]) => timer.delay === 1000);
  assert(laterCountdown);
  h.timers.delete(laterCountdown[0]);
  laterCountdown[1].fn();
  assert.match(textOf(panel), /Remaining: about 3 minutes for this phase/);
  assert.match(textOf(panel), /Rate limit: retry in 0m 29s/);
  h.advanceTime(190000);
  const expiredCountdown = [...h.timers].find(([, timer]) => timer.delay === 1000);
  assert(expiredCountdown);
  h.timers.delete(expiredCountdown[0]);
  expiredCountdown[1].fn();
  assert.match(textOf(panel), /Remaining: Unknown · Expected finish: Unknown/);
  assert.match(textOf(panel), /Rate limit: retry due now/);
  const refresh = findNode(panel, n => n.localName === 'button' && textOf(n) === 'Refresh');
  for (const reason of ['discord_unavailable', 'paused', 'worker_offline']) {
    fire(refresh, 'click');
    await settle();
    copy = textOf(panel);
    assert.match(copy, /Remaining: Unknown · Expected finish: Unknown|Remaining: Unknown until worker activity is confirmed · Expected finish: Unknown/);
    assert.doesNotMatch(copy, /Rate limit: retry in/);
    assert.equal([...h.timers.values()].filter(timer => timer.delay === 1000).length, 0, reason);
  }
});

test('Members rejects a pending old search result before the debounce fires', async () => {
  const pending = [];
  const h = harness((url, opts) => url.endsWith('/member-sync')
    ? Promise.resolve(response({ status: 'complete', phase: 'complete' }))
    : new Promise(resolve => pending.push({ url, opts, resolve })));
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  assert.equal(pending.length, 1);
  const search = findNode(panel, n => n.localName === 'input' && n.getAttribute('aria-label') === 'Search members');
  search.value = 'New Query';
  fire(search, 'input');
  assert.equal(pending[0].opts.signal.aborted, true);
  pending[0].resolve(response({ members: [{ user_id: '1800882749273538599', display_name: 'Old Result' }] }));
  await settle();
  assert.doesNotMatch(textOf(panel), /Old Result/);
  const debounce = [...h.timers].find(([, timer]) => timer.delay === 300);
  assert(debounce);
  h.timers.delete(debounce[0]);
  debounce[1].fn();
  assert.equal(pending.length, 2);
  assert.match(pending[1].url, /q=New\+Query/);
  pending[1].resolve(response({ members: [{ user_id: '1800882749273538600', display_name: 'New Result' }] }));
  await settle();
  assert.match(textOf(panel), /New Result/);
  assert.doesNotMatch(textOf(panel), /Old Result/);
});

test('Members filter, date, and sort changes discard old history and list responses immediately', async () => {
  const pending = [];
  let listReads = 0;
  const h = harness((url, opts) => {
    if (url.endsWith('/member-sync')) return Promise.resolve(response({ status: 'complete', phase: 'complete' }));
    if (url.includes('/history')) return new Promise(resolve => pending.push({ type: 'history', url, opts, resolve }));
    if (listReads++ === 0) return Promise.resolve(response({ members: [{ user_id: '1800882749273538599', display_name: 'Willow' }] }));
    return new Promise(resolve => pending.push({ type: 'list', url, opts, resolve }));
  });
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'member-enrollment' };
  const panel = h.app.tabMembers();
  await settle();
  fire(findNode(panel, n => n.localName === 'button' && textOf(n) === 'View history'), 'click');
  const history = pending.find(item => item.type === 'history');
  assert(history);
  const filter = findNode(panel, n => n.localName === 'select' && n.getAttribute('aria-label') === 'Member status');
  filter.value = 'banned';
  fire(filter, 'change');
  assert.equal(history.opts.signal.aborted, true);
  history.resolve(response({ entries: [{ kind: 'ban', occurred_at_ms: 1790000000000, source_kind: 'discord_audit' }] }));
  await settle();
  assert.doesNotMatch(textOf(panel), /Discord audit action/);
  const oldList = pending.find(item => item.type === 'list');
  assert.match(oldList.url, /status=banned/);
  const date = findNode(panel, n => n.localName === 'input' && n.getAttribute('aria-label') === 'Joined from date, UTC');
  date.value = '2026-09-01';
  fire(date, 'input');
  assert.equal(oldList.opts.signal.aborted, true);
  oldList.resolve(response({ members: [{ user_id: '1800882749273538599', display_name: 'Wrong Filter' }] }));
  await settle();
  assert.doesNotMatch(textOf(panel), /Wrong Filter/);
  const sort = findNode(panel, n => n.localName === 'select' && n.getAttribute('aria-label') === 'Join date order');
  sort.value = 'joined_oldest';
  fire(sort, 'change');
  assert.equal([...h.timers.values()].filter(timer => timer.delay === 300).length, 0);
  const newList = pending.filter(item => item.type === 'list').at(-1);
  assert.match(newList.url, /joined_from=2026-09-01/);
  assert.match(newList.url, /sort=joined_oldest/);
  newList.resolve(response({ members: [{ user_id: '1800882749273538601', display_name: 'Right Filter' }] }));
  await settle();
  assert.match(textOf(panel), /Right Filter/);
});

test('Members drops responses from a previous server and page enrollment', async () => {
  const pending = [];
  const h = harness((url) => new Promise(resolve => pending.push({ url, resolve })));
  h.app.state.tab = 'members';
  h.app.state.config = { enrollment_id: 'old-enrollment' };
  const panel = h.app.tabMembers();
  h.app.state.config = { enrollment_id: 'replacement-enrollment' };
  pending.find(item => item.url.endsWith('/member-sync')).resolve(response({ status: 'complete', phase: 'complete', progress_revision: 100, processed: 1 }));
  pending.find(item => item.url.includes('/members?')).resolve(response({ members: [{ user_id: '1800882749273538599', display_name: 'Wrong server' }] }));
  await settle();
  assert.doesNotMatch(textOf(panel), /Wrong server/);
  assert.doesNotMatch(textOf(panel), /Initial member import/);
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
  const pending = app.handleEvent(run, {});
  assert.equal(app.isBusy(), true);
  await app.handleEvent(run, {});
  assert.equal(calls, 1);
  reject(new Error('save refused'));
  await pending;
  assert.equal(app.isBusy(), false);
  await settle();
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
  const inputs = h.elements.filter(n => n.localName === 'input');
  const save = h.elements.find(n => n.localName === 'button' && n.textContent === 'Save');
  return { ...h, calls, inputs, save };
}

test('settings patches send only changed fields to the selected server', async () => {
  const { calls, inputs, save } = settingsHarness();
  inputs[0].value = '?';
  await fire(save, 'click', {});
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
  await settle();

  const roleInput = h.elements.find(n => n.localName === 'input' && /role id/i.test(n.getAttribute("placeholder") || ''));
  const saveRoles = h.elements.find(n => n.localName === 'button' && n.textContent === 'Save case-removal roles');
  assert.equal(Object.keys(h.app.currentDrafts()).length, 0, 'server-loaded roles must become the saved baseline');
  roleInput.value = '1, 2';
  await fire(roleInput, 'input');
  await fire(saveRoles, 'click', {});
  assert.equal(h.app.isDirty(), false, 'an acknowledged role save must not remain unsaved');
  const prefix = h.elements.find(n => n.localName === 'input' && n.value === '!');
  prefix.value = '?';
  await fire(prefix, 'input');
  roleInput.value = '2';
  await fire(roleInput, 'input');
  await fire(saveRoles, 'click', {});
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
  const input = h.elements.find((node) => node.localName === 'input' && node.getAttribute("placeholder") === 'channel id');
  const set = h.elements.find((node) => node.localName === 'button' && node.textContent === 'Set');
  input.value = '222';
  h.app.markDirty();
  await fire(set, 'click', {});
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
  const fields = h.elements.filter((node) => node.localName === 'textarea');
  fields[0].value = '9007199254740993\n<@9007199254740995>';
  fields[1].value = '<#9007199254740997>';
  const save = h.elements.find((node) => node.localName === 'button' && node.textContent === 'Save ignores');
  await fire(save, 'click', {});
  assert.match(calls[0].url, /\/logfilters\/message_edit$/);
  assert.deepEqual(JSON.parse(calls[0].opts.body), { users: ['9007199254740993', '9007199254740995'], channels: ['9007199254740997'] });
  assert.deepEqual(JSON.parse(JSON.stringify(h.app.state.config.log_channels[0].ignore)), { users: ['9007199254740993', '9007199254740995'], channels: ['9007199254740997'] });
  fields[0].value = 'OtherBot';
  await fire(save, 'click', {});
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
  const inputs = Array.from(h.nodes.get('#content').querySelectorAll('input, select, textarea')).filter((node) => node.getAttribute("placeholder") === 'channel id');
  inputs[1].value = '555';
  await fire(inputs[1], 'input');

  await h.app.reconcileQueuedLogChannel(pending);
  await settle();

  const renderedInputs = Array.from(h.nodes.get('#content').querySelectorAll('input, select, textarea')).filter((node) => node.getAttribute("placeholder") === 'channel id');
  assert.equal(renderedInputs[1].value, '555');
  assert.equal(h.app.isDirty(), true);
  assert.equal(findNode(h.nodes.get('#content'), (node) => node.localName === 'span' && node.textContent === 'queued'), null);
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

for (const asyncRoot of [false, true]) test(`terminal request-key replay restores authoritative mapping (async React: ${asyncRoot})`, async () => {
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
  }, new Map(), new Map(), () => true, null, asyncRoot);
  appRef = h.app;
  appRef.state.config = {
    enrollment_id: 'membership', rules: [], commands: [], badwords: [],
    settings: { prefix: '!', bot_name: '' },
    log_channels: [{ event: 'member_join', effective: '111', guild_override: '111', global: null }],
  };
  appRef.state.tab = 'logchannels';
  appRef.renderTab();
  await settle();
  const input = Array.from(h.nodes.get('#content').querySelectorAll('input, select, textarea')).find((node) => node.getAttribute("placeholder") === 'channel id');
  input.value = '222';
  appRef.markDirty();
  const set = findNode(h.nodes.get('#content'), (node) => node.localName === 'button' && node.textContent === 'Set');
  await fire(set, 'click', {});
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
  let status = findNode(h.nodes.get('#content'), (node) => node.localName === 'span' && String(node.className).split(/\s+/).includes('badge'));
  assert.equal(status.textContent, 'failed');

  await h.app.reconcileQueuedLogChannel(terminal);
  assert.equal(h.app.state.config.log_channels[0].pending_action.state, 'failed');
  status = findNode(h.nodes.get('#content'), (node) => node.localName === 'span' && String(node.className).split(/\s+/).includes('badge'));
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
  await settle();

  const roleInput = h.elements.find(n => n.localName === 'input' && /role id/i.test(n.getAttribute("placeholder") || ''));
  const saveRoles = h.elements.find(n => n.localName === 'button' && n.textContent === 'Save case-removal roles');
  roleInput.value = 'moderators';
  await fire(saveRoles, 'click', {});
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

  const roleInput = h.elements.find(n => n.localName === 'input' && /role id/i.test(n.getAttribute("placeholder") || ''));
  const saveRoles = h.elements.find(n => n.localName === 'button' && n.textContent === 'Save case-removal roles');
  roleInput.value = '2';
  await fire(roleInput, 'input');
  await fire(saveRoles, 'click', {});

  resolveInitialRoles(response({ roles: ['1'], revision: 'revision-1' }));
  await settle();
  assert.equal(roleInput.value, '2');

  await fire(saveRoles, 'click', {});
  const puts = calls.filter(c => c.method === 'PUT' && c.url.endsWith('/case-delete-roles'));
  assert.equal(puts.length, 2);
  assert.equal(puts[1].body.revision, 'revision-2');
});

test('settings keep clear-to-inherit and empty nickname distinct', async () => {
  const { calls, inputs, save } = settingsHarness();
  inputs[0].value = '';
  inputs[1].value = '';
  await fire(save, 'click', {});
  const put = calls.find((c) => c.method === 'PUT' && c.url.endsWith('/settings'));
  assert.deepEqual(put.body, { prefix: null, bot_name: '' });
});

test('settings do not expose historical chat-retention controls', () => {
  const { elements, inputs } = settingsHarness();
  assert.equal(inputs.filter(n => n.getAttribute("type") === 'number').length, 0);
  assert.ok(!elements.some(n => /retention|keep forever/i.test(n.textContent || '')));
});

test('partial settings actions preserve dirty sibling fields in the same card', async () => {
  const h = settingsHarness();
  h.app.renderTab();
  const prefix = h.document.querySelector('input');
  const nickname = h.document.querySelectorAll('input')[1];
  prefix.value = '?'; await fire(prefix, 'input');
  nickname.value = 'Draft nickname'; await fire(nickname, 'input');
  const inherit = h.elements.find(n => n.localName === 'button' && n.textContent === 'Use global');
  await fire(inherit, 'click');
  assert.equal(h.document.querySelectorAll('input')[1].value, 'Draft nickname');
  assert.equal(h.document.querySelector('input').value, '/');
  assert.equal(h.app.isDirty(), true);
});

function templateDraftHarness() {
  let fail = true;
  let membership = 'original';
  const config = () => ({ enrollment_id: membership, templates: [
    { event: 'join', name: 'Join', effective: 'Saved join', global: 'Saved join' },
    { event: 'leave', name: 'Leave', effective: 'Saved leave', global: 'Saved leave' },
  ] });
  const h = harness(async (url, options) => {
    if (url.endsWith('/config') && fail) throw new Error('offline');
    if (url.endsWith('/config')) return response(config());
    if (options.method === 'PUT') return response({});
    return response({});
  });
  h.app.state.config = config(); h.app.state.tab = 'templates'; h.app.renderTab();
  return { ...h, recover(next = 'original') { fail = false; membership = next; } };
}

test('unrelated drafts survive the unavailable screen and restore after Retry', async () => {
  const h = templateDraftHarness();
  const fields = h.document.querySelectorAll('textarea');
  fields[1].value = 'Unsaved leave'; await fire(fields[1], 'input');
  await fire(h.elements.find(n => n.localName === 'button' && n.textContent === 'Save'), 'click');
  assert.match(h.document.querySelector('#content').textContent, /Configuration unavailable/);
  assert.equal(h.app.isDirty(), true);
  await fire(h.elements.find(n => n.localName === 'button' && n.textContent === 'Retry'), 'click');
  assert.match(h.document.querySelector('#content').textContent, /Configuration unavailable/);
  h.recover();
  await fire(h.elements.find(n => n.localName === 'button' && n.textContent === 'Retry'), 'click');
  assert.equal(h.document.querySelectorAll('textarea')[1].value, 'Unsaved leave');
  assert.equal(h.app.isDirty(), true);
});

test('confirmed navigation clears pending drafts and prevents later replay', async () => {
  const h = templateDraftHarness();
  const draft = h.document.querySelectorAll('textarea')[1];
  draft.value = 'Unsaved leave'; await fire(draft, 'input');
  await fire(h.elements.find(n => n.localName === 'button' && n.textContent === 'Save'), 'click');
  h.app.navigate('templates');
  h.recover(); await h.app.loadConfig();
  h.app.navigate('templates');
  assert.equal(h.document.querySelectorAll('textarea')[1].value, 'Saved leave');
  assert.equal(h.app.isDirty(), false);
});

for (const kind of ['rule', 'command']) {
  for (const action of ['add', 'remove']) {
    test(`${kind} structural ${action} marks the editor dirty`, async () => {
      const { app, elements } = harness();
      if (kind === 'rule') await app.editRule({ name: 'Flood', rule_id: 'flood', effective: { params: {}, actions: [{ at: 1, action: 'warn' }] } });
      else await app.editCommand(null);
      assert.equal(app.isDirty(), false, 'opening the editor does not modify it');
      const panel = elements.find(n => n.classList.contains('modal-box'));
      assert.equal(panel?.matches('.modal-open > .modal-box'), true,
        'daisyUI only reveals editor panels that are direct children of the open modal');
      const button = action === 'add'
        ? elements.find(n => n.localName === 'button' && (n.textContent === '+ add step' || n.textContent === '+ add action'))
        : elements.find(n => n.localName === 'button' && (n.getAttribute('aria-label') === 'Remove action 1' || n.textContent === '✕' || Array.from(n.children).some(c => c.textContent === '✕')));
      assert.ok(button);
      await fire(button, 'click', {});
      assert.equal(app.isDirty(), true);
    });
  }
}

async function ruleSaveHarness({ scope = 'global', override = false } = {}) {
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
  await h.app.editRule(h.app.state.config.rules[0]);
  const content = h.nodes.get('#content');
  const modal = h.document.querySelector('.modal-overlay');
  const scopeInput = findNode(modal, (n) => n.localName === 'select' && Array.from(n.children).some((c) => c.getAttribute("value") === 'global'));
  scopeInput.value = scope;
  const enabled = findNode(modal, (n) => n.getAttribute("type") === 'checkbox');
  const save = findNode(modal, (n) => n.localName === 'button' && n.textContent === 'Save');
  const cancel = findNode(modal, (n) => n.textContent === 'Cancel');
  return { ...h, calls, failures, content, modal, enabled, save, cancel, snapshot };
}

test('global rule partial success reconciles the committed value before navigation', async () => {
  const h = await ruleSaveHarness();
  h.failures.delete = true;
  await fire(h.enabled, 'change', { target: { checked: false } });
  await fire(h.save, 'click', {});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'POST']);
  assert.equal(h.snapshot().rules[0].effective.enabled, false);
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
  assert.match(h.nodes.get('#toast').textContent, /Global rule saved/);
  assert.match(h.nodes.get('#toast').textContent, /cleanup unavailable/);
  assert.match(h.nodes.get('#toast').className, /error/);
  await fire(h.cancel, 'click', {});
  h.app.navigate('rules');
  assert.match(textOf(h.content), /disabled/);
  assert.equal(h.app.isBusy(), false);
});

test('global rule partial success with failed reconciliation stays unavailable through Retry', async () => {
  const h = await ruleSaveHarness();
  h.failures.delete = h.failures.refresh = true;
  await fire(h.enabled, 'change', { target: { checked: false } });
  await fire(h.save, 'click', {});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'POST']);
  assert.equal(h.app.state.config, null);
  assert.match(textOf(h.content), /Configuration unavailable/);
  assert.match(h.nodes.get('#toast').textContent, /Global rule saved/);
  assert.match(h.nodes.get('#toast').textContent, /cleanup unavailable/);
  assert.match(h.nodes.get('#toast').textContent, /Retry/);
  assert.equal(h.app.isBusy(), false);
  assert.equal(h.modal.isConnected, false);
  assertAccessible(findNode(h.content, n => n.textContent === 'Retry'));
  h.failures.refresh = false;
  const retry = findNode(h.content, (n) => n.textContent === 'Retry');
  await fire(retry, 'click', {});
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
  assert.match(textOf(h.content), /disabled/);
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE', 'POST', 'POST']);
});

test('a refused rule PUT retains the checkbox draft and never deletes an override', async () => {
  const h = await ruleSaveHarness();
  h.failures.put = true;
  await fire(h.enabled, 'change', { target: { checked: false } });
  await fire(h.save, 'click', {});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT']);
  assert.equal(h.modal.isConnected, true);
  assert.match(h.nodes.get('#toast').textContent, /write refused/);
  assertAccessible(h.nodes.get('#toast'));
  assert.equal(h.app.state.config.rules[0].effective.enabled, true);
  assert.equal(h.app.isBusy(), false);
  h.failures.put = false;
  await fire(h.save, 'click', {});
  assert.equal(h.calls.filter((c) => c.method === 'PUT')[1].body.data.enabled, false);
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
});

test('a successful global rule save applies both acknowledgments without a config reload', async () => {
  const h = await ruleSaveHarness({ override: true });
  await fire(h.enabled, 'change', { target: { checked: false } });
  await fire(h.save, 'click', {});
  assert.deepEqual(h.calls.map((c) => c.method), ['PUT', 'DELETE']);
  assert.equal(h.calls[0].body.scope, 'global');
  assert.equal(h.calls[1].body.scope, 'guild');
  assert.equal(h.app.state.config.rules[0].guild_override, null);
  assert.equal(h.app.state.config.rules[0].effective.enabled, false);
  assert.equal(h.modal.isConnected, false);
  assert.equal(h.nodes.get('#toast').textContent, 'Rule saved');
});

test('a guild-only rule save does not run global override cleanup', async () => {
  const h = await ruleSaveHarness({ scope: 'guild' });
  await fire(h.enabled, 'change', { target: { checked: false } });
  await fire(h.save, 'click', {});
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
  await h.app.editCommand(null);
  const content = h.nodes.get('#content');
  const modal = h.document.querySelector('.modal-overlay');
  const name = findNode(modal, (n) => n.getAttribute("placeholder") === 'command name');
  name.value = 'hello';
  const enabled = findNode(modal, (n) => n.getAttribute("type") === 'checkbox');
  const save = findNode(modal, (n) => n.localName === 'button' && n.textContent === 'Save');
  await fire(enabled, 'change', { target: { checked: false } });
  await fire(save, 'click', {});
  assert.equal(modal.isConnected, true);
  assert.equal(h.app.isBusy(), false);
  refuse = false;
  await fire(save, 'click', {});
  assert.equal(calls.length, 2);
  assert.equal(calls[0].data.enabled, false);
  assert.equal(calls[1].data.enabled, false);
  assert.equal(modal.isConnected, false);
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

test('drafts survive retries only within the captured bot membership', async () => {
  for (const next of ['original', 'replacement']) {
    const h = templateDraftHarness();
    const draft = h.document.querySelectorAll('textarea')[1];
    draft.value = 'Old draft'; await fire(draft, 'input');
    await fire(h.elements.find(n => n.localName === 'button' && n.textContent === 'Save'), 'click');
    h.recover(next);
    await fire(h.elements.find(n => n.localName === 'button' && n.textContent === 'Retry'), 'click');
    assert.equal(h.document.querySelectorAll('textarea')[1].value, next === 'original' ? 'Old draft' : 'Saved leave');
    assert.equal(h.app.isDirty(), next === 'original');
  }
});

test('an async action cannot restore drafts into a replacement membership', async () => {
  const h = templateDraftHarness();
  const input = h.document.querySelectorAll('textarea')[1];
  input.value = 'Old draft'; await fire(input, 'input');
  await h.app.handleEvent(async () => {
    h.app.state.config = { ...h.app.state.config, enrollment_id: 'replacement' };
    h.app.renderTab();
  }, {});
  await settle();
  assert.equal(h.document.querySelectorAll('textarea')[1].value, 'Saved leave');
  assert.equal(h.app.isDirty(), false);
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
  app.renderTab();
  const source = elements.find(n => n.localName === 'select' && Array.from(n.options).some(option => option.textContent === 'Select a source server'));
  let copy = elements.find(n => n.localName === 'button' && n.textContent === 'Copy');
  source.value = app.state.guilds[1].id;
  await fire(copy, 'click', {});
  assert.equal(calls.filter((c) => c.url.endsWith('/copy')).length, 0, 'copy waits for an explicitly loaded source');
  await chooseOption(source, 'Second');
  assert.equal(calls.filter((c) => c.url.includes(app.state.guilds[1].id)).length, 1);
  const section = elements.find(n => n.localName === 'select' && Array.from(n.options).some(option => option.value === 'all'));
  section.value = 'settings'; await fire(section, 'change');
  const prefix = elements.find(n => n.localName === 'input' && n.value === '!');
  prefix.value = '?'; await fire(prefix, 'input');
  await fire(elements.find(n => n.localName === 'button' && n.textContent === 'Save'), 'click');
  const restoredSource = elements.find(n => n.localName === 'select' && Array.from(n.options).some(option => option.textContent === 'Select a source server'));
  const restoredSection = elements.find(n => n.localName === 'select' && Array.from(n.options).some(option => option.value === 'all'));
  assert.equal(restoredSource.value, app.state.guilds[1].id);
  assert.equal(restoredSection.value, 'settings', 'unrelated save must not broaden the selected copy scope');
  copy = elements.find(n => n.localName === 'button' && n.textContent === 'Copy');
  await fire(copy, 'click', {});
  const sent = calls.find(c => c.opts.method === 'POST' && c.url.endsWith('/copy'));
  assert.equal(JSON.parse(sent.opts.body).from_enrollment_id, 'selected-source');
  assert.deepEqual(JSON.parse(sent.opts.body).sections, ['settings']);
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
  assert.match(textOf(h.elements.find(n=>n.localName==='p')),/Force-cancel a stuck log-channel request/);
  const cancel=h.elements.find(n=>n.localName==='button'&&n.textContent==='Cancel');
  await fire(cancel, 'click', {});
  assert.equal(calls[1].opts.method,'DELETE');
  assert.equal(calls[1].opts.headers['x-jmodguard-enrollment'],'membership');
  assert.equal(calls[2].opts.method,'POST');
  const refresh=h.elements.find(n=>n.localName==='button'&&n.textContent==='Refresh');
  await fire(refresh, 'click', {});
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
  const controls = h.elements.filter((n) => n.localName === 'button' && (n.textContent === 'Cancel' || n.textContent === 'Force cancel'));
  assert.deepEqual(controls.map((n) => n.textContent), ['Cancel', 'Force cancel', 'Force cancel']);
  await fire(controls[0], 'click', {});
  assert.equal(confirms, 0);
  assert.equal(calls[1].opts.method, 'DELETE');
  assert.match(calls[1].url, /\/actions\/job$/);
  await fire(controls[1], 'click', {});
  assert.equal(confirms, 1);
  assert.equal(calls[3].opts.method, 'DELETE');
  assert.match(calls[3].url, /\/actions\/log-run$/);
});

test('moderation and member reads send the loaded page membership including query-string routes', async () => {
  const calls = [];
  const h = harness(async (url, opts) => { calls.push({url,opts}); return response({}); });
  h.app.state.config = { enrollment_id: 'page-membership' };
  for (const suffix of ['events?event=member_ban&user=123', 'bans?limit=100', 'stats', 'cases', 'cases/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'case-delete-roles', 'members?q=willow', 'members/1800882749273538599/history', 'member-sync']) {
    await h.app.api(`/api/guilds/${h.app.state.guildId}/${suffix}`);
    assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], 'page-membership');
  }
  await h.app.api('/api/guilds/987654321098765433/stats');
  assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], undefined);
  h.app.state.config = null;
  await h.app.api(`/api/guilds/${h.app.state.guildId}/stats`);
  assert.equal(calls.at(-1).opts.headers['x-jmodguard-enrollment'], undefined);
  assert.equal(calls.length, 11, 'reads must not silently load a new membership');
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
  const modal=await h.app.editRule(legacy);
  assert.doesNotMatch(textOf(modal), /Action escalation/);
  const dryRun = findNode(modal,n=>n.localName==='label' && textOf(n).includes('Dry-run (log only)'));
  assert.equal(findNode(dryRun,n=>n.localName==='input').checked,true);
  const save=findNode(modal,n=>n.textContent === 'Save');
  await fire(save, 'click', {});
  assert.equal(calls.length,1);
  assert.equal(calls[0].data.dry_run,true);
  assert.deepEqual(calls[0].data.params,{mode:'exact_image_test',min_channels:2,min_attachments:2,window_secs:60});
  assert.deepEqual(calls[0].data.actions.map(a=>a.action),['log']);
  assert.equal(h.app.spamConfigured('attachment_flood', calls[0].data),true);
  assert.match(textOf(h.app.ruleCard({name:'Images',rule_id:'attachment_flood',effective:calls[0].data})), /dry-run/);
  assert.doesNotMatch(textOf(h.app.ruleCard({name:'Images',rule_id:'attachment_flood',effective:calls[0].data})), /log-only test/);
  calls[0].data.params.min_channels=1;
  assert.equal(h.app.spamConfigured('attachment_flood', calls[0].data),false);
  const punish=await h.app.editRule({name:'Images',rule_id:'attachment_flood',effective:{enabled:true,dry_run:true,params:{mode:'exact_image_test',min_channels:3,min_attachments:2,window_secs:60},actions:[{action:'log'}]}});
  const dryRunToggle = findNode(punish,n=>n.localName==='label' && textOf(n).includes('Dry-run (log only)'));
  findNode(dryRunToggle,n=>n.localName==='input' && n.getAttribute("type")==='checkbox').checked = false;
  await fire(findNode(punish,n=>n.textContent === 'Save'), 'click', {});
  assert.equal(calls.at(-1).data.dry_run,false);
  assert.equal(calls.at(-1).data.params.mode,'exact_image');
  assert.deepEqual(calls.at(-1).data.actions.map(a=>a.action),['delete','ban']);
  const back=await h.app.editRule({name:'Images',rule_id:'attachment_flood',effective:calls.at(-1).data});
  const backToggle = findNode(back,n=>n.localName==='label' && textOf(n).includes('Dry-run (log only)'));
  assert.equal(findNode(backToggle,n=>n.localName==='input').checked,false);
  findNode(backToggle,n=>n.localName==='input').checked=true;
  await fire(findNode(back,n=>n.textContent==='Save'), 'click', {});
  assert.equal(calls.at(-1).data.dry_run,true);
  assert.equal(calls.at(-1).data.params.mode,'exact_image_test');
  assert.deepEqual(calls.at(-1).data.actions.map(a=>a.action),['log']);
});

test('checkbox labels keep primary text color', () => {
  const css = fs.readFileSync(new URL('../frontend/style.css', import.meta.url), 'utf8');
  assert.match(css, /\.checkbox-label\s*\{[^}]*color:\s*var\(--text\)/);
});

test('text rule UI can save independent thresholds without generic escalation', async () => {
  const calls=[];
  const h=harness(async (url, options) => {
    if (options.method === 'PUT') calls.push(JSON.parse(options.body));
    return response({ settings:{}, rules:[], commands:[], log_channels:[], badwords:[] });
  });
  let modal=await h.app.editRule({name:'Text',rule_id:'repeat_text',effective:{enabled:true,dry_run:true,params:{mode:'exact_text',min_channels:3,window_secs:60,min_length:20},actions:[{action:'log'}]}});
  assert.doesNotMatch(textOf(modal), /Action escalation/);
  assert.match(textOf(modal), /Minimum length/);
  await fire(findNode(modal,n=>n.textContent === 'Save'), 'click', {});
  assert.equal(calls[0].data.params.mode,'exact_text');
  assert.equal(calls[0].data.params.min_length,20);
  assert.equal(h.app.spamConfigured('repeat_text', calls[0].data),true);
  assert.match(textOf(h.app.ruleCard({name:'Text',rule_id:'repeat_text',effective:calls[0].data})), /dry-run/);
  modal = await h.app.editRule({name:'Text',rule_id:'repeat_text',effective:calls.at(-1).data});
  const toggle=findNode(modal,n=>n.localName==='label' && textOf(n).includes('Dry-run (log only)'));
  findNode(toggle,n=>n.localName==='input').checked=false;
  await fire(findNode(modal,n=>n.textContent==='Save'), 'click', {});
  assert.equal(calls.at(-1).data.dry_run,false);
  assert.deepEqual(calls.at(-1).data.actions.map(a=>a.action),['delete','ban']);
  const back=await h.app.editRule({name:'Text',rule_id:'repeat_text',effective:calls.at(-1).data});
  const backToggle=findNode(back,n=>n.localName==='label' && textOf(n).includes('Dry-run (log only)'));
  assert.equal(findNode(backToggle,n=>n.localName==='input').checked,false);
  findNode(backToggle,n=>n.localName==='input').checked=true;
  await fire(findNode(back,n=>n.textContent==='Save'), 'click', {});
  assert.equal(calls.at(-1).data.dry_run,true);
  assert.deepEqual(calls.at(-1).data.actions.map(a=>a.action),['log']);
});

test('image test UI rejects invalid thresholds before the API request', async () => {
  let sent=0;
  const h=harness(async()=>{sent++;return response({});});
  const modal=await h.app.editRule({name:'Images',rule_id:'attachment_flood',effective:{enabled:true,params:{min_channels:1},actions:[]}});
  await fire(findNode(modal,n=>n.textContent === 'Save'), 'click', {});
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
  await settle();
  const next = findNode(root, n => n.localName === 'button' && n.textContent === 'Next page');
  assert.equal(next.disabled, false);
  fire(next, 'click');
  await settle();
  assert.ok(calls.some(url => url.includes(`after=${ids[49]}`)));
  const caseButton = findNode(root, n => n.localName === 'button' && n.textContent === ids[50].slice(0, 8));
  fire(caseButton, 'click');
  await settle();
  assert.ok(textOf(root).includes('note 33'));
  assert.ok(textOf(root).includes('note 64'));
  const excerpt = findNode(root, n => n.localName === 'pre');
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
  await settle();
  fire(findNode(root, n => n.textContent === 'Removed records / retry cleanup'), 'click');
  await settle();
  assert.ok(textOf(root).includes(id));
  await fire(findNode(root, n => n.localName === 'button' && n.textContent === 'Retry cleanup'), 'click');
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
  await settle();
  fire(findNode(root, n => n.localName === 'button' && n.textContent === id.slice(0,8)), 'click');
  await settle();
  const note = findNode(root, n => n.localName === 'textarea');
  const submit = findNode(root, n => n.localName === 'button' && n.textContent === 'Add note');
  assert.ok(note, textOf(root));
  note.value = 'same note';
  const first = fire(submit, 'click');
  await fire(submit, 'click');
  assert.equal(posts.length,1);
  resolveFirst(); await first; await settle();
  await fire(submit, 'click');
  assert.deepEqual(posts[1],posts[0]);
  note.value = 'different note';
  await fire(submit, 'click');
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
  await settle();
  fire(findNode(root, n => n.localName === 'button' && n.textContent === id.slice(0, 8)), 'click');
  await settle();
  const note = findNode(root, n => n.localName === 'textarea');
  note.value = 'saved note';
  h.app.markDirty();
  const submit = findNode(root, n => n.localName === 'button' && n.textContent === 'Add note');
  await fire(submit, 'click', {});
  assert.equal(h.app.isDirty(), false);
  assert.equal(findNode(root, n => n.localName === 'textarea').value, '', 'acknowledged note must not remain as a fresh draft');
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
  await fire(button, 'click');
  assert.equal(h.opened[0][0], '/#preview=new-preview-token');
  assert.equal(h.opened[0][1], '_blank');
  assert.equal(h.opened[0][2], 'noopener');
});

test('only the owner sees the same-origin Administration link', () => {
  const h = harness();
  h.app.state.me.is_bot_owner = false;
  h.app.bindOwnerTools();
  assert.equal(findNode(h.nodes.get('#owner-tools'), n => n.textContent === 'Administration'), null);
  h.app.state.me.is_bot_owner = true;
  h.app.bindOwnerTools();
  const link = findNode(h.nodes.get('#owner-tools'), n => n.textContent === 'Administration');
  assert.equal(link.getAttribute('href'), '/administration/index.html');
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
  assert.equal(h.nodes.get('#login-view'), null);
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
  assert.equal(h.nodes.get('#login-view'), null);
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
  assert.equal(h.nodes.get('#login-view'), null);
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
    assert.equal(h.nodes.get('#login-view'), null);
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
  h.app.tabSettings();
  const inputs = h.elements.filter(n => n.localName === 'input');
  const save = h.elements.find(n => n.localName === 'button' && n.textContent === 'Save');
  inputs[0].value = '?';
  await fire(save, 'click', {});
  assert.equal(h.nodes.get('#login-view').classList.contains('hidden'), false);
});


test('React WebUI retains the framework imports, styling and pale-blue tokens', () => {
  const html = fs.readFileSync(new URL('../frontend/index.html', import.meta.url), 'utf8');
  const js = fs.readFileSync(new URL('../frontend/app.js', import.meta.url), 'utf8');
  const css = fs.readFileSync(new URL('../frontend/style.css', import.meta.url), 'utf8');
  assert.doesNotMatch(html, /https?:\/\//);
  assert.doesNotMatch(js, /esm\.sh|cdn\.jsdelivr\.net|JMODGUARD_ESM_BASE|JMODGUARD_DAISYUI_HREF/);
  assert.match(js, /from "react"/);
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
  assert.match(css, /--color-base-100:\s*var\(--panel\)/);
  assert.match(js, /hero-content/);
  assert.match(js, /stat-title/);
  assert.match(js, /btn-primary/);
  assert.match(js, /dashboard-hero/);
  assert.doesNotMatch(js, /btn-neutral/);
  assert.match(js, /className: "stats /);
  assert.match(js, /btn btn-primary/);
  assert.match(js, /className: "navbar /);
  assert.match(css, /\.workspace-header\s*\{[^}]*justify-content:\s*space-between/);
  assert.match(js, /sidebar-footer[\s\S]*Appearance[\s\S]*logout-btn/);
  assert.match(js, /className: "menu w-full"/);
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
  const err = Object.assign(new Error('not authenticated'), { status: 400 });
  assert.equal(h.app.handleAuthLoss(err), true);
  await settle();
  assert.equal(h.nodes.get('#login-view').classList.contains('hidden'), false);
});

test('rule badges separate status from heading and preserve distinct semantic variants', () => {
  const { app } = harness();
  for (const [enabled, dry_run, variant] of [[true, false, 'badge-success'], [true, true, 'badge-warning'], [false, false, 'badge-error']]) {
    const card = app.ruleCard({ name: 'Mention spam', rule_id: 'mention_spam', global: {}, guild_override: {}, effective: { enabled, dry_run } });
    assert.equal(textOf(findNode(card, n => n.localName === 'h3')), 'Mention spam');
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
  const modal = await h.app.editRule({ name: 'Mention spam', rule_id: 'mention_spam', effective: { enabled: true, dry_run: true, params: {}, actions: [{ at: 1, action: 'warn', duration_minutes: 0, delete_days: 1, reason: 'spam' }] } });
  await chooseOption(modal.querySelector('.escalation-step select'), 'ban');
  for (const [label, value] of [['Strike count', '3'], ['Timeout (minutes)', '15'], ['Delete history (days)', '2'], ['Reason template', 'Updated reason']]) {
    const wrapper = findNode(modal, n => n.localName === 'label' && textOf(n).startsWith(label));
    assert.ok(wrapper, label);
    const input = wrapper.control;
    assert.equal(input?.localName, 'input');
    input.value = value;
    await fire(input, 'change');
  }
  await fire(findNode(modal, n => n.localName === 'button' && n.textContent === 'Save'), 'click', {});
  assert.equal(submitted.data.actions[0].at, 3);
  assert.equal(submitted.data.actions[0].action, 'ban');
  assert.equal(submitted.data.actions[0].duration_minutes, 15);
  assert.equal(submitted.data.actions[0].delete_days, 2);
  assert.equal(submitted.data.actions[0].reason, 'Updated reason');
});

async function chooseOption(select, label) {
  const option = Array.from(select.options).find(node => node.textContent === label);
  assert.ok(option, label);
  select.value = option.value;
  await fire(select, 'input');
  await fire(select, 'change');
}

test('changing only a rule action through Headless UI marks unsaved edits', async () => {
  const h = harness();
  const modal = await h.app.editRule({name: 'Mentions', rule_id: 'mention_spam', effective: {enabled: true, dry_run: true, params: {}, actions: [{at: 1, action: 'warn'}]}});
  assert.equal(h.app.isDirty(), false);
  await chooseOption(modal.querySelector('.escalation-step select'), 'ban');
  assert.equal(modal.querySelector('.escalation-step select').value, 'ban');
  assert.equal(h.app.isDirty(), true);
});

test('activity-log events from the config payload render template and log-channel cards', async () => {
  const h = harness();
  h.app.state.config = {
    templates: [
      { event: 'voice_join', effective: '**:loud_sound: {user} joined voice**\n\n**Channel:** {channel}\n**Time:** `{time}`' },
      { event: 'member_role_update', effective: '**:label: {user} roles updated**\n\n**Added:** {added}\n**Removed:** {removed}' },
    ],
    log_channels: [
      { event: 'voice_join', effective: '0', guild_override: null, global: null, ignore: { users: [], channels: [] } },
      { event: 'thread_create', effective: '222', guild_override: '222', global: null, ignore: { users: ['42'], channels: [] } },
    ],
  };
  h.app.navigate('templates');
  await settle();
  const reference = h.document.querySelector('.reference-panel');
  assert.match(reference.textContent, /\{added\}/);
  assert.match(reference.textContent, /do not supply message text/);
  const cards = [...h.document.querySelectorAll('.template-card')];
  assert.equal(cards.length, 2);
  assert.match(cards[0].textContent, /Voice join/);
  assert.match(cards[0].textContent, /voice_join/);
  assert.match(cards[0].querySelector('pre').textContent, /Alice/);
  assert.match(cards[0].querySelector('pre').textContent, /#general/);
  assert.match(cards[1].textContent, /Member role update/);
  assert.match(cards[1].querySelector('pre').textContent, /\{added\}/);
  h.app.navigate('logchannels');
  await settle();
  const logs = [...h.document.querySelectorAll('.log-channel-card')];
  assert.equal(logs.length, 2);
  assert.match(logs[0].textContent, /Voice join/);
  assert.equal(logs[0].querySelector('input').value, '');
  assert.match(logs[1].textContent, /Thread create/);
  assert.equal(logs[1].querySelector('input').value, '222');
  assert.match(logs[1].querySelector('textarea').value, /^42$/);
});

test('native server selection restores the selected server and draft when navigation is cancelled', async () => {
  const calls = [];
  const h = harness(async url => { calls.push(url); return response({}); }, new Map(), new Map(), () => false);
  h.app.state.config = { templates: [{event: 'member_join', effective: 'Welcome'}] };
  h.app.navigate('templates');
  await settle();
  const draft = h.document.querySelector('.template-card textarea');
  draft.value = 'Unsaved template';
  await fire(draft, 'input');
  const select = h.document.querySelector('#guild-select');
  assert.equal(select.localName, 'select');
  const originalGuild = h.app.state.guildId;
  await chooseOption(select, 'Second');
  assert.equal(select.value, originalGuild);
  assert.equal(h.app.state.guildId, originalGuild);
  assert.equal(draft.value, 'Unsaved template');
  assert.equal(h.app.isDirty(), true);
  assert.deepEqual(calls, []);
});


test('failed copy-source load reports the error and leaves copy unavailable', async () => {
  let writes = 0;
  const { app, elements, nodes } = harness(async (url, opts) => {
    if (url.endsWith('/copy')) writes++;
    throw new Error('Source unavailable');
  });
  app.state.config = { enrollment_id: 'destination', settings: { prefix: '!', bot_name: '' }, settings_override: {}, global_settings: { prefix: '!' } };
  app.state.tab = 'settings';
  app.tabSettings();
  const source = elements.find(n => n.localName === 'select' && Array.from(n.options).some(option => option.textContent === 'Select a source server'));
  await chooseOption(source, 'Second');
  assert.match(nodes.get('#toast').textContent, /Source unavailable/);
  const copy = elements.find(n => n.localName === 'button' && n.textContent === 'Copy');
  assert.equal(copy.disabled, false);
  await fire(copy, 'click', {});
  assert.equal(writes, 0);
  assert.match(nodes.get('#toast').textContent, /Select a source server/);
});

function assertAccessible(node) {
  assert.ok(node, 'feedback/recovery control exists');
  for (let ancestor = node; ancestor; ancestor = ancestor.parentElement) {
    assert.notEqual(ancestor.getAttribute('aria-hidden'), 'true');
    assert.notEqual(ancestor.inert, true);
  }
}

test('command plan validation retains editable drafts and accessible errors on blur and Save', async () => {
  const writes = [];
  let refuse = true;
  const h = harness(async (_url, options) => {
    const body = JSON.parse(options.body);
    writes.push(body);
    if (refuse) throw new Error('command write refused');
    return response({ config_patch: { kind: 'command', name: body.data.name, scope: 'guild', value: body.data } });
  });
  h.app.state.config = { commands: [] };
  h.app.state.tab = 'commands';
  const modal = await h.app.editCommand(null);
  const json = modal.querySelector('textarea.mono');
  const save = findNode(modal, n => n.localName === 'button' && n.textContent === 'Save');
  for (const value of ['{broken', 'null', '[]', '{"steps":{}}', ...[null, 1, 'bad', true, []].map(step => JSON.stringify({steps: [step]}))]) {
    json.value = value;
    await fire(json, 'input');
    await fire(json, 'focusout');
    assert.equal(modal.isConnected, true, value);
    assert.equal(json.value, value);
    assert.match(h.nodes.get('#toast').textContent, /Invalid behavior JSON/);
    assertAccessible(h.nodes.get('#toast'));
    await fire(save, 'click');
    assert.equal(modal.isConnected, true);
    assert.equal(writes.length, 0);
    assertAccessible(h.nodes.get('#toast'));
  }
  const name = modal.querySelector('input[placeholder="command name"]');
  name.value = 'hello';
  await fire(name, 'input');
  json.value = '{"steps":[{"type":"reply","template":"Hello"}]}';
  await fire(json, 'input');
  await fire(json, 'focusout');
  await fire(save, 'click');
  assert.equal(modal.isConnected, true);
  assert.equal(name.value, 'hello');
  assert.match(h.nodes.get('#toast').textContent, /command write refused/);
  assertAccessible(h.nodes.get('#toast'));
  refuse = false;
  await fire(save, 'click');
  assert.equal(modal.isConnected, false);
  assert.deepEqual(writes.at(-1).data.behavior.params.steps, [{type: 'reply', template: 'Hello'}]);
});

test('acknowledged command deletion exposes Retry when the config refresh fails', async () => {
  const calls = [];
  const h = harness(async (url, options) => {
    calls.push({url, method: options.method});
    if (options.method === 'DELETE') return response({ok: true});
    throw new Error('refresh unavailable');
  });
  h.app.state.config = { commands: [] };
  h.app.state.tab = 'commands';
  const modal = await h.app.editCommand({name: 'hello', scope: 'guild', effective: {name: 'hello', behavior: {kind: 'plan', params: {steps: []}}}});
  await fire(findNode(modal, n => n.localName === 'button' && n.textContent === 'Delete'), 'click');
  assert.deepEqual(calls.map(call => call.method), ['DELETE', 'POST']);
  assert.equal(h.app.state.config, null);
  assert.equal(modal.isConnected, false);
  assertAccessible(findNode(h.nodes.get('#content'), n => n.localName === 'button' && n.textContent === 'Retry'));
});

test('read views retain request deadlines and cancel obsolete reads without rendering errors', async () => {
  for (const tab of ['dashboard', 'events', 'bans', 'actions']) {
    const signals = [];
    const h = harness((_url, options) => new Promise((_resolve, reject) => {
      signals.push(options.signal);
      options.signal.addEventListener('abort', () => reject(Object.assign(new Error('aborted'), {name: 'AbortError'})), {once: true});
    }));
    h.app.state.config = {enrollment_id: 'membership', settings: {}, rules: [], commands: [], badwords: [], log_channels: [], templates: [], event_types: []};
    h.app.navigate(tab);
    await settle();
    const deadline = [...h.timers.values()].find(timer => timer.delay === 30000);
    assert.ok(deadline, tab + ' must bound loading');
    await deadline.fn();
    assert.equal(signals[0].aborted, true);
    assert.match(textOf(h.nodes.get('#content')), tab === 'dashboard' ? /Unavailable/ : /Request timed out/);
    assert.equal([...h.timers.values()].some(timer => timer.delay === 30000), false);
    h.app.navigate(tab);
    await settle();
    const pending = signals.at(-1);
    h.app.navigate('templates');
    await settle();
    assert.equal(pending.aborted, true);
    assert.equal([...h.timers.values()].some(timer => timer.delay === 30000), false);
    assert.doesNotMatch(textOf(h.nodes.get('#content')), /Request timed out|Failed to fetch/);
  }
});

test('field labels exclude control values and command steps have individual identities', async () => {
  const h = harness();
  h.app.state.config = {templates: [{event: 'member_join', effective: 'Welcome {user}'}]};
  h.app.navigate('templates');
  await settle();
  for (const label of h.nodes.get('#content').querySelectorAll('.field > label')) {
    assert.ok(['Message template', 'Save to'].includes(label.textContent));
    assert.ok(label.control);
    assert.equal(label.control.getAttribute('aria-labelledby'), label.id);
  }
  const modal = await h.app.editCommand(null);
  await fire(findNode(modal, n => n.localName === 'button' && n.textContent === '+ add step'), 'click');
  const inputs = [...modal.querySelectorAll('.step-card input, .step-card select')];
  assert.equal(inputs.length, 4);
  assert.equal(new Set(inputs.map(input => input.id)).size, inputs.length);
  for (const input of inputs) assert.match(input.getAttribute('aria-label'), /^Step (action|value)$/);
});

test('appearance retains the session choice across startup and workspace when storage is blocked', async () => {
  const blocked = {get() { throw new Error('blocked'); }, set() { throw new Error('blocked'); }};
  const h = harness(undefined, blocked);
  const picker = h.document.querySelector('select[aria-label="Appearance"]');
  picker.value = 'dark';
  await fire(picker, 'change');
  assert.equal(h.document.documentElement.dataset.theme, 'dark');
  h.app.state.config = {templates: []};
  h.app.navigate('templates');
  await settle();
  assert.equal(picker.isConnected, false);
  assert.equal(h.document.querySelector('select[aria-label="Appearance"]').value, 'dark');
  assert.equal(h.document.documentElement.dataset.theme, 'dark');
});

test('custom pattern draft save uses dry_run and custom id', () => {
  const h = harness();
  const draft = h.app.buildCustomRuleSaveBody(
    {
      name: 'x',
      dry_run: false,
      params: { kind: 'pattern', pattern: 'scam' },
      actions: [{ at: 1, action: 'delete', reason: '', duration_minutes: 0, delete_days: 0 }],
    },
    true,
  );
  assert.equal(draft.dry_run, true);
  assert.equal(draft.params.kind, 'pattern');
  const record = h.app.newCustomRuleDraft();
  assert.match(record.rule_id, /^custom:[0-9a-f]{16}$/);
});
