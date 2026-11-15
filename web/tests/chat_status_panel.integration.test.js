const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {
  FakeElement,
  fakeEngineNode,
  makeEngineTimers,
  installEngineTimers,
  installScrollTopClamp,
  settle,
  distanceFromBottom,
  eMsg,
  eTurn,
} = require('./turn_engine_harness');
const {baseSessionContext, createChatSidebarContext} = require('./session_context_stub');
const {createFakeWebSocketClass} = require('./fake_websocket');
const {loadWebsocketModules} = require('./websocket_context_stub');
const {readStatic} = require('./read_static');

// ---------------------------------------------------------------------------
// The session status surfaces (chat/status-panel.js) on a page of the real chat,
// sidebar, tab and WebSocket modules. The Status view panel (#status-panel) takes
// the message list's place while Status is selected in the Chat view toggle
// (chat/for-you.js). The waiting count shows on the toggle option's badge.
// Rows are read back by their data attributes, so a case asserts structure
// and the values the view carries, not the labels. The phone layout comes from a
// platform stub that switchLayout drives the way platform.js does: a mode change
// runs each onChange callback.
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
function panelRig() {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  const stream = new FakeElement('DIV', {id: 'streaming-msg'});
  root.appendChild(stream);
  // The elements carry the template's classes (index.html), the way the page's markup builds them.
  const chat = new FakeElement('DIV', {id: 'tab-chat', className: 'flex-1 flex flex-col min-h-0 min-w-0 relative'});
  const panel = new FakeElement('DIV', {
    id: 'status-panel',
    className: 'hidden flex-1 min-h-0 min-w-0 overflow-y-auto bg-slate-900 p-4 text-xs space-y-1',
  });
  const badge = new FakeElement('SPAN', {id: 'status-waiting-count', className: 'hidden'});
  const chatButton = new FakeElement('BUTTON', {id: 'btn-chat', className: 'bg-blue-600/20 text-blue-300'});
  // The view toggle as index.html renders it: For you and Everything always, the
  // Status option hidden until a web session opens. The badge is the option's span.
  const toggles = ['foryou', 'everything', 'status'].map((option) => {
    const btn = new FakeElement('BUTTON', option === 'status' ? {className: 'hidden'} : {});
    btn.dataset.chatViewOption = option;
    if (option === 'status') btn.appendChild(badge);
    return btn;
  });
  const scrollButton = new FakeElement('BUTTON', {id: 'scroll-to-bottom', className: 'hidden'});
  const inputArea = new FakeElement('DIV', {id: 'input-area'});
  const elements = new Map([
    ['messages', root], ['streaming-msg', stream], ['tab-chat', chat], ['status-panel', panel],
    ['status-waiting-count', badge], ['btn-chat', chatButton],
    ['scroll-to-bottom', scrollButton], ['input-area', inputArea],
  ]);
  const page = {own_task_state: 'open', web_status: null, descendants: [], children: [], derived_waiting: []};
  const statusGets = [];
  const {context} = baseSessionContext();
  const layout = {mode: 'desktop', callbacks: []};
  context.platform = {
    get isMobile() { return layout.mode !== 'desktop'; },
    onChange(fn) { layout.callbacks.push(fn); },
  };
  context.document.createElement = (tag) => new FakeElement(tag);
  context.document.createTreeWalker = () => ({});
  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelector = () => null;
  context.document.querySelectorAll = (selector) =>
    selector === '[data-chat-view-option]' ? toggles : [];
  const timers = makeEngineTimers();
  installEngineTimers(context, timers);
  context.setInterval = () => 0;
  context.clearInterval = () => {};
  context.fetch = async (url) => {
    if (url.startsWith('/api/internal/web/status')) {
      statusGets.push(url);
      const sessionId = url.slice(url.indexOf('session_id=') + 'session_id='.length);
      return {ok: true, status: 200, json: async () => ({
        session_id: sessionId, own_task_state: page.own_task_state, web_status: page.web_status,
        descendants: page.descendants, children: page.children, derived_waiting: page.derived_waiting,
      })};
    }
    return {ok: true, status: 200, json: async () => []};
  };
  context.marked = {parse: (text) => text, Renderer: function Renderer() {}, use() {}};
  context.wsUrlWithToken = (path) => path;
  context.WebSocket = createFakeWebSocketClass();
  context.eventCursor = 0;
  context.reconnectDelay = 1000;
  // tabs.js loads in the connection layer, before the sidebar module that calls switchTab on it.
  vm.createContext(context);
  vm.runInContext(readStatic('tabs.js'), context, {filename: 'tabs.js'});
  createChatSidebarContext(context);
  context.Chat.buildTurnEngineMessageNode = fakeEngineNode;
  loadWebsocketModules(context);
  // The message list is the other suites' subject: a live message lands there
  // through the stub, and the status panel reads the message it is handed.
  context.appendMessageObject = () => {};
  return {context, root, stream, chat, panel, badge, toggles, scrollButton, inputArea, page, statusGets, layout,
    timers, FakeWebSocket: context.WebSocket};
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

// Selects the Status view the way the toggle's click does.
function selectStatus(rig) {
  rig.context.setChatView('status');
}

// The toggle's Status option, hidden until a web session opens.
function statusOption(rig) {
  return rig.toggles[2];
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

// The Status view's rows, in order.
function viewKinds(panel) {
  return panel.children.map((el) => el.dataset.statusRow);
}

function viewRow(panel, kind) {
  return panel.children.find((el) => el.dataset.statusRow === kind);
}

function viewWaitingItems(panel) {
  return viewRow(panel, 'waiting').querySelectorAll('[data-waiting-item]');
}

function viewChildItems(panel) {
  return panel.querySelectorAll('[data-child-item]');
}

function viewChildNowItems(panel) {
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

test('the Status view badge carries this session\'s waiting count and hides at 0', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(2, 1);
  await openSession(rig, {web_status: STATUS});
  assert.equal(rig.badge.classList.contains('hidden'), false, 'two waiting items show the badge');
  assert.equal(rig.badge.textContent, '2');

  // A child session's waiting items stay out of the badge: the count is this
  // session's own Waiting on you list.
  await openSession(rig, {web_status: {...STATUS, waiting: []}});
  assert.equal(rig.badge.classList.contains('hidden'), true, 'an empty waiting list hides the badge');
  assert.equal(rig.badge.textContent, '');

  await openSession(rig, {web_status: null});
  assert.equal(rig.badge.classList.contains('hidden'), true, 'no status hides the badge');
});

test('the Status view panel scrolls by itself with no height cap', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  assert.equal(rig.panel.classList.contains('overflow-y-auto'), true, 'the panel scrolls by itself');
  assert.equal(/max-h-/.test(rig.panel.className), false, 'no height cap cuts the waiting items off');
});

test('the Status view shows Goal, Now, Waiting on you, children and child Now rows in order', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(2, 1);
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  assert.equal(rig.panel.classList.contains('hidden'), false);
  assert.deepEqual(viewKinds(rig.panel), ['goal', 'now', 'waiting', 'children', 'children-now']);
  assert.equal(viewRow(rig.panel, 'goal').children[1].textContent, STATUS.goal);
});

test('the Now row carries the full text and its update time', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  const row = viewRow(rig.panel, 'now');
  assert.equal(row.children[1].children[0].textContent, STATUS.now);
  assert.equal(row.querySelector('[data-status-updated]').dataset.statusUpdated, STATUS.updated_at);
});

test('own Waiting on you items show open Contexts; the empty list reads "Nothing"', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {
    ...STATUS,
    waiting: [{text: 'Decide the size', context: 'The sweep costs $5 an hour.', link: null}],
  }});
  selectStatus(rig);
  const item = viewWaitingItems(rig.panel)[0];
  const toggle = item.querySelector('[data-context-toggle]');
  const body = item.querySelector('[data-context-body]');
  assert.equal(toggle.getAttribute('aria-expanded'), 'true', 'an own item shows its Context open');
  assert.equal(body.classList.contains('hidden'), false);
  assert.equal(body.textContent, 'The sweep costs $5 an hour.');

  toggle.onclick();
  assert.equal(toggle.getAttribute('aria-expanded'), 'false');
  assert.equal(body.classList.contains('hidden'), true, 'the fold still closes on click');

  const empty = panelRig();
  await openSession(empty, {web_status: {...STATUS, waiting: []}});
  selectStatus(empty);
  assert.equal(viewRow(empty.panel, 'waiting').querySelectorAll('[data-waiting-empty]').length, 1);
  assert.equal(viewWaitingItems(empty.panel).length, 0);
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
  selectStatus(rig);
  const body = viewWaitingItems(rig.panel)[0].querySelector('[data-context-body]');
  const anchors = allElements(body).filter((el) => el.tagName === 'A');
  assert.deepEqual(anchors.map((el) => el.href), ['https://example.com/plan', '/absolute_filepath/tmp/page.html']);
});

test('an item without context renders no fold', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {...STATUS, waiting: [{text: 'Approve the deploy', link: null}]}});
  selectStatus(rig);
  assert.equal(viewWaitingItems(rig.panel)[0].querySelectorAll('[data-context]').length, 0);
});

test('descendant items group by session with the id cut to eight characters and their Contexts folded', async () => {
  const rig = panelRig();
  rig.page.descendants = [{
    session_id: CHILD_ID,
    name: 'Child one',
    updated_at: '2026-10-16T20:30:00Z',
    waiting: [{text: 'Decide the size', context: 'Why the decision comes now.', link: null}],
  }, {
    session_id: OTHER_CHILD_ID,
    name: 'Child two',
    updated_at: '2026-10-16T20:30:00Z',
    waiting: [{text: 'Child two item', link: null}],
  }];
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  const row = viewRow(rig.panel, 'children');
  const headers = row.querySelectorAll('[data-child-group]');
  assert.deepEqual(headers.map((el) => el.textContent),
    ['Child one · ' + CHILD_ID.slice(0, 8), 'Child two · ' + OTHER_CHILD_ID.slice(0, 8)]);
  const items = viewChildItems(rig.panel);
  assert.equal(items.length, 2, 'one line per descendant item under its group');
  const body = items[0].querySelector('[data-context-body]');
  assert.equal(items[0].querySelector('[data-context-toggle]').getAttribute('aria-expanded'), 'false',
      'a descendant context stays folded');
  assert.equal(body.classList.contains('hidden'), true);
  assert.equal(body.textContent, 'Why the decision comes now.');
  const link = items[0].querySelectorAll('[data-open-link]');
  assert.equal(link.length, 1);
  assert.equal(link[0].href, '/?session=' + CHILD_ID, 'the item links to its child session page');
});

test('every Waiting on you item of the session shows in its panel, the user-act ones included', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {
    ...STATUS,
    waiting: [
      {text: 'Decide the size', link: null},
      {text: 'Log into the admin console', link: null},
    ],
  }});
  selectStatus(rig);
  const items = viewWaitingItems(rig.panel).map((el) => el.textContent);
  assert.equal(items.length, 2);
  assert.ok(items[1].includes('Log into the admin console'), items[1]);
  assert.deepEqual(viewKinds(rig.panel), ['goal', 'now', 'waiting']);
  assert.equal(rig.badge.textContent, '2');
});

test('descendant items show in their session group, the user-act ones included', async () => {
  const rig = panelRig();
  rig.page.descendants = [{
    session_id: CHILD_ID,
    name: 'Child one',
    updated_at: '2026-10-16T20:30:00Z',
    waiting: [
      {text: 'Log into the release console', link: null},
      {text: 'Decide the size', link: null},
    ],
  }];
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  const items = viewChildItems(rig.panel).map((el) => el.textContent);
  assert.equal(items.length, 2);
  assert.ok(items[0].includes('Log into the release console'), items[0]);
  assert.ok(items[1].includes('Decide the size'), items[1]);
});

test('a derived item shows under its holder: the session\'s own in Waiting on you, a child\'s in its group', async () => {
  const rig = panelRig();
  rig.page.descendants = [{session_id: OTHER_CHILD_ID, name: 'Child two', updated_at: '2026-10-16T20:30:00Z', waiting: []}];
  rig.page.derived_waiting = [{
    session_id: 'session-a',
    text: 'Approve the ssh login to synthetic-login-host on /host-auth',
    context: 'Synthetic login check',
    link: '/host-auth',
    since: '2026-11-01T21:43:00-07:00',
  }, {
    session_id: OTHER_CHILD_ID,
    text: 'Approve the ssh login to synthetic-other-host on /host-auth',
    context: 'Synthetic login check',
    link: '/host-auth',
    since: '2026-11-01T21:43:00-07:00',
  }];
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  const own = viewWaitingItems(rig.panel).map((el) => el.textContent);
  assert.equal(own.length, 3, 'the two stored items and the derived one');
  assert.ok(own[2].includes('synthetic-login-host'), own[2]);
  const child = viewChildItems(rig.panel).map((el) => el.textContent);
  assert.equal(child.length, 1);
  assert.ok(child[0].includes('synthetic-other-host'), child[0]);
  // The badge counts the stored items only, as before.
  assert.equal(rig.badge.textContent, '2');
});

test('the child Now rows list each open child with its Now, update time and Open link', async () => {
  const rig = panelRig();
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  const items = viewChildNowItems(rig.panel);
  assert.equal(items.length, 2, 'one row per child');
  assert.equal(items[0].children[0].textContent, 'Child one · ' + CHILD_ID.slice(0, 8) + ': Waiting for the sweep');
  assert.equal(items[0].querySelectorAll('[data-status-updated]').length, 1, 'the row carries its update time');
  const link = items[0].querySelectorAll('[data-open-link]');
  assert.equal(link.length, 1);
  assert.equal(link[0].href, '/?session=' + CHILD_ID, 'the row links to its child session page');
});

test('without children rows the Status view ends at Waiting on you', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  assert.deepEqual(viewKinds(rig.panel), ['goal', 'now', 'waiting']);
});

test('a session with no status shows "No status yet" in the Status view, with the child rows beside it', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: null});
  selectStatus(rig);
  assert.deepEqual(viewKinds(rig.panel), ['none', 'children']);
  assert.equal(viewRow(rig.panel, 'none').textContent, 'No status yet');
});

test('waiting ages show for own items and for descendant items in the Status view', async () => {
  const rig = panelRig();
  const now = Date.now();
  const ownSince = new Date(now - 6.5 * 60 * 60 * 1000).toISOString();
  const childSince = new Date(now - 3.5 * 24 * 60 * 60 * 1000).toISOString();
  rig.page.descendants = [{
    session_id: CHILD_ID,
    name: 'Child one',
    updated_at: '2026-10-16T20:30:00Z',
    waiting: [{text: 'Child decision', link: null, since: childSince, since_exact: false}],
  }];
  await openSession(rig, {web_status: {
    ...STATUS,
    waiting: [{text: 'Own decision', link: null, since: ownSince, since_exact: true}],
  }});
  selectStatus(rig);
  assert.equal(viewWaitingItems(rig.panel)[0].querySelector('[data-waiting-age]').textContent, '6h');
  assert.equal(viewChildItems(rig.panel)[0].querySelector('[data-waiting-age]').textContent, 'at least 3d');
});

test('a live web_status message redraws the badge and the shown Status view', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w1', content: 'Status updated: Now, Waiting on you', web_status: NEWER,
      timestamp: '2026-10-16T21:00:00Z'},
  });
  assert.equal(rig.badge.textContent, '1', 'the badge follows the live status');
  selectStatus(rig);
  assert.equal(viewRow(rig.panel, 'now').children[1].children[0].textContent, NEWER.now);

  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w2', content: 'Status updated: Now', web_status: OLDER, timestamp: 't2'},
  });
  assert.equal(viewRow(rig.panel, 'now').children[1].children[0].textContent, NEWER.now,
      'an older message does not move the Status view back');
});

test('a web_status message from history replaces the status only when it is newer', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: NEWER}, [
    {role: 'web_status', id: 'w0', content: 'Status updated: Now', web_status: OLDER, timestamp: 't0'},
  ]);
  selectStatus(rig);
  assert.equal(viewRow(rig.panel, 'now').children[1].children[0].textContent, NEWER.now,
      'an older message in history does not move the view back');

  const other = panelRig();
  await openSession(other, {id: 'session-b', web_status: null}, [
    {role: 'web_status', id: 'w1', content: 'Status updated: Now', web_status: OLDER, timestamp: 't1'},
    {role: 'web_status', id: 'w2', content: 'Status updated: Now', web_status: NEWER, timestamp: 't2'},
  ]);
  selectStatus(other);
  assert.equal(viewRow(other.panel, 'now').children[1].children[0].textContent, NEWER.now,
      'the newest message in history sets the status');
});

test('a web_status_subtree notice triggers one GET and redraws the shown Status view', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: STATUS});
  assert.equal(rig.statusGets.length, 1, 'the open fetches the child rows once');
  selectStatus(rig);
  assert.equal(viewChildItems(rig.panel).length, 1);

  rig.page.descendants = descendantRows(2, 2);
  await deliver(rig, {type: 'web_status_subtree', source_session_id: OTHER_CHILD_ID});
  assert.equal(rig.statusGets.length, 2, 'the notice triggers exactly one GET');
  assert.equal(viewChildItems(rig.panel).length, 4, 'the Status view redraws with the new rows');
});

test('a session left at Status re-opens with the panel shown and the list hidden', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {id: 'session-a', web_status: STATUS});
  selectStatus(rig);
  assert.equal(rig.panel.classList.contains('hidden'), false, 'the panel takes the list\'s place');
  assert.equal(rig.root.classList.contains('hidden'), true, 'the list is display:none');
  assert.equal(rig.scrollButton.classList.contains('hidden'), true, 'the scroll-to-bottom button hides with the list');
  assert.equal(rig.inputArea.classList.contains('hidden'), false, 'the input box stays visible and usable');
  assert.equal(rig.context.Chat.activeTurnEngineFor(rig.root).view, 'foryou', 'the engine keeps its message view');

  rig.page.descendants = [];
  await openSession(rig, {id: 'session-b', web_status: NEWER});
  assert.equal(rig.panel.classList.contains('hidden'), true, 'the other session opens on its list');
  assert.equal(rig.root.classList.contains('hidden'), false, 'and shows the message list');
  assert.equal(rig.badge.classList.contains('hidden'), false, 'the new session\'s badge shows');
  assert.equal(rig.badge.textContent, String(NEWER.waiting.length), 'session-a\'s badge count is gone');

  await openSession(rig, {id: 'session-a', web_status: STATUS});
  assert.equal(rig.panel.classList.contains('hidden'), false, 'the re-open shows the panel again');
  assert.equal(rig.root.classList.contains('hidden'), true, 'and hides the list');
  selectStatus(rig);  // the toggle click that leaves Status
  assert.equal(rig.context.Chat.activeTurnEngineFor(rig.root).view, 'foryou',
      'leaving Status shows the message view chosen before Status');
  selectStatus(rig);
  assert.equal(viewRow(rig.panel, 'now').children[1].children[0].textContent, STATUS.now);
  assert.equal(viewChildItems(rig.panel).length, 0, 'the child rows of session-b are gone');
  assert.equal(rig.statusGets.length, 3);
});

test('the toggle offers Status for a web session only, and its badge follows the session kinds', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  assert.equal(statusOption(rig).classList.contains('hidden'), false, 'a web session shows the Status option');
  assert.equal(rig.badge.classList.contains('hidden'), false, 'the option\'s badge carries the count');

  await openSession(rig, {id: 'session-b', thread_session: true, slack_origin: {team_id: 'T1', channel_id: 'C1', thread_ts: '1.5'}});
  assert.equal(statusOption(rig).classList.contains('hidden'), true,
      'a Slack thread session shows no Status option');
  assert.equal(rig.badge.classList.contains('hidden'), true, 'and no badge');
  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w1', content: 'Status updated: Now', web_status: NEWER, timestamp: 't'},
  });
  assert.equal(statusOption(rig).classList.contains('hidden'), true,
      'a web_status message does not show the option');
  assert.equal(rig.statusGets.length, 1, 'the thread session makes no descendant GET');

  // A worker session's page renders its own transcript; the status hook takes the session alone.
  rig.context.openSessionStatusPanel({id: 'session-c', profile: 'worker'}, []);
  assert.equal(statusOption(rig).classList.contains('hidden'), true, 'a worker session shows no Status option');
});

test('a child web session shows the Status option, and a discord thread session does not', async () => {
  const rig = panelRig();
  await openSession(rig, {id: 'child-a', task_parent_id: 'parent-1', web_status: STATUS});
  assert.equal(statusOption(rig).classList.contains('hidden'), false, 'a child web session shows the Status option');
  await openSession(rig, {id: 'thread-d', thread_session: true, discord_origin: {guild_id: 'G1', channel_id: 'C2', thread_id: 'th-2'}});
  assert.equal(statusOption(rig).classList.contains('hidden'), true,
      'a Discord thread session shows no Status option');
});

test('selecting Status hides the list and shows the panel in its place', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  assert.equal(rig.panel.classList.contains('hidden'), false, 'the panel takes the list\'s place');
  assert.equal(rig.root.classList.contains('hidden'), true, 'the list is display:none, not covered');
  assert.equal(rig.scrollButton.classList.contains('hidden'), true, 'the scroll-to-bottom button hides');
  assert.equal(rig.inputArea.classList.contains('hidden'), false, 'the input box stays visible');

  rig.context.setChatView('everything');
  assert.equal(rig.panel.classList.contains('hidden'), true, 'leaving Status hides the panel');
  assert.equal(rig.root.classList.contains('hidden'), false, 'and shows the list');
  assert.equal(rig.context.Chat.activeTurnEngineFor(rig.root).view, 'everything',
      'the engine mounts the view the toggle names');
});

// The streaming paint restores the bottom pin after every draft render: a reader
// who is not at the bottom gets the jump button. While Status shows, the list is
// hidden and the button points at nothing, so it stays down; the return sets it
// from the engine's pin state.
test('a streaming paint while Status shows leaves the scroll-to-bottom button hidden', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  rig.context.restoreBottomPin(rig.root, false, false);
  assert.equal(rig.scrollButton.classList.contains('hidden'), true,
      'the hidden list raises no jump button over the panel');
});

// The live readback's failing arrival timing (sitrep_status_view_live_readback_v1,
// case 5, observations A and B): a bottom-pinned reader selects Status. The
// session's running turn then lands visible content in the hidden list. That
// content is a committed turn appended to the engine's store, and a
// streaming-draft update whose paint grows the list's streaming fixture, all
// before the reader leaves Status. After the return no event of any kind
// arrives, and the plan's bound is the bottom within two frames of the return.
// The engine's return projection rides the ResizeObserver's own frame
// scheduling, so the fake runs the return through the page's toggle path
// alone. On the unfixed code nothing reprojects at the unhide, and the reader
// stays off the bottom.
test('content that entered the hidden list pins the return at the bottom with no further event', async () => {
  const rig = panelRig();
  installScrollTopClamp(rig.root);
  rig.root.clientHeight = 300;
  await openSession(rig, {web_status: STATUS}, [...eTurn('h0_', 0), ...eTurn('h1_', 0)]);
  settle(rig.timers);
  // Everything is the readback's failing view: its streaming fixture lives in
  // the list, so a draft update grows the list the reader returns to.
  rig.context.setChatView('everything');
  settle(rig.timers);
  assert.equal(distanceFromBottom(rig.root), 0, 'the reader starts pinned at the bottom');

  selectStatus(rig);
  const engine = rig.context.Chat.activeTurnEngineFor(rig.root);
  // The browser's ResizeObserver marks the hidden list at the switch; the fake
  // runs that zero-height pass by hand before the arrivals.
  rig.root.clientHeight = 0;
  engine.handleResize();

  // Arrival kinds while Status shows, each the engine-facing effect of its page
  // path. The draft paint (usage.js paintStreamDraft) grows the fixture and
  // restores the pin, and the committed turn lands through appendMessage.
  const draftBlock = new FakeElement('DIV');
  draftBlock.__baseHeight = 80;
  rig.stream.appendChild(draftBlock);
  rig.context.restoreBottomPin(rig.root, engine.shouldFollow(), false);
  engine.appendMessage(eMsg('assistant', 'live-s0', 'a step streamed while Status showed'), false);
  engine.appendMessage(eMsg('web_reply', 'live-r0', 'the reply that arrived while Status showed'), false);
  engine.appendMessage(eMsg('separator', 'live-p0', '', {event_index: 5001}), false);

  // The reader leaves Status; nothing arrives afterwards.
  rig.root.clientHeight = 300;
  rig.context.setChatView('everything');
  assert.equal(rig.root.classList.contains('hidden'), false, 'the return shows the list');
  assert.equal(distanceFromBottom(rig.root), 0, 'the return is at the bottom with no further event');

  settle(rig.timers);
  assert.equal(distanceFromBottom(rig.root), 0, 'the bottom holds with no event to repair it');
  assert.ok(rig.root.querySelector('[data-message-id="live-r0"]'),
      'the turn that entered the hidden list is in the list after the return');
});

test('a web_status message renders as a system line in Everything', () => {
  const rig = panelRig();
  const fields = {id: 'x1', content: 'Status updated: Now', timestamp: '2026-10-16T21:00:00Z'};
  const status = rig.context.renderMessage({role: 'web_status', ...fields}, 'session-a');
  const system = rig.context.renderMessage({role: 'system', ...fields}, 'session-a');
  assert.equal(status, system.replace('data-message-role="system"', 'data-message-role="web_status"'));
});

test('a phone Status view stacks each label above its value, with no label column', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  assert.equal(allElements(rig.panel).filter((el) => el.classList.contains('w-28')).length, 0);
  assert.equal(viewRow(rig.panel, 'goal').classList.contains('flex-col'), true);
  assert.equal(viewWaitingItems(rig.panel).length, 2);
});

test('a platform.onChange switch redraws the shown Status view in the new layout', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  selectStatus(rig);
  assert.equal(viewRow(rig.panel, 'goal').classList.contains('flex-col'), false, 'desktop keeps a label column');

  const view = rig.panel;
  switchLayout(rig, 'mobile');
  assert.equal(rig.panel, view, 'the view element is the same');
  assert.equal(viewRow(rig.panel, 'goal').classList.contains('flex-col'), true,
      'a phone stacks each label above its value');

  switchLayout(rig, 'desktop');
  assert.equal(viewRow(rig.panel, 'goal').classList.contains('flex-col'), false, 'back to the label column');
});
