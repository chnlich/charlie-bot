// ---------------------------------------------------------------------------
// Indicator priority over the nested sidebar (status.js + groups.js): a row's
// own spinner, then its own clock; a collapsed parent stands in for its
// subtree (gear for a running descendant, clock for a waiting one, dot for an
// unread one); an expanded parent shows its own facts only. Facts arrive
// through the list paint, the status poll (applySessionStatus) and the
// websocket broadcasts.
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
    els.set('waiting-indicator-' + id, createElement({className: 'hidden'}));
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
    clock: !elements.get('waiting-indicator-' + id).classList.contains('hidden'),
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

  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false, clock: false}, 'the collapsed root stands in for the running grandchild');
  assert.deepEqual(shown('c'), {spinner: false, gear: true, dot: false, clock: false}, 'the collapsed middle node too');
  assert.equal(context.Sidebar.effectiveIndicatorState('p'), 'worker_only');
});

test('an expanded parent shows only its own facts', () => {
  const {context, shown} = buildContext(['p', 'c']);
  context.renderSessionList([row('p', null), row('c', 'p', {has_running_tasks: true})], 'all');
  assert.equal(shown('p').gear, true);

  context.Sidebar.expandTreeNode('p');

  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: false, clock: false});
  assert.equal(context.Sidebar.effectiveIndicatorState('p'), 'idle');
});

test('the unread dot of a collapsed parent stands in for an unread descendant, and activity outranks it', () => {
  const {context, shown} = buildContext(['p', 'c']);
  context.renderSessionList([row('p', null), row('c', 'p', {has_unread: true})], 'all');

  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: true, clock: false});
  assert.equal(context.Sidebar.effectiveUnread('p'), true);

  // The child starts running: the parent's gear replaces its stand-in dot.
  context.setSessionIndicator('c', 'worker_only');
  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false, clock: false});

  // Expanded, the parent has no unread of its own.
  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: false, clock: false});
});

test('a thinking descendant lights the collapsed parent’s gear', () => {
  const {context, shown} = buildContext(['p', 'w']);
  context.renderSessionList([row('p', null), row('w', 'p', {profile: 'worker'})], 'all');
  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: false, clock: false});

  // A worker's live Run carries its header timer: its own row reads thinking.
  context.setSessionIndicator('w', 'thinking');
  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false, clock: false});

  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: false, clock: false},
      'expanded, the parent shows only its own state');
  context.setSessionIndicator('w', 'idle');
  context.Sidebar.toggleTreeNode('p');
  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: false, clock: false},
      'the gear clears when the Run ends');
});

test('a row’s own thinking spinner outranks its subtree', () => {
  const {context, shown} = buildContext(['p', 'c']);
  context.renderSessionList([row('p', null), row('c', 'p', {has_running_tasks: true})], 'all');

  context.setSessionIndicator('p', 'thinking');
  assert.deepEqual(shown('p'), {spinner: true, gear: false, dot: false, clock: false});

  context.setSessionIndicator('p', 'idle');
  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false, clock: false}, 'back to the stand-in when its own work ends');
});

test('a status poll reply for a leaf repaints its collapsed ancestors', async () => {
  const {context, shown} = buildContext(['p', 'c', 'g']);
  context.renderSessionList([row('p', null), row('c', 'p', {profile: 'manager'}), row('g', 'c')], 'all');
  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: false, clock: false});

  context.document.querySelectorAll = (selector) => (
    selector === 'a[id^="session-"]' ? [{id: 'session-p'}, {id: 'session-c'}, {id: 'session-g'}] : []
  );
  let reply = {g: {has_running_tasks: true, has_unread: false}};
  context.fetch = async () => ({ok: true, json: async () => reply});

  await context.Sidebar.pollSessionStatus();
  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false, clock: false});
  assert.deepEqual(shown('c'), {spinner: false, gear: true, dot: false, clock: false});
  // The leaf itself runs the work: its own row shows the spinner.
  assert.deepEqual(shown('g'), {spinner: true, gear: false, dot: false, clock: false});

  reply = {g: {has_running_tasks: false, has_unread: true}};
  await context.Sidebar.pollSessionStatus();
  assert.deepEqual(shown('p'), {spinner: false, gear: false, dot: true, clock: false});
  assert.deepEqual(shown('g'), {spinner: false, gear: false, dot: true, clock: false});
});

test('a childless legacy row keeps main’s behavior: its own state and unread flag only', () => {
  const {context, nav, shown} = buildContext(['solo']);
  // profile null: a legacy session. Its own running state (running worker
  // threads) stays the legacy delegated-work gear, byte for byte.
  context.renderSessionList([row('solo', null, {profile: null, has_unread: true})], 'all');
  // The paint itself renders the dot visible; no post-paint pass touches a childless row.
  const dotClass = nav.innerHTML.match(/<span id="unread-solo"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(dotClass), false);
  assert.equal(context.Sidebar.effectiveUnread('solo'), true);
  assert.equal(context.Sidebar.effectiveIndicatorState('solo'), 'idle');
  context.setSessionIndicator('solo', 'worker_only');
  assert.deepEqual(shown('solo'), {spinner: false, gear: true, dot: false, clock: false});
  context.setSessionIndicator('solo', 'idle');
  assert.deepEqual(shown('solo'), {spinner: false, gear: false, dot: true, clock: false});
});

test('a task-tree row’s own running Run paints the spinner, not the gear', () => {
  const {context, nav, shown} = buildContext(['root']);
  // profile manager: a task-tree node. Its own live Run (has_running_tasks
  // from the task-tree derivation) is its own work: the spinner, whatever the
  // row's profile — the gear is a collapsed stand-in's icon, never an own one.
  context.renderSessionList([row('root', null, {has_running_tasks: true})], 'all');
  const spinnerClass = nav.innerHTML.match(/<svg id="spinner-root"[^>]*class="([^"]*)"/)[1];
  const gearClass = nav.innerHTML.match(/<svg id="worker-indicator-root"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(spinnerClass), false, 'own running paints the spinner at paint');
  assert.equal(/\bhidden\b/.test(gearClass), true);
  // The status poll reports the same fact through the shared seam.
  context.setSessionIndicator('root', 'worker_only');
  assert.deepEqual(shown('root'), {spinner: true, gear: false, dot: false, clock: false});
  context.setSessionIndicator('root', 'idle');
  assert.deepEqual(shown('root'), {spinner: false, gear: false, dot: false, clock: false});
});

test('a running worker leaf paints the spinner, not the delegated-work gear', () => {
  const {context, nav, shown} = buildContext(['p', 'w']);
  context.renderSessionList([row('p', null), row('w', 'p', {has_running_tasks: true})], 'all');
  const spinnerClass = nav.innerHTML.match(/<svg id="spinner-w"[^>]*class="([^"]*)"/)[1];
  const gearClass = nav.innerHTML.match(/<svg id="worker-indicator-w"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(spinnerClass), false, 'the leaf\u2019s own run shows as the spinner at paint');
  assert.equal(/\bhidden\b/.test(gearClass), true);

  // The status poll reports the same fact through the shared seam.
  context.setSessionIndicator('w', 'worker_only');
  assert.deepEqual(shown('w'), {spinner: true, gear: false, dot: false, clock: false});
  // The collapsed parent still stands in with its gear: it delegates.
  assert.deepEqual(shown('p'), {spinner: false, gear: true, dot: false, clock: false});

  context.setSessionIndicator('w', 'idle');
  assert.deepEqual(shown('w'), {spinner: false, gear: false, dot: false, clock: false});
});

// ---------------------------------------------------------------------------
// The full icon priority table (one visual language, one activity icon per
// row): own state first — running spinner, waiting clock — then a collapsed
// row's stand-in for its subtree in the same order. The unread dot shows only
// when no activity icon does. A failed Run paints nothing anywhere: the
// alert-indicator element no longer exists for any state.
// ---------------------------------------------------------------------------

test('no state paints an alert-indicator element; a failed child reads idle', () => {
  const {context, nav, shown} = buildContext(['w']);
  // A failed Run's work verdict is idle (the derivation emits only running
  // and waiting), and no render path emits the retired alert element in any
  // state: the paint, the poll seam, and the collapsed stand-in all agree.
  context.renderSessionList([row('w', null, {work_state: 'idle', has_unread: true})], 'all');
  assert.equal(/alert-indicator-/.test(nav.innerHTML), false,
      'no render path emits an alert-indicator element');
  const dotClass = nav.innerHTML.match(/<span id="unread-w"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(dotClass), false,
      'a failed child’s idle row paints only its unread dot');
  // The status poll's seam agrees for the remaining states.
  context.setSessionIndicator('w', 'waiting');
  assert.equal(/alert-indicator-/.test(nav.innerHTML), false);
  assert.deepEqual(shown('w'), {spinner: false, gear: false, clock: true, dot: false});
  context.setSessionIndicator('w', 'thinking');
  assert.equal(/alert-indicator-/.test(nav.innerHTML), false,
      'the spinner is priority 1 and the alert element does not exist');
  assert.deepEqual(shown('w'), {spinner: true, gear: false, clock: false, dot: false});
  context.setSessionIndicator('w', 'idle');
  assert.deepEqual(shown('w'), {spinner: false, gear: false, clock: false, dot: true},
      'back to idle, the unread dot shows again');
});

test('a task-tree row’s own waiting (queued) paints the muted clock and hides the dot', () => {
  const {context, nav, shown} = buildContext(['w']);
  context.renderSessionList([row('w', null, {work_state: 'waiting'})], 'all');
  const clockClass = nav.innerHTML.match(/<svg id="waiting-indicator-w"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(clockClass), false, 'own waiting paints the clock at paint');
  const dotClass = nav.innerHTML.match(/<span id="unread-w"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(dotClass), true, 'the activity icon hides the dot');
  context.setSessionIndicator('w', 'waiting');
  assert.deepEqual(shown('w'),
      {spinner: false, gear: false, clock: true, dot: false});
});

test('a collapsed parent over a failed (idle) child shows no activity icon', () => {
  const {context, shown} = buildContext(['p', 'w']);
  context.renderSessionList([row('p', null), row('w', 'p', {work_state: 'idle'})], 'all');
  assert.deepEqual(shown('p'),
      {spinner: false, gear: false, clock: false, dot: false},
      'a failed child is no activity: the collapsed parent paints nothing');
  // Expanding the parent changes nothing.
  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'),
      {spinner: false, gear: false, clock: false, dot: false});
});

test('a collapsed parent stands in for a waiting descendant with the clock', () => {
  const {context, shown} = buildContext(['p', 'w']);
  context.renderSessionList([row('p', null), row('w', 'p', {work_state: 'waiting'})], 'all');
  assert.deepEqual(shown('p'),
      {spinner: false, gear: false, clock: true, dot: false},
      'the collapsed parent shows the clock for a waiting descendant');
  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'),
      {spinner: false, gear: false, clock: false, dot: false});
});

test('the stand-in order is running, then waiting', () => {
  const {context, shown} = buildContext(['p', 'a', 'b']);
  context.renderSessionList([
    row('p', null),
    row('a', 'p', {work_state: 'waiting'}),
    row('b', 'p', {work_state: 'waiting'}),
  ], 'all');
  assert.deepEqual(shown('p'),
      {spinner: false, gear: false, clock: true, dot: false},
      'a waiting descendant lights the stand-in clock');
  context.setSessionIndicator('a', 'worker_only');
  assert.deepEqual(shown('p'),
      {spinner: false, gear: true, clock: false, dot: false},
      'a running descendant outranks waiting');
  context.setSessionIndicator('a', 'idle');
  context.setSessionIndicator('b', 'idle');
  assert.deepEqual(shown('p'),
      {spinner: false, gear: false, clock: false, dot: false});
});

test('the unread dot yields to every activity icon, own or stand-in', () => {
  const {context, shown} = buildContext(['p', 'w']);
  context.renderSessionList([row('p', null), row('w', 'p', {work_state: 'waiting'})], 'all');
  context.recordUnreadFact('w', true);
  context.refreshSessionIndicator('w');
  assert.deepEqual(shown('w'),
      {spinner: false, gear: false, clock: true, dot: false},
      'the waiting clock hides the descendant’s own dot');
  assert.deepEqual(shown('p'),
      {spinner: false, gear: false, clock: true, dot: false},
      'the collapsed parent’s dot yields to its stand-in clock');
});
