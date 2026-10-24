const assert = require('node:assert/strict');
const test = require('node:test');

const {
  FakeElement,
  fakeEngineNode,
  makeEngineTimers,
  installEngineTimers,
} = require('./turn_engine_harness');
const {baseSessionContext, createChatSidebarContext} = require('./session_context_stub');
const {createFakeWebSocketClass} = require('./fake_websocket');
const {loadWebsocketModules} = require('./websocket_context_stub');

const PANEL_EXPANDED_STORAGE_KEY = 'chat-status-panel-expanded';

// ---------------------------------------------------------------------------
// The session status panel (chat/status-panel.js) on a page of the real chat,
// sidebar and WebSocket modules. Rows are read back by their data attributes, so
// a case asserts structure and the values the panel carries, not the labels. The
// phone layout comes from a platform stub that switchLayout drives the way
// platform.js does: a mode change runs each onChange callback.
// ---------------------------------------------------------------------------

const STATUS = {
  goal: 'Ship the status panel',
  now: 'Frontend under review',
  waiting: [
    {text: 'Decide the sample size', link: 'https://example.com/plan'},
    {text: 'Approve the deploy', link: null},
  ],
  updated_at: '2026-10-16T20:00:00Z',
};

const NEWER = {
  goal: 'Ship the status panel',
  now: 'Tests running',
  waiting: [{text: 'Review the diff', link: null}],
  updated_at: '2026-10-16T21:00:00Z',
};

const OLDER = {
  goal: 'Ship the status panel',
  now: 'Planning',
  waiting: [],
  updated_at: '2026-10-16T19:00:00Z',
};

const CHILD_ID = 'child-aaaaaaaa-1111';
const OTHER_CHILD_ID = 'child-bbbbbbbb-2222';

const NOW_CHILDREN = [
  {session_id: CHILD_ID, name: 'Child one', now: 'Waiting for the sweep', updated_at: '2026-10-16T20:30:00Z'},
  {session_id: OTHER_CHILD_ID, name: 'Child two', now: 'Tests running', updated_at: '2026-10-16T20:10:00Z'},
];

function descendantRows(...counts) {
  return counts.map((n, i) => ({
    session_id: i === 0 ? CHILD_ID : OTHER_CHILD_ID,
    name: i === 0 ? 'Child one' : 'Child two',
    updated_at: '2026-10-16T20:30:00Z',
    waiting: Array.from({length: n}, (_, j) => ({text: `item ${i}.${j}`, link: null})),
  }));
}

// One page: the chat and sidebar modules, the WebSocket transport and core handlers,
// and a descendant GET answered from `page.descendants`.
function panelRig(localStorageItems = {}) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  const stream = new FakeElement('DIV', {id: 'streaming-msg'});
  root.appendChild(stream);
  const panel = new FakeElement('DIV', {id: 'chat-status-panel', className: 'hidden'});
  const elements = new Map([['messages', root], ['streaming-msg', stream], ['chat-status-panel', panel]]);
  const page = {descendants: [], children: []};
  const statusGets = [];
  const {context, localStorageData} = baseSessionContext({localStorageItems});
  const panelStorageAccesses = [];
  const localStorage = context.localStorage;
  context.localStorage = {
    getItem(key) {
      if (key === PANEL_EXPANDED_STORAGE_KEY) panelStorageAccesses.push({operation: 'get'});
      return localStorage.getItem(key);
    },
    setItem(key, value) {
      if (key === PANEL_EXPANDED_STORAGE_KEY) {
        panelStorageAccesses.push({operation: 'set', value: String(value)});
      }
      localStorage.setItem(key, value);
    },
    removeItem(key) {
      if (key === PANEL_EXPANDED_STORAGE_KEY) panelStorageAccesses.push({operation: 'remove'});
      localStorage.removeItem(key);
    },
  };
  const layout = {mode: 'desktop', callbacks: []};
  context.platform = {
    get isMobile() { return layout.mode !== 'desktop'; },
    onChange(fn) { layout.callbacks.push(fn); },
  };
  context.document.createElement = (tag) => new FakeElement(tag);
  context.document.createTreeWalker = () => ({});
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelector = () => null;
  context.document.querySelectorAll = () => [];
  const timers = makeEngineTimers();
  installEngineTimers(context, timers);
  context.setInterval = () => 0;
  context.clearInterval = () => {};
  context.fetch = async (url) => {
    if (url.startsWith('/api/internal/web/status')) {
      statusGets.push(url);
      const sessionId = url.slice(url.indexOf('session_id=') + 'session_id='.length);
      return {ok: true, status: 200, json: async () => ({
        session_id: sessionId, web_status: null, descendants: page.descendants, children: page.children,
      })};
    }
    return {ok: true, status: 200, json: async () => []};
  };
  context.marked = {parse: (text) => text, Renderer: function Renderer() {}, use() {}};
  context.wsUrlWithToken = (path) => path;
  context.WebSocket = createFakeWebSocketClass();
  context.eventCursor = 0;
  context.reconnectDelay = 1000;
  createChatSidebarContext(context);
  context.Chat.buildTurnEngineMessageNode = fakeEngineNode;
  loadWebsocketModules(context);
  // The message list is the other suites' subject: a live message lands there
  // through the stub, and the status panel reads the message it is handed.
  context.appendMessageObject = () => {};
  return {context, panel, page, statusGets, layout, localStorageData, panelStorageAccesses,
    FakeWebSocket: context.WebSocket};
}

// A layout switch as platform.js makes it: the mode changes, then each callback runs with the new and old mode.
function switchLayout(rig, mode) {
  const previous = rig.layout.mode;
  rig.layout.mode = mode;
  rig.layout.callbacks.forEach((fn) => fn(mode, previous));
}

// The element and every element under it, in document order.
function allElements(el) {
  return [el, ...el.children.flatMap(allElements)];
}

// The descendant GET resolves in microtasks: a case reads the panel after one turn of the queue.
function settled() {
  return new Promise((resolve) => setImmediate(resolve));
}

// Opens a session the way switchSession does: SESSION_ID first, then the render.
async function openSession(rig, session, messages = []) {
  const data = Object.assign({
    id: 'session-a',
    name: 'Session a',
    profile: 'manager',
    backend: 'claude-opus-4.6',
    round_ratings: {},
    task_parent_id: null,
  }, session);
  rig.context.SESSION_ID = data.id;
  rig.context.renderSessionView({
    session: data,
    messages,
    pending_draft: null,
    event_count: 0,
    oldest_message_ordinal: 0,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    switchable_backends: [],
    has_more: false,
  });
  await settled();
}

// The panel's top-level rows, in order: a status row names its kind.
function rowKinds(panel) {
  return panel.children.map((el) => el.dataset.statusRow);
}

function rowByKind(panel, kind) {
  return panel.children.find((el) => el.dataset.statusRow === kind);
}

function nowText(panel) {
  return rowByKind(panel, 'now').children[1].children[0].textContent;
}

function waitingItems(panel) {
  return rowByKind(panel, 'waiting').querySelectorAll('[data-waiting-item]');
}

function childToggle(panel) {
  return rowByKind(panel, 'children').children[0];
}

function childItems(panel) {
  return panel.querySelectorAll('[data-child-item]');
}

function childNowToggle(panel) {
  return rowByKind(panel, 'children-now').children[0];
}

function childNowItems(panel) {
  return panel.querySelectorAll('[data-child-now-item]');
}

// The socket of the open page: an open event, then one server frame.
async function deliver(rig, frame) {
  rig.context.connectWS();
  const socket = rig.FakeWebSocket.instances[rig.FakeWebSocket.instances.length - 1];
  socket.emitOpen();
  socket.emitMessage(frame);
  await settled();
}

test('a web session with a status shows the Goal, Now and Waiting on you rows in order', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  assert.equal(rig.panel.classList.contains('hidden'), false);
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
  assert.equal(rowByKind(rig.panel, 'goal').children[1].textContent, STATUS.goal);
  assert.equal(nowText(rig.panel), STATUS.now);
  const updated = rowByKind(rig.panel, 'now').querySelector('[data-status-updated]');
  assert.equal(updated.dataset.statusUpdated, STATUS.updated_at, 'the Now row carries its update time');
  assert.equal(waitingItems(rig.panel).length, 2);
});

test('a waiting item with a link ends with a link to that URL, and one without a link has none', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  const [withLink, withoutLink] = waitingItems(rig.panel);
  const links = withLink.querySelectorAll('[data-open-link]');
  assert.equal(links.length, 1);
  assert.equal(links[0].href, 'https://example.com/plan');
  assert.equal(withoutLink.querySelectorAll('[data-open-link]').length, 0);
});

test('waiting items show how long they have waited, own and in child sessions', async () => {
  const rig = panelRig();
  const now = Date.now();
  const ownSince = new Date(now - 6.5 * 60 * 60 * 1000).toISOString();
  const childSince = new Date(now - 3.5 * 24 * 60 * 60 * 1000).toISOString();
  const status = {
    ...STATUS,
    waiting: [{text: 'Own decision', link: null, since: ownSince, since_exact: true}],
  };
  rig.page.descendants = [{
    session_id: CHILD_ID,
    name: 'Child one',
    updated_at: '2026-10-16T20:30:00Z',
    waiting: [{text: 'Child decision', link: null, since: childSince, since_exact: false}],
  }];

  await openSession(rig, {web_status: status});

  const ownAge = waitingItems(rig.panel)[0].querySelector('[data-waiting-age]');
  assert.equal(ownAge.textContent, '6h');
  childToggle(rig.panel).onclick();
  const childAge = childItems(rig.panel)[0].querySelector('[data-waiting-age]');
  assert.equal(childAge.textContent, '至少 3d');
});

test('an item whose link is not a web or host-absolute URL gets no link', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {...STATUS, waiting: [{text: 'Run it', link: 'javascript:alert(1)'}]}});
  assert.equal(waitingItems(rig.panel)[0].querySelectorAll('[data-open-link]').length, 0);
});

test('an empty waiting list shows the empty marker and no item', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {...STATUS, waiting: []}});
  const waiting = rowByKind(rig.panel, 'waiting');
  assert.equal(waiting.querySelectorAll('[data-waiting-empty]').length, 1);
  assert.equal(waitingItems(rig.panel).length, 0);
});

test('a null status shows one status row, and the child row still shows for descendant rows', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: null});
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'none']);

  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: null});
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'none', 'children']);
});

test('a session with no web_status key reads the same as a null status', async () => {
  const rig = panelRig();
  await openSession(rig, {});
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'none']);
});

test('the child row counts every descendant item, starts folded, and lists them once opened', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(2, 1);
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting', 'children']);
  const toggle = childToggle(rig.panel);
  assert.equal(toggle.dataset.childCount, '3');
  assert.equal(toggle.getAttribute('aria-expanded'), 'false');
  assert.equal(childItems(rig.panel).length, 0, 'folded: no child item shows');

  // A click re-renders the panel, so the opened row is read again.
  childToggle(rig.panel).onclick();
  assert.equal(childToggle(rig.panel).getAttribute('aria-expanded'), 'true');
  const items = childItems(rig.panel);
  assert.equal(items.length, 3, 'one line per descendant item');
  const text = items[0].children[0].textContent;
  assert.ok(text.includes('Child one') && text.includes(CHILD_ID.slice(0, 8)) && text.includes('item 0.0'));
  assert.ok(!text.includes(CHILD_ID), 'the session id is cut to its first eight characters');
  const link = items[0].querySelectorAll('[data-open-link]');
  assert.equal(link.length, 1);
  assert.equal(link[0].href, '/?session=' + CHILD_ID, 'the item links to its child session page');
});

test('a web_status_subtree notice triggers one GET and redraws the child row with the new count', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: STATUS});
  assert.equal(rig.statusGets.length, 1, 'the open fetches the child rows once');
  assert.equal(childToggle(rig.panel).dataset.childCount, '1');

  rig.page.descendants = descendantRows(2, 2);
  await deliver(rig, {type: 'web_status_subtree', source_session_id: OTHER_CHILD_ID});
  assert.equal(rig.statusGets.length, 2, 'the notice triggers exactly one GET');
  assert.equal(childToggle(rig.panel).dataset.childCount, '4');
});

test('the child Now block starts folded and shows the count of open children', async () => {
  const rig = panelRig();
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting', 'children-now']);
  const toggle = childNowToggle(rig.panel);
  assert.equal(toggle.dataset.childCount, '2');
  assert.equal(toggle.getAttribute('aria-expanded'), 'false');
  assert.equal(toggle.children[1].textContent, "Child sessions' Now · 2");
  assert.equal(childNowItems(rig.panel).length, 0, 'folded: no child Now row shows');
});

test('opened, the child Now block lists each child with its Now, update time and Open link', async () => {
  const rig = panelRig();
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: STATUS});
  childNowToggle(rig.panel).onclick();
  assert.equal(childNowToggle(rig.panel).getAttribute('aria-expanded'), 'true');
  const items = childNowItems(rig.panel);
  assert.equal(items.length, 2, 'one row per child');
  assert.equal(items[0].children[0].textContent, 'Child one · ' + CHILD_ID.slice(0, 8) + ': Waiting for the sweep');
  assert.equal(items[0].querySelectorAll('[data-status-updated]').length, 1, 'the row carries its update time');
  const link = items[0].querySelectorAll('[data-open-link]');
  assert.equal(link.length, 1);
  assert.equal(link[0].href, '/?session=' + CHILD_ID, 'the row links to its child session page');
});

test('a session switch folds the child Now block again', async () => {
  const rig = panelRig();
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: STATUS});
  childNowToggle(rig.panel).onclick();
  assert.equal(childNowItems(rig.panel).length, 2);

  await openSession(rig, {id: 'session-b', web_status: STATUS});
  assert.equal(childNowToggle(rig.panel).getAttribute('aria-expanded'), 'false');
  assert.equal(childNowItems(rig.panel).length, 0);
});

test('a web_status_subtree notice redraws the child Now block with the new rows', async () => {
  const rig = panelRig();
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: STATUS});
  childNowToggle(rig.panel).onclick();
  assert.equal(childNowItems(rig.panel).length, 2);

  rig.page.children = [NOW_CHILDREN[1]];
  await deliver(rig, {type: 'web_status_subtree', source_session_id: CHILD_ID});
  assert.equal(childNowToggle(rig.panel).dataset.childCount, '1');
  assert.equal(childNowItems(rig.panel).length, 1);
});

test('with no children rows the child Now block is absent', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
});

test('a live web_status message updates the panel in place', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  const before = rig.panel;
  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w1', content: 'Status updated: Now, Waiting on you', web_status: NEWER,
      timestamp: '2026-10-16T21:00:00Z'},
  });
  assert.equal(rig.panel, before, 'the panel element is the same');
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
  assert.equal(nowText(rig.panel), NEWER.now);
  assert.equal(waitingItems(rig.panel).length, 1);
});

test('a web_status message from history replaces the status only when it is newer', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: NEWER}, [
    {role: 'web_status', id: 'w0', content: 'Status updated: Now', web_status: OLDER, timestamp: 't0'},
  ]);
  assert.equal(nowText(rig.panel), NEWER.now, 'an older message in history does not move the panel back');

  await openSession(rig, {web_status: null}, [
    {role: 'web_status', id: 'w1', content: 'Status updated: Now', web_status: OLDER, timestamp: 't1'},
    {role: 'web_status', id: 'w2', content: 'Status updated: Now', web_status: NEWER, timestamp: 't2'},
  ]);
  assert.equal(nowText(rig.panel), NEWER.now, 'the newest message in history sets the status');
});

test('a session switch redraws the panel for the session it opens', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {id: 'session-a', web_status: STATUS});
  assert.equal(nowText(rig.panel), STATUS.now);

  rig.page.descendants = [];
  await openSession(rig, {id: 'session-b', web_status: NEWER});
  assert.equal(nowText(rig.panel), NEWER.now);
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting'], 'the child row of session-a is gone');
  assert.equal(rig.statusGets.length, 2);
});

test('a thread session keeps its panel hidden when a web_status message lands', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  await openSession(rig, {id: 'session-b', slack_origin: {team_id: 'T1', channel_id: 'C1', thread_ts: '1.5'}});
  assert.equal(rig.panel.classList.contains('hidden'), true);
  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w1', content: 'Status updated: Now', web_status: NEWER, timestamp: 't'},
  });
  assert.equal(rig.panel.classList.contains('hidden'), true, 'a thread session keeps no panel');
  assert.equal(rig.statusGets.length, 1, 'and makes no descendant GET');
});

test('a web_status message renders as a system line in Everything', () => {
  const rig = panelRig();
  const fields = {id: 'x1', content: 'Status updated: Now', timestamp: '2026-10-16T21:00:00Z'};
  const status = rig.context.renderMessage({role: 'web_status', ...fields}, 'session-a');
  const system = rig.context.renderMessage({role: 'system', ...fields}, 'session-a');
  assert.equal(status, system.replace('data-message-role="system"', 'data-message-role="web_status"'));
});

test('a phone shows the panel as one collapsed row for a status with waiting items', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);
  const toggle = rig.panel.children[0];
  assert.equal(toggle.getAttribute('aria-expanded'), 'false');
  assert.equal(toggle.dataset.waitingCount, '2');
  assert.equal(toggle.dataset.childCount, '0');
});

test('a phone row for a status with no waiting item counts zero', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: {...STATUS, waiting: []}});
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);
  assert.equal(rig.panel.children[0].dataset.waitingCount, '0');
});

test('a phone row for a session with no status carries no waiting count', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: null});
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);
  assert.equal(rig.panel.children[0].dataset.waitingCount, undefined);
});

test('a phone collapsed row counts the descendant items and stays one row', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  rig.page.descendants = descendantRows(2, 1);
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);
  assert.equal(rig.panel.children[0].dataset.childCount, '3');
});

test('the toggle row sticks to the panel top on phones and desktop', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: STATUS});
  const toggle = rig.panel.children[0];
  for (const cls of ['sticky', '-top-2', 'z-10', 'bg-slate-900']) {
    assert.equal(toggle.classList.contains(cls), true, `toggle row carries ${cls}`);
  }

  switchLayout(rig, 'desktop');
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
  for (const cls of ['sticky', '-top-2', 'z-10', 'bg-slate-900']) {
    assert.equal(rig.panel.children[0].classList.contains(cls), true, `desktop toggle row carries ${cls}`);
  }
});

test('the desktop panel has a toggle row and expands when no choice is stored', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});

  assert.equal(rig.localStorageData.has(PANEL_EXPANDED_STORAGE_KEY), false);
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
  assert.equal(rowByKind(rig.panel, 'toggle').getAttribute('aria-expanded'), 'true');
  assert.ok(rig.panelStorageAccesses.some((access) => access.operation === 'get'));
});

test('the desktop toggle stores folding and expansion choices across a session switch', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});

  rowByKind(rig.panel, 'toggle').onclick();
  assert.equal(rig.localStorageData.get(PANEL_EXPANDED_STORAGE_KEY), '0');
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);

  await openSession(rig, {id: 'session-b', web_status: NEWER});
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);
  assert.equal(rowByKind(rig.panel, 'toggle').getAttribute('aria-expanded'), 'false');

  rowByKind(rig.panel, 'toggle').onclick();
  assert.equal(rig.localStorageData.get(PANEL_EXPANDED_STORAGE_KEY), '1');
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
});

test('a stored expanded choice shows the desktop status rows', async () => {
  const rig = panelRig({[PANEL_EXPANDED_STORAGE_KEY]: '1'});
  await openSession(rig, {web_status: STATUS});

  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
  assert.equal(rowByKind(rig.panel, 'toggle').getAttribute('aria-expanded'), 'true');
});

test('the phone neither reads nor writes the desktop panel choice', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);

  rowByKind(rig.panel, 'toggle').onclick();
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
  rowByKind(rig.panel, 'toggle').onclick();
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);
  assert.deepEqual(rig.panelStorageAccesses, []);
  assert.equal(rig.localStorageData.has(PANEL_EXPANDED_STORAGE_KEY), false);
});

test('a folded desktop choice applies when switching from phone mode to desktop', async () => {
  const rig = panelRig({[PANEL_EXPANDED_STORAGE_KEY]: '0'});
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);
  assert.deepEqual(rig.panelStorageAccesses, []);

  switchLayout(rig, 'desktop');
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);
  assert.equal(rowByKind(rig.panel, 'toggle').getAttribute('aria-expanded'), 'false');
  assert.deepEqual(rig.panelStorageAccesses, [{operation: 'get'}]);
});

test('a tap expands the phone panel with the row at its top, and a second tap collapses it', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: STATUS});
  const toggle = () => rig.panel.children[0];

  toggle().onclick();
  assert.equal(toggle().getAttribute('aria-expanded'), 'true');
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting', 'children']);

  toggle().onclick();
  assert.equal(toggle().getAttribute('aria-expanded'), 'false');
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);
});

test('the expanded phone panel holds across a session switch', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: STATUS});
  rig.panel.children[0].onclick();
  await openSession(rig, {id: 'session-b', web_status: NEWER});
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
});

test('the expanded phone layout stacks each label above its value, with no label column', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: STATUS});
  rig.panel.children[0].onclick();
  assert.equal(allElements(rig.panel).filter((el) => el.classList.contains('w-28')).length, 0);
  assert.equal(rowByKind(rig.panel, 'goal').classList.contains('flex-col'), true);
  assert.equal(rowByKind(rig.panel, 'waiting').querySelectorAll('[data-waiting-item]').length, 2);
});

test('the desktop panel keeps its toggle, rows, label column and height cap', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting', 'children']);
  assert.equal(allElements(rig.panel).filter((el) => el.classList.contains('w-28')).length, 3,
      'each of the three labelled rows keeps the label column');
  assert.equal(rig.panel.classList.contains('max-h-48'), true);
  assert.equal(rowByKind(rig.panel, 'toggle').getAttribute('aria-expanded'), 'true');
});

test('a platform.onChange switch redraws the panel in the new layout', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);

  const panel = rig.panel;
  switchLayout(rig, 'mobile');
  assert.equal(rig.panel, panel, 'the panel element is the same');
  assert.deepEqual(rowKinds(rig.panel), ['toggle']);

  switchLayout(rig, 'desktop');
  assert.deepEqual(rowKinds(rig.panel), ['toggle', 'goal', 'now', 'waiting']);
});

test('a waiting item context is a fold closed under the item text', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {
    ...STATUS,
    waiting: [{text: 'Decide the size', context: 'The sweep costs $5 an hour.', link: null}],
  }});
  const item = waitingItems(rig.panel)[0];
  const toggle = item.querySelector('[data-context-toggle]');
  const body = item.querySelector('[data-context-body]');
  assert.equal(toggle.getAttribute('aria-expanded'), 'false');
  assert.equal(body.classList.contains('hidden'), true);
  assert.equal(body.textContent, 'The sweep costs $5 an hour.');
});

test('clicking the context fold opens it and clicking again closes it', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {
    ...STATUS,
    waiting: [{text: 'Decide the size', context: 'The sweep costs $5 an hour.', link: null}],
  }});
  const item = waitingItems(rig.panel)[0];
  const toggle = item.querySelector('[data-context-toggle]');
  const body = item.querySelector('[data-context-body]');

  toggle.onclick();
  assert.equal(toggle.getAttribute('aria-expanded'), 'true');
  assert.equal(body.classList.contains('hidden'), false);

  toggle.onclick();
  assert.equal(toggle.getAttribute('aria-expanded'), 'false');
  assert.equal(body.classList.contains('hidden'), true);
});

test('a link inside an item context becomes an anchor through safeLink', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {
    ...STATUS,
    waiting: [{
      text: 'Decide the size',
      context: 'Evidence: https://example.com/plan and /absolute_filepath/tmp/page.html',
      link: null,
    }],
  }});
  const body = waitingItems(rig.panel)[0].querySelector('[data-context-body]');
  const anchors = allElements(body).filter((el) => el.tagName === 'A');
  assert.deepEqual(anchors.map((el) => el.href), ['https://example.com/plan', '/absolute_filepath/tmp/page.html']);
});

test('slashes inside prose and punctuation after a URL are not links in a context', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {
    ...STATUS,
    waiting: [{
      text: 'Decide the size',
      context: 'Keep and/or drop 1/2 of it. See https://example.com/plan.',
      link: null,
    }],
  }});
  const body = waitingItems(rig.panel)[0].querySelector('[data-context-body]');
  const anchors = allElements(body).filter((el) => el.tagName === 'A');
  assert.deepEqual(anchors.map((el) => el.href), ['https://example.com/plan']);
  assert.equal(body.textContent, 'Keep and/or drop 1/2 of it. See https://example.com/plan.');
});

test('an item without context renders no fold', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {...STATUS, waiting: [{text: 'Approve the deploy', link: null}]}});
  const item = waitingItems(rig.panel)[0];
  assert.equal(item.querySelectorAll('[data-context]').length, 0);
});

test('a descendant item context is a fold closed under the item text', async () => {
  const rig = panelRig();
  rig.page.descendants = [{
    session_id: CHILD_ID,
    name: 'Child one',
    updated_at: '2026-10-16T20:30:00Z',
    waiting: [{text: 'Decide the size', context: 'Why the decision comes now.', link: null}],
  }];
  await openSession(rig, {web_status: STATUS});
  childToggle(rig.panel).onclick();
  const item = childItems(rig.panel)[0];
  const body = item.querySelector('[data-context-body]');
  assert.equal(body.classList.contains('hidden'), true);
  assert.equal(body.textContent, 'Why the decision comes now.');
});
