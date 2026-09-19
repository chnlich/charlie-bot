// ---------------------------------------------------------------------------
// Indicator priority over the nested sidebar (status.js + groups.js): a row's
// own spinner, then its own gear; a collapsed parent stands in for its
// subtree (gear for a running descendant, dot for an unread one); an expanded
// parent shows its own facts only. Facts arrive through the list paint, the
// status poll (applySessionStatus) and the websocket broadcasts.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { createElement } = require('./dom_element_stub');
const { baseSessionContext, buildSidebarFilterElements, createChatSidebarContext, inlinePageTimers,
  makeSessionMeta } = require('./session_context_stub');

const at = (hour) => `2026-04-02T${String(hour).padStart(2, '0')}:00:00Z`;

function indicatorElements(ids) {
  const els = new Map();
  ids.forEach((id) => {
    els.set('spinner-' + id, createElement({className: 'hidden'}));
    els.set('worker-indicator-' + id, createElement({className: 'hidden'}));
    els.set('unread-' + id, createElement({className: 'hidden'}));
  });
  return els;
}

function buildContext(ids) {
  const nav = createElement();
  const elements = new Map([['session-list', nav], ...buildSidebarFilterElements(), ...indicatorElements(ids)]);
  const {context} = baseSessionContext({elements});
  context.SESSION_ID = 'none';
  context.INITIAL_SESSIONS = [];
  context.INITIAL_LOAD_ERRORS = [];
  inlinePageTimers(context);
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = () => [];
  context.document.querySelector = () => null;
  context.fetch = async (url) => { throw new Error('unexpected fetch ' + url); };
  createChatSidebarContext(context);
  const shown = (id) => ({
    spinner: !elements.get('spinner-' + id).classList.contains('hidden'),
    gear: !elements.get('worker-indicator-' + id).classList.contains('hidden'),
    dot: !elements.get('unread-' + id).classList.contains('hidden'),
  });
  return {context, nav, elements, shown};
}

function row(id, parent, overrides = {}) {
  return makeSessionMeta(id, {group: 'Work', status: 'active', profile: parent ? 'worker' : 'manager',
    task_parent_id: parent, updated_at: at(10), ...overrides});
}

test('a collapsed parent shows the gear for a running descendant and its own dot stays hidden behind it', () => {
  const {context, shown} = buildContext(['p', 'c', 'g']);
  context.renderSessionList([
    row('p', null), row('c', 'p', {profile: 'manager'}), row('g', 'c', {has_running_tasks: true}),
  ], 'all');

  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false}, 'the collapsed root stands in for the running grandchild');
  assert.deepEqual(shown('c'), {spinner: false, gear: true, dot: false}, 'the collapsed middle node too');
  assert.equal(context.Sidebar.effectiveIndicatorState('p'), 'worker_only');
});

test('an expanded parent shows only its own facts', () => {
  const {context, shown} = buildContext(['p', 'c']);
  context.renderSessionList([row('p', null), row('c', 'p', {has_running_tasks: true})], 'all');
  assert.equal(shown('p').gear, true);

  context.Sidebar.expandTreeNode('p');

  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: false});
  assert.equal(context.Sidebar.effectiveIndicatorState('p'), 'idle');
});

test('the unread dot of a collapsed parent stands in for an unread descendant, and activity outranks it', () => {
  const {context, shown} = buildContext(['p', 'c']);
  context.renderSessionList([row('p', null), row('c', 'p', {has_unread: true})], 'all');

  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: true});
  assert.equal(context.Sidebar.effectiveUnread('p'), true);

  // The child starts running: the parent's gear replaces its stand-in dot.
  context.setSessionIndicator('c', 'worker_only');
  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false});

  // Expanded, the parent has no unread of its own.
  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: false});
});

test('a row’s own thinking spinner outranks its subtree', () => {
  const {context, shown} = buildContext(['p', 'c']);
  context.renderSessionList([row('p', null), row('c', 'p', {has_running_tasks: true})], 'all');

  context.setSessionIndicator('p', 'thinking');
  assert.deepEqual(shown('p'), {spinner: true, gear: false, dot: false});

  context.setSessionIndicator('p', 'idle');
  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false}, 'back to the stand-in when its own work ends');
});

test('a status poll reply for a leaf repaints its collapsed ancestors', async () => {
  const {context, shown} = buildContext(['p', 'c', 'g']);
  context.renderSessionList([row('p', null), row('c', 'p', {profile: 'manager'}), row('g', 'c')], 'all');
  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: false});

  context.document.querySelectorAll = (selector) => (
    selector === 'a[id^="session-"]' ? [{id: 'session-p'}, {id: 'session-c'}, {id: 'session-g'}] : []
  );
  let reply = {g: {has_running_tasks: true, has_unread: false}};
  context.fetch = async () => ({ok: true, json: async () => reply});

  await context.Sidebar.pollSessionStatus();
  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false});
  assert.deepEqual(shown('c'), {spinner: false, gear: true, dot: false});
  assert.deepEqual(shown('g'), {spinner: false, gear: true, dot: false});

  reply = {g: {has_running_tasks: false, has_unread: true}};
  await context.Sidebar.pollSessionStatus();
  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: true});
  assert.deepEqual(shown('g'), {spinner: false, gear: false, dot: true});
});

test('a childless row keeps main’s behavior: its own state and unread flag only', () => {
  const {context, nav, shown} = buildContext(['solo']);
  context.renderSessionList([row('solo', null, {has_unread: true})], 'all');
  // The paint itself renders the dot visible; no post-paint pass touches a childless row.
  const dotClass = nav.innerHTML.match(/<span id="unread-solo"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(dotClass), false);
  assert.equal(context.Sidebar.effectiveUnread('solo'), true);
  assert.equal(context.Sidebar.effectiveIndicatorState('solo'), 'idle');
  context.setSessionIndicator('solo', 'worker_only');
  assert.deepEqual(shown('solo'), {spinner: false, gear: true, dot: false});
  context.setSessionIndicator('solo', 'idle');
  assert.deepEqual(shown('solo'), {spinner: false, gear: false, dot: true});
});
