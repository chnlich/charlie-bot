const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {
  FakeElement,
  fakeEngineNode,
  makeEngineTimers,
  installEngineTimers,
} = require('./turn_engine_harness');
const {baseSessionContext, createChatSidebarContext} = require('./session_context_stub');
const {createFakeWebSocketClass} = require('./fake_websocket');
const {loadWebsocketModules} = require('./websocket_context_stub');
const {readStatic} = require('./read_static');

// ---------------------------------------------------------------------------
// The session status surfaces (chat/status-panel.js) on a page of the real chat,
// sidebar, tab and WebSocket modules. The Status tab (#tab-status) takes the Chat
// column's place when it is selected, and the waiting count shows on the tab's
// badge. The tabs are the real tabs.js, so a switch runs the tab's own show and
// hide. Rows are read back by their data attributes, so a case asserts structure
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

const WITH_REMAINING = {
  ...STATUS,
  remaining: [
    {text: 'Move the server to Rust', done: true},
    {text: 'Delete the Python server', done: false},
  ],
  remaining_updated_at: '2026-10-16T20:30:00Z',
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
  const view = new FakeElement('DIV', {
    id: 'tab-status',
    className: 'hidden flex-1 min-h-0 min-w-0 overflow-y-auto bg-slate-900 p-4 text-xs space-y-1',
  });
  const badge = new FakeElement('SPAN', {id: 'status-waiting-count', className: 'hidden'});
  const button = new FakeElement('BUTTON', {id: 'btn-status', className: 'hidden'});
  const chatButton = new FakeElement('BUTTON', {id: 'btn-chat', className: 'bg-blue-600/20 text-blue-300'});
  const elements = new Map([
    ['messages', root], ['streaming-msg', stream], ['tab-chat', chat], ['tab-status', view],
    ['status-waiting-count', badge], ['btn-status', button], ['btn-chat', chatButton],
  ]);
  const page = {web_status: null, descendants: [], children: [], derived_waiting: []};
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
        session_id: sessionId, web_status: page.web_status, descendants: page.descendants, children: page.children,
        derived_waiting: page.derived_waiting,
      })};
    }
    return {ok: true, status: 200, json: async () => []};
  };
  context.marked = {parse: (text) => text, Renderer: function Renderer() {}, use() {}};
  context.wsUrlWithToken = (path) => path;
  context.WebSocket = createFakeWebSocketClass();
  context.eventCursor = 0;
  context.reconnectDelay = 1000;
  // tabs.js loads in the connection layer, before the chat modules whose Status tab registers on it.
  vm.createContext(context);
  vm.runInContext(readStatic('tabs.js'), context, {filename: 'tabs.js'});
  createChatSidebarContext(context);
  context.Chat.buildTurnEngineMessageNode = fakeEngineNode;
  loadWebsocketModules(context);
  // The message list is the other suites' subject: a live message lands there
  // through the stub, and the status panel reads the message it is handed.
  context.appendMessageObject = () => {};
  return {context, root, chat, view, badge, button, page, statusGets, layout,
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

// The Status tab's rows, in order.
function viewKinds(view) {
  return view.children.map((el) => el.dataset.statusRow);
}

function viewRow(view, kind) {
  return view.children.find((el) => el.dataset.statusRow === kind);
}

function viewWaitingItems(view) {
  return viewRow(view, 'waiting').querySelectorAll('[data-waiting-item]');
}

function viewRemainingItems(view) {
  return viewRow(view, 'remaining').querySelectorAll('[data-remaining-item]');
}

function viewChildItems(view) {
  return view.querySelectorAll('[data-child-item]');
}

function viewChildNowItems(view) {
  return view.querySelectorAll('[data-child-now-item]');
}

// The socket of the open page: an open event, then one server frame.
async function deliver(rig, frame) {
  rig.context.connectWS();
  const socket = rig.FakeWebSocket.instances[rig.FakeWebSocket.instances.length - 1];
  socket.emitOpen();
  socket.emitMessage(frame);
  await settled();
}

test('the Status tab badge carries this session\'s waiting count and hides at 0', async () => {
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

test('the Status tab scrolls as one page with no height cap', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: STATUS});
  rig.context.switchTab('status');
  assert.equal(rig.view.classList.contains('overflow-y-auto'), true, 'the whole view scrolls');
  assert.equal(/max-h-/.test(rig.view.className), false, 'no height cap cuts the waiting items off');
});

test('the Status tab shows Goal, Remaining, Now, Waiting on you, children and child Now rows in order', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(2, 1);
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: WITH_REMAINING});
  rig.context.switchTab('status');
  assert.equal(rig.view.classList.contains('hidden'), false);
  assert.deepEqual(viewKinds(rig.view), ['goal', 'remaining', 'now', 'waiting', 'children', 'children-now']);
  assert.equal(viewRow(rig.view, 'goal').children[1].textContent, WITH_REMAINING.goal);
});

test('the Status tab lists open Remaining items first and folds the done ones closed by default', async () => {
  const rig = panelRig();
  const remaining = [
    {text: 'open one', done: false},
    {text: 'done one', done: true},
    {text: 'open two', done: false},
    {text: 'done two', done: true},
  ];
  await openSession(rig, {web_status: {...STATUS, remaining,
    remaining_updated_at: WITH_REMAINING.remaining_updated_at}});
  rig.context.switchTab('status');
  const row = viewRow(rig.view, 'remaining');
  const value = row.children[1];
  assert.equal(value.children[0].textContent, '2 of 4 left');
  assert.equal(value.children[1].dataset.statusUpdated, WITH_REMAINING.remaining_updated_at);

  const openItems = viewRemainingItems(rig.view).filter((el) => el.dataset.remainingItem === 'open');
  assert.deepEqual(openItems.map((el) => el.children[1].textContent), ['open one', 'open two'],
      'the open items read in written order, the done ones folded');
  const toggle = value.querySelector('[data-remaining-done-toggle]');
  assert.equal(toggle.getAttribute('aria-expanded'), 'false', 'the done fold starts closed');
  assert.equal(toggle.children[1].textContent, '2 done');
  const doneBody = value.querySelector('[data-remaining-done]');
  assert.equal(doneBody.classList.contains('hidden'), true);

  toggle.onclick();
  assert.equal(toggle.getAttribute('aria-expanded'), 'true');
  assert.equal(doneBody.classList.contains('hidden'), false);
  const doneItems = viewRemainingItems(rig.view).filter((el) => el.dataset.remainingItem === 'done');
  assert.deepEqual(doneItems.map((el) => el.children[1].textContent), ['done one', 'done two'],
      'opened, the fold lists the done items in written order');
});

test('a Remaining list of all done items shows the zero count and the fold, and no open line', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {
    ...STATUS,
    remaining: [{text: 'only done', done: true}],
    remaining_updated_at: '2026-10-16T20:30:00Z',
  }});
  rig.context.switchTab('status');
  const row = viewRow(rig.view, 'remaining');
  assert.equal(row.children[1].children[0].textContent, '0 of 1 left');
  assert.equal(viewRemainingItems(rig.view).filter((el) => el.dataset.remainingItem === 'open').length, 0);
  assert.equal(row.querySelectorAll('[data-remaining-done-toggle]').length, 1);
});

test('a status without Remaining draws no Remaining row in the Status tab', async () => {
  for (const status of [STATUS, {...STATUS, remaining: []}]) {
    const rig = panelRig();
    await openSession(rig, {web_status: status});
    rig.context.switchTab('status');
    assert.equal(viewRow(rig.view, 'remaining'), undefined);
  }
});

test('the Now row carries the full text and its update time', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  rig.context.switchTab('status');
  const row = viewRow(rig.view, 'now');
  assert.equal(row.children[1].children[0].textContent, STATUS.now);
  assert.equal(row.querySelector('[data-status-updated]').dataset.statusUpdated, STATUS.updated_at);
});

test('own Waiting on you items show open Contexts; the empty list reads "Nothing"', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {
    ...STATUS,
    waiting: [{text: 'Decide the size', context: 'The sweep costs $5 an hour.', link: null}],
  }});
  rig.context.switchTab('status');
  const item = viewWaitingItems(rig.view)[0];
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
  empty.context.switchTab('status');
  assert.equal(viewRow(empty.view, 'waiting').querySelectorAll('[data-waiting-empty]').length, 1);
  assert.equal(viewWaitingItems(empty.view).length, 0);
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
  rig.context.switchTab('status');
  const body = viewWaitingItems(rig.view)[0].querySelector('[data-context-body]');
  const anchors = allElements(body).filter((el) => el.tagName === 'A');
  assert.deepEqual(anchors.map((el) => el.href), ['https://example.com/plan', '/absolute_filepath/tmp/page.html']);
});

test('an item without context renders no fold', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {...STATUS, waiting: [{text: 'Approve the deploy', link: null}]}});
  rig.context.switchTab('status');
  assert.equal(viewWaitingItems(rig.view)[0].querySelectorAll('[data-context]').length, 0);
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
  rig.context.switchTab('status');
  const row = viewRow(rig.view, 'children');
  const headers = row.querySelectorAll('[data-child-group]');
  assert.deepEqual(headers.map((el) => el.textContent),
    ['Child one · ' + CHILD_ID.slice(0, 8), 'Child two · ' + OTHER_CHILD_ID.slice(0, 8)]);
  const items = viewChildItems(rig.view);
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

test('the panel collapses its user-act items, own and descendant, into one login-sheet line', async () => {
  const rig = panelRig();
  rig.page.descendants = [{
    session_id: CHILD_ID,
    name: 'Child one',
    updated_at: '2026-10-16T20:30:00Z',
    waiting: [
      {text: 'Log into the release console', link: null, tier: 'user-act'},
      {text: 'Decide the size', link: null, tier: 't0', rule: 'money'},
    ],
  }, {
    session_id: OTHER_CHILD_ID,
    name: 'Child two',
    updated_at: '2026-10-16T20:30:00Z',
    waiting: [{text: 'Child two item', link: null, since: '2026-10-20T18:00:00Z', tier: 'user-act'}],
  }];
  await openSession(rig, {web_status: {
    ...STATUS,
    waiting: [
      {text: 'Approve the deploy', link: null},
      {text: 'Log into the admin console', link: null, tier: 'user-act'},
    ],
  }});
  rig.context.switchTab('status');
  // The one line covers own and descendant items and links to the day's login sheet.
  const sheet = viewRow(rig.view, 'login-sheet');
  assert.ok(sheet, 'the login-sheet line shows when user-act items exist');
  const text = allElements(sheet).find((el) => el.textContent === '3 actions that only you can do today');
  assert.ok(text, 'the line counts both sessions\' items, an item judged earlier included');
  const link = allElements(sheet).filter((el) => el.tagName === 'A');
  assert.deepEqual(link.map((el) => el.href),
    ['/approval?date=' + new Intl.DateTimeFormat('en-CA', {timeZone: 'America/Los_Angeles'}).format(new Date())]);
  // The user-act items stay out of the item rows; the child group whose items
  // all moved shows no header.
  assert.deepEqual(viewWaitingItems(rig.view).map((el) => el.textContent.includes('Approve the deploy')), [true]);
  assert.deepEqual(viewChildItems(rig.view).map((el) => el.textContent.includes('Decide the size')), [true]);
  assert.deepEqual(viewRow(rig.view, 'children').querySelectorAll('[data-child-group]').map((el) => el.textContent),
    ['Child one · ' + CHILD_ID.slice(0, 8)]);
  // The badge counts what the panel shows, so the user-act item stays out of it.
  assert.equal(rig.badge.textContent, '1');
});

test('a derived user-act item the server holds counts in the login-sheet line and stays out of the item rows', async () => {
  const rig = panelRig();
  rig.page.derived_waiting = [{
    session_id: OTHER_CHILD_ID,
    text: 'Approve the ssh login to synthetic-login-host on /host-auth',
    context: 'Synthetic login check',
    link: '/host-auth',
    tier: 'user-act',
    rule: null,
    since: '2026-11-01T21:43:00-07:00',
  }];
  await openSession(rig, {web_status: STATUS});
  rig.context.switchTab('status');
  const sheet = viewRow(rig.view, 'login-sheet');
  assert.ok(sheet, 'the derived user-act item shows in the login-sheet line');
  assert.ok(allElements(sheet).find((el) => el.textContent === '1 action that only you can do today'));
  assert.equal(viewWaitingItems(rig.view).some((el) => el.textContent.includes('Approve the ssh login')), false);
  // The badge counts only the session's stored waiting items.
  assert.equal(rig.badge.textContent, '2');
});

test('without user-act items the Status tab shows no login-sheet line', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  rig.context.switchTab('status');
  assert.equal(viewRow(rig.view, 'login-sheet'), undefined);
  assert.deepEqual(viewKinds(rig.view), ['goal', 'now', 'waiting']);
});

test('the child Now rows list each open child with its Now, update time and Open link', async () => {
  const rig = panelRig();
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: STATUS});
  rig.context.switchTab('status');
  const items = viewChildNowItems(rig.view);
  assert.equal(items.length, 2, 'one row per child');
  assert.equal(items[0].children[0].textContent, 'Child one · ' + CHILD_ID.slice(0, 8) + ': Waiting for the sweep');
  assert.equal(items[0].querySelectorAll('[data-status-updated]').length, 1, 'the row carries its update time');
  const link = items[0].querySelectorAll('[data-open-link]');
  assert.equal(link.length, 1);
  assert.equal(link[0].href, '/?session=' + CHILD_ID, 'the row links to its child session page');
});

test('without children rows the Status tab ends at Waiting on you', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  rig.context.switchTab('status');
  assert.deepEqual(viewKinds(rig.view), ['goal', 'now', 'waiting']);
});

test('a session with no status shows "No status yet" in the Status tab, with the child rows beside it', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: null});
  rig.context.switchTab('status');
  assert.deepEqual(viewKinds(rig.view), ['none', 'children']);
  assert.equal(viewRow(rig.view, 'none').textContent, 'No status yet');
});

test('waiting ages show for own items and for descendant items in the Status tab', async () => {
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
  rig.context.switchTab('status');
  assert.equal(viewWaitingItems(rig.view)[0].querySelector('[data-waiting-age]').textContent, '6h');
  assert.equal(viewChildItems(rig.view)[0].querySelector('[data-waiting-age]').textContent, '至少 3d');
});

test('a live web_status message redraws the badge and the shown Status tab', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w1', content: 'Status updated: Now, Waiting on you', web_status: NEWER,
      timestamp: '2026-10-16T21:00:00Z'},
  });
  assert.equal(rig.badge.textContent, '1', 'the badge follows the live status');
  rig.context.switchTab('status');
  assert.equal(viewRow(rig.view, 'now').children[1].children[0].textContent, NEWER.now);

  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w2', content: 'Status updated: Now', web_status: OLDER, timestamp: 't2'},
  });
  assert.equal(viewRow(rig.view, 'now').children[1].children[0].textContent, NEWER.now,
      'an older message does not move the Status tab back');
});

test('a web_status message from history replaces the status only when it is newer', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: NEWER}, [
    {role: 'web_status', id: 'w0', content: 'Status updated: Now', web_status: OLDER, timestamp: 't0'},
  ]);
  rig.context.switchTab('status');
  assert.equal(viewRow(rig.view, 'now').children[1].children[0].textContent, NEWER.now,
      'an older message in history does not move the view back');

  const other = panelRig();
  await openSession(other, {id: 'session-b', web_status: null}, [
    {role: 'web_status', id: 'w1', content: 'Status updated: Now', web_status: OLDER, timestamp: 't1'},
    {role: 'web_status', id: 'w2', content: 'Status updated: Now', web_status: NEWER, timestamp: 't2'},
  ]);
  other.context.switchTab('status');
  assert.equal(viewRow(other.view, 'now').children[1].children[0].textContent, NEWER.now,
      'the newest message in history sets the status');
});

test('a web_status_subtree notice triggers one GET and redraws the shown Status tab', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: STATUS});
  assert.equal(rig.statusGets.length, 1, 'the open fetches the child rows once');
  rig.context.switchTab('status');
  assert.equal(viewChildItems(rig.view).length, 1);

  rig.page.descendants = descendantRows(2, 2);
  await deliver(rig, {type: 'web_status_subtree', source_session_id: OTHER_CHILD_ID});
  assert.equal(rig.statusGets.length, 2, 'the notice triggers exactly one GET');
  assert.equal(viewChildItems(rig.view).length, 4, 'the Status tab redraws with the new rows');
});

test('a session switch resets the Status tab and draws the session it opens', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {id: 'session-a', web_status: STATUS});
  rig.context.switchTab('status');

  rig.page.descendants = [];
  await openSession(rig, {id: 'session-b', web_status: NEWER});
  assert.equal(rig.view.classList.contains('hidden'), true, 'the session switch returns the page to Chat');
  assert.equal(rig.chat.classList.contains('hidden'), false, 'and shows the Chat column');
  assert.equal(rig.badge.classList.contains('hidden'), false, 'the new session\'s badge shows');
  assert.equal(rig.badge.textContent, String(NEWER.waiting.length), 'session-a\'s badge count is gone');
  rig.context.switchTab('status');
  assert.equal(viewRow(rig.view, 'now').children[1].children[0].textContent, NEWER.now);
  assert.equal(viewChildItems(rig.view).length, 0, 'the child rows of session-a are gone');
  assert.equal(rig.statusGets.length, 2);
});

test('a thread session and a worker session show neither the Status button nor its badge', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  assert.equal(rig.button.classList.contains('hidden'), false, 'a web session shows the Status button');

  await openSession(rig, {id: 'session-b', slack_origin: {team_id: 'T1', channel_id: 'C1', thread_ts: '1.5'}});
  assert.equal(rig.button.classList.contains('hidden'), true, 'a Slack thread session shows no Status button');
  assert.equal(rig.badge.classList.contains('hidden'), true, 'and no badge');
  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w1', content: 'Status updated: Now', web_status: NEWER, timestamp: 't'},
  });
  assert.equal(rig.button.classList.contains('hidden'), true, 'a web_status message does not show the button');
  assert.equal(rig.statusGets.length, 1, 'the thread session makes no descendant GET');

  // A worker session's page renders its own transcript; the status hook takes the session alone.
  rig.context.openSessionStatusPanel({id: 'session-c', profile: 'worker'}, []);
  assert.equal(rig.button.classList.contains('hidden'), true, 'a worker session shows no Status button');
});

test('a child web session shows the Status button, and a discord thread session does not', async () => {
  const rig = panelRig();
  await openSession(rig, {id: 'child-a', task_parent_id: 'parent-1', web_status: STATUS});
  assert.equal(rig.button.classList.contains('hidden'), false, 'a child web session shows the Status button');
  await openSession(rig, {id: 'thread-d', discord_origin: {guild_id: 'G1', channel_id: 'C2', thread_id: 'th-2'}});
  assert.equal(rig.button.classList.contains('hidden'), true, 'a Discord thread session shows no Status button');
});

test('the Status tab registers beside Chat with the plan\'s values', async () => {
  const rig = panelRig();
  const tab = vm.runInContext("registeredTabs.get('status')", rig.context);
  assert.deepEqual(
    {
      showsChat: tab.showsChat,
      fullScreenOnMobile: tab.fullScreenOnMobile,
      keepOnSessionSwitch: tab.keepOnSessionSwitch,
    },
    {showsChat: false, fullScreenOnMobile: false, keepOnSessionSwitch: false});

  await openSession(rig, {web_status: STATUS});
  rig.context.switchTab('status');
  assert.equal(rig.view.classList.contains('hidden'), false, 'the Status tab shows its panel');
  assert.equal(rig.chat.classList.contains('hidden'), true, 'and takes the Chat column\'s place');
  assert.equal(rig.context.tabAfterSessionSwitch(), 'chat', 'the next session opens on Chat');
});

test('a web_status message renders as a system line in Everything', () => {
  const rig = panelRig();
  const fields = {id: 'x1', content: 'Status updated: Now', timestamp: '2026-10-16T21:00:00Z'};
  const status = rig.context.renderMessage({role: 'web_status', ...fields}, 'session-a');
  const system = rig.context.renderMessage({role: 'system', ...fields}, 'session-a');
  assert.equal(status, system.replace('data-message-role="system"', 'data-message-role="web_status"'));
});

test('a phone Status tab stacks each label above its value, with no label column', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  await openSession(rig, {web_status: STATUS});
  rig.context.switchTab('status');
  assert.equal(allElements(rig.view).filter((el) => el.classList.contains('w-28')).length, 0);
  assert.equal(viewRow(rig.view, 'goal').classList.contains('flex-col'), true);
  assert.equal(viewWaitingItems(rig.view).length, 2);
});

test('a platform.onChange switch redraws the shown Status tab in the new layout', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  rig.context.switchTab('status');
  assert.equal(viewRow(rig.view, 'goal').classList.contains('flex-col'), false, 'desktop keeps a label column');

  const view = rig.view;
  switchLayout(rig, 'mobile');
  assert.equal(rig.view, view, 'the view element is the same');
  assert.equal(viewRow(rig.view, 'goal').classList.contains('flex-col'), true,
      'a phone stacks each label above its value');

  switchLayout(rig, 'desktop');
  assert.equal(viewRow(rig.view, 'goal').classList.contains('flex-col'), false, 'back to the label column');
});
