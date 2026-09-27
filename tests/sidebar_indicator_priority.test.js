// ---------------------------------------------------------------------------
// Indicator priority over the nested sidebar (status.js + groups.js): two
// independent families, each picking at most one element per row in a fixed
// order that reads no expansion state. The activity family — the row's own
// icon (spinner, a legacy row's own gear, the clock), then a parent's gear
// for a running descendant. The unread family — the row's own filled dot,
// else the subtree's hollow mark; no activity state gates either family.
// The clock never stands in for a subtree. Facts arrive through the list
// paint, the status poll (applySessionStatus) and the websocket broadcasts.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { createElement } = require('./dom_element_stub');
const { baseSessionContext, buildSidebarFilterElements, createChatSidebarContext, inlinePageTimers,
  makeSessionMeta } = require('./session_context_stub');

const at = (hour) => `2026-04-02T${String(hour).padStart(2, '0')}:00:00Z`;
const ICON_KINDS = ['spinner', 'worker-indicator', 'waiting-indicator', 'unread', 'subtree-unread'];

function indicatorElements(ids) {
  const els = new Map();
  ids.forEach((id) => {
    ICON_KINDS.forEach((kind) => {
      els.set(kind + '-' + id, createElement({className: 'hidden'}));
    });
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
    subtreeMark: !elements.get('subtree-unread-' + id).classList.contains('hidden'),
  });
  return {context, nav, elements, shown};
}

function row(id, parent, overrides = {}) {
  return makeSessionMeta(id, {group: 'Work', status: 'active', profile: parent ? 'worker' : 'manager',
    task_parent_id: parent, updated_at: at(10), ...overrides});
}

const IDLE_ICONS = {spinner: false, gear: false, clock: false, dot: false, subtreeMark: false};

test('a parent shows the gear for a running descendant; without unread facts no mark shows', () => {
  const {context, shown} = buildContext(['p', 'c', 'g']);
  context.renderSessionList([
    row('p', null), row('c', 'p', {profile: 'manager'}), row('g', 'c', {has_running_tasks: true}),
  ], 'all');

  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true}, 'the root stands in for the running grandchild');
  assert.deepEqual(shown('c'), {...IDLE_ICONS, gear: true}, 'the middle node too');
  assert.equal(context.Sidebar.effectiveIndicatorState('p'), 'worker_only');
});

test('an expanded parent keeps the gear: expansion never changes the icon', () => {
  const {context, shown} = buildContext(['p', 'c']);
  context.renderSessionList([row('p', null), row('c', 'p', {has_running_tasks: true})], 'all');
  assert.equal(shown('p').gear, true);

  context.Sidebar.expandTreeNode('p');

  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true});
  assert.equal(context.Sidebar.effectiveIndicatorState('p'), 'worker_only');
  context.Sidebar.toggleTreeNode('p');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true}, 'collapsing again changes nothing');
});

test('icons are identical before and after expand and collapse', () => {
  const {context, shown} = buildContext(['p', 'c', 'g']);
  context.renderSessionList([
    row('p', null),
    row('c', 'p', {profile: 'manager', has_unread: true}),
    row('g', 'c', {has_running_tasks: true}),
  ], 'all');
  const before = shown('p');

  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'), before, 'expansion never changes the row’s icons');
  context.Sidebar.toggleTreeNode('p');
  assert.deepEqual(shown('p'), before, 'collapse never changes the row’s icons');
  assert.deepEqual(before, {...IDLE_ICONS, gear: true, subtreeMark: true},
      'first paint: the running descendant’s gear and the unread child’s subtree mark show together');

  // The repaint path (the activity seam) agrees with the first paint.
  context.setSessionIndicator('g', 'worker_only');
  assert.deepEqual(shown('p'), before, 'repaint keeps the gear beside the subtree mark');
  context.refreshSessionIndicator('p');
  assert.deepEqual(shown('p'), before, 'the unread-only repaint keeps both marks');
});

test('a parent’s own dot shows beside the stand-in gear and outranks the subtree mark', () => {
  const {context, shown} = buildContext(['p', 'a', 'b']);
  context.renderSessionList([
    row('p', null, {has_unread: true}),
    row('a', 'p', {has_running_tasks: true}),
    row('b', 'p', {profile: 'manager', has_unread: true}),
  ], 'all');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true, dot: true},
      'first paint: the gear stands in for the running child beside the own dot');

  // The repaint path (the unread broadcast's seam) agrees: the own dot still
  // outranks the subtree mark while the gear shows.
  context.recordUnreadFact('p', true);
  context.refreshSessionIndicator('p');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true, dot: true},
      'repaint: the own dot outranks the subtree mark while the gear shows');
});

test('an unread child manager gives the parent the subtree mark, not the dot, collapsed and expanded', () => {
  const {context, nav, shown} = buildContext(['p', 'c']);
  context.renderSessionList([row('p', null), row('c', 'p', {profile: 'manager', has_unread: true})], 'all');

  assert.deepEqual(shown('p'), {...IDLE_ICONS, subtreeMark: true});
  // The child's own dot renders at paint; only parent rows get a post-paint pass.
  const dotClass = nav.innerHTML.match(/<span id="unread-c"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(dotClass), false, 'the child shows its own dot');
  assert.equal(context.Sidebar.ownUnread('p'), false);
  assert.equal(context.Sidebar.subtreeUnread('p'), true);

  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, subtreeMark: true}, 'expansion never changes the icon');
  context.Sidebar.toggleTreeNode('p');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, subtreeMark: true}, 'collapse never changes the icon');
});

test('a running child and an unread sibling light the gear and the subtree mark together', () => {
  const {context, shown} = buildContext(['p', 'a', 'b']);
  context.renderSessionList([
    row('p', null),
    row('a', 'p', {has_running_tasks: true}),
    row('b', 'p', {profile: 'manager', has_unread: true}),
  ], 'all');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true, subtreeMark: true},
      'first paint: the gear stands in for the running child beside the sibling’s mark');

  context.setSessionIndicator('a', 'idle');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, subtreeMark: true}, 'the mark stays when the Run ends');
  context.setSessionIndicator('a', 'worker_only');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true, subtreeMark: true},
      'repaint: the gear returns beside the mark when the Run restarts');
});

test('the row’s own dot outranks the subtree mark', () => {
  const {context, shown} = buildContext(['p', 'c']);
  context.renderSessionList([
    row('p', null, {has_unread: true}), row('c', 'p', {profile: 'manager', has_unread: true}),
  ], 'all');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, dot: true},
      'the row’s own unread paints the dot and hides the subtree mark');

  // The real read path (recordUnreadFact + refreshSessionIndicator) clears the
  // dot and reveals the subtree mark in the same paint.
  context.recordUnreadFact('p', false);
  context.refreshSessionIndicator('p');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, subtreeMark: true});
});

test('opening an unread child clears every ancestor’s mark in the same paint', () => {
  const {context, shown} = buildContext(['root', 'mid', 'leaf']);
  context.renderSessionList([
    row('root', null),
    row('mid', 'root', {profile: 'manager', has_unread: true}),
    row('leaf', 'mid'),
  ], 'all');
  assert.deepEqual(shown('root'), {...IDLE_ICONS, subtreeMark: true});
  assert.deepEqual(shown('mid'), {...IDLE_ICONS, dot: true});

  // The real read path (app.js's initial render, switchSession's winning
  // generation): recordUnreadFact(id, false), then refreshSessionIndicator(id).
  context.recordUnreadFact('mid', false);
  context.refreshSessionIndicator('mid');

  assert.deepEqual(shown('mid'), IDLE_ICONS, 'the opened row’s dot clears');
  assert.deepEqual(shown('root'), IDLE_ICONS, 'the ancestor’s subtree mark clears in the same paint');
});

test('a thinking descendant lights the parent’s gear', () => {
  const {context, shown} = buildContext(['p', 'w']);
  context.renderSessionList([row('p', null), row('w', 'p', {profile: 'worker'})], 'all');
  assert.deepEqual(shown('p'), IDLE_ICONS);

  // A worker's live Run carries its header timer: its own row reads thinking.
  context.setSessionIndicator('w', 'thinking');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true});

  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true}, 'expansion never changes the icon');
  context.setSessionIndicator('w', 'idle');
  context.Sidebar.toggleTreeNode('p');
  assert.deepEqual(shown('p'), IDLE_ICONS, 'the gear clears when the Run ends');
});

test('a row’s own thinking spinner outranks its subtree', () => {
  const {context, shown} = buildContext(['p', 'c']);
  context.renderSessionList([row('p', null), row('c', 'p', {has_running_tasks: true})], 'all');

  context.setSessionIndicator('p', 'thinking');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, spinner: true});

  context.setSessionIndicator('p', 'idle');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true}, 'back to the stand-in when its own work ends');
});

test('a status-poll reply for a leaf repaints its ancestors', async () => {
  const {context, shown} = buildContext(['p', 'c', 'g']);
  context.renderSessionList([row('p', null), row('c', 'p', {profile: 'manager'}), row('g', 'c')], 'all');
  assert.deepEqual(shown('p'), IDLE_ICONS);

  context.document.querySelectorAll = (selector) => (
    selector === 'a[id^="session-"]' ? [{id: 'session-p'}, {id: 'session-c'}, {id: 'session-g'}] : []
  );
  let reply = {g: {has_running_tasks: true, has_unread: false}};
  context.fetch = async () => ({ok: true, json: async () => reply});

  await context.Sidebar.pollSessionStatus();
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true});
  assert.deepEqual(shown('c'), {...IDLE_ICONS, gear: true});
  // The leaf itself runs the work: its own row shows the spinner.
  assert.deepEqual(shown('g'), {...IDLE_ICONS, spinner: true});

  reply = {g: {has_running_tasks: false, has_unread: true}};
  await context.Sidebar.pollSessionStatus();
  assert.deepEqual(shown('p'), {...IDLE_ICONS, subtreeMark: true},
      'the root lights its subtree mark for the leaf’s unread reply');
  assert.deepEqual(shown('c'), {...IDLE_ICONS, subtreeMark: true});
  assert.deepEqual(shown('g'), {...IDLE_ICONS, dot: true}, 'the leaf shows its own dot');
});

test('a childless legacy row keeps main’s behavior: its own state and unread flag only', () => {
  const {context, nav, shown} = buildContext(['solo']);
  // profile null: a legacy session. Its own running state (running worker
  // threads) stays the legacy delegated-work gear, byte for byte.
  context.renderSessionList([row('solo', null, {profile: null, has_unread: true})], 'all');
  // The paint itself renders the dot visible; no post-paint pass touches a childless row.
  const dotClass = nav.innerHTML.match(/<span id="unread-solo"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(dotClass), false);
  assert.equal(context.Sidebar.ownUnread('solo'), true);
  assert.equal(context.Sidebar.subtreeUnread('solo'), false);
  assert.equal(context.Sidebar.effectiveIndicatorState('solo'), 'idle');
  context.setSessionIndicator('solo', 'worker_only');
  assert.deepEqual(shown('solo'), {...IDLE_ICONS, gear: true, dot: true},
      'repaint: the legacy gear and the own dot show together');
  context.setSessionIndicator('solo', 'idle');
  assert.deepEqual(shown('solo'), {...IDLE_ICONS, dot: true});
});

test('a task-tree row’s own running Run paints the spinner, not the gear', () => {
  const {context, nav, shown} = buildContext(['root']);
  // profile manager: a task-tree node. Its own live Run (has_running_tasks
  // from the task-tree derivation) is its own work: the spinner, whatever the
  // row's profile — the gear is a stand-in's icon, never an own one.
  context.renderSessionList([row('root', null, {has_running_tasks: true, has_unread: true})], 'all');
  const spinnerClass = nav.innerHTML.match(/<svg id="spinner-root"[^>]*class="([^"]*)"/)[1];
  const gearClass = nav.innerHTML.match(/<svg id="worker-indicator-root"[^>]*class="([^"]*)"/)[1];
  const dotClass = nav.innerHTML.match(/<span id="unread-root"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(spinnerClass), false, 'own running paints the spinner at paint');
  assert.equal(/\bhidden\b/.test(gearClass), true);
  assert.equal(/\bhidden\b/.test(dotClass), false,
      'first paint: the spinner and the own dot show together');
  // The status poll reports the same fact through the shared seam.
  context.setSessionIndicator('root', 'worker_only');
  assert.deepEqual(shown('root'), {...IDLE_ICONS, spinner: true, dot: true},
      'repaint: the spinner keeps the own dot beside it');
  context.setSessionIndicator('root', 'idle');
  assert.deepEqual(shown('root'), {...IDLE_ICONS, dot: true});
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
  assert.deepEqual(shown('w'), {...IDLE_ICONS, spinner: true});
  // The parent still stands in with its gear: it delegates.
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true});

  context.setSessionIndicator('w', 'idle');
  assert.deepEqual(shown('w'), IDLE_ICONS);
});

// ---------------------------------------------------------------------------
// The full priority table (one visual language; two independent families per
// row): the activity family — own state first, running spinner or waiting
// clock, then a parent row's stand-in gear for a running descendant — and the
// unread family — the row's own dot, else the subtree's hollow mark. The
// clock never stands in for a subtree. A failed Run paints no activity icon
// anywhere: the alert-indicator element no longer exists for any state.
// ---------------------------------------------------------------------------

test('no state paints an alert-indicator element; a failed child reads idle', () => {
  const {context, nav, shown} = buildContext(['w']);
  // A failed Run's work verdict is idle (the derivation emits only running
  // and waiting), and no render path emits the retired alert element in any
  // state: the paint, the poll seam, and the stand-in all agree.
  context.renderSessionList([row('w', null, {work_state: 'idle', has_unread: true})], 'all');
  assert.equal(/alert-indicator-/.test(nav.innerHTML), false,
      'no render path emits an alert-indicator element');
  const dotClass = nav.innerHTML.match(/<span id="unread-w"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(dotClass), false,
      'a failed child’s idle row paints only its unread dot');
  // The status poll's seam agrees for the remaining states.
  context.setSessionIndicator('w', 'waiting');
  assert.equal(/alert-indicator-/.test(nav.innerHTML), false);
  assert.deepEqual(shown('w'), {...IDLE_ICONS, clock: true, dot: true},
      'repaint: the waiting clock keeps the own dot beside it');
  context.setSessionIndicator('w', 'thinking');
  assert.equal(/alert-indicator-/.test(nav.innerHTML), false,
      'the spinner is priority 1 and the alert element does not exist');
  assert.deepEqual(shown('w'), {...IDLE_ICONS, spinner: true, dot: true},
      'repaint: the spinner keeps the own dot beside it too');
  context.setSessionIndicator('w', 'idle');
  assert.deepEqual(shown('w'), {...IDLE_ICONS, dot: true},
      'the dot persists through every activity state');
});

test('a task-tree row’s own waiting (queued) paints the muted clock beside its unread dot', () => {
  const {context, nav, shown} = buildContext(['w']);
  context.renderSessionList([row('w', null, {work_state: 'waiting', has_unread: true})], 'all');
  const clockClass = nav.innerHTML.match(/<svg id="waiting-indicator-w"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(clockClass), false, 'own waiting paints the clock at paint');
  const dotClass = nav.innerHTML.match(/<span id="unread-w"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(dotClass), false,
      'first paint: the clock and the own dot show together');
  context.setSessionIndicator('w', 'waiting');
  assert.deepEqual(shown('w'), {...IDLE_ICONS, clock: true, dot: true},
      'repaint: the clock keeps the own dot beside it');
});

test('a parent over a failed (idle) child shows no activity icon', () => {
  const {context, shown} = buildContext(['p', 'w']);
  context.renderSessionList([row('p', null), row('w', 'p', {work_state: 'idle'})], 'all');
  assert.deepEqual(shown('p'), IDLE_ICONS,
      'a failed child is no activity: the parent paints nothing');

  // Expanding the parent changes nothing.
  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'), IDLE_ICONS);
});

test('a waiting child gives the parent no clock; the queued row keeps its own', () => {
  const {context, nav, shown} = buildContext(['p', 'w']);
  context.renderSessionList([row('p', null), row('w', 'p', {work_state: 'waiting'})], 'all');
  assert.deepEqual(shown('p'), IDLE_ICONS, 'the clock never stands in for a subtree');
  // The queued row's own clock renders at paint; only parent rows get a post-paint pass.
  const clockClass = nav.innerHTML.match(/<svg id="waiting-indicator-w"[^>]*class="([^"]*)"/)[1];
  assert.equal(/\bhidden\b/.test(clockClass), false, 'the queued row shows its own clock');

  context.Sidebar.expandTreeNode('p');
  assert.deepEqual(shown('p'), IDLE_ICONS, 'expansion never changes the icon');
});

test('the subtree stand-in is the gear alone: waiting descendants contribute nothing', () => {
  const {context, shown} = buildContext(['p', 'a', 'b']);
  context.renderSessionList([
    row('p', null),
    row('a', 'p', {work_state: 'waiting'}),
    row('b', 'p', {work_state: 'waiting'}),
  ], 'all');
  assert.deepEqual(shown('p'), IDLE_ICONS, 'queued descendants light no stand-in icon');
  context.setSessionIndicator('a', 'worker_only');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, gear: true}, 'a running descendant lights the gear');
  context.setSessionIndicator('a', 'idle');
  assert.deepEqual(shown('p'), IDLE_ICONS);
});

test('the waiting clock and the descendant’s own dot show together; the mark reads the subtree', () => {
  const {context, shown} = buildContext(['p', 'w']);
  context.renderSessionList([row('p', null), row('w', 'p', {work_state: 'waiting'})], 'all');
  context.recordUnreadFact('w', true);
  context.refreshSessionIndicator('w');
  assert.deepEqual(shown('w'), {...IDLE_ICONS, clock: true, dot: true},
      'repaint: the waiting clock and the descendant’s own dot show together');
  assert.deepEqual(shown('p'), {...IDLE_ICONS, subtreeMark: true},
      'the parent shows the subtree mark for the unread reply, no clock for the queued one');
});
