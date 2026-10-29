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

// ---------------------------------------------------------------------------
// The session status surfaces (chat/status-panel.js) on a page of the real chat,
// sidebar and WebSocket modules. The summary strip (#chat-status-panel) and the
// full Status view (#chat-status-view) draw from one status state. Rows are read
// back by their data attributes, so a case asserts structure and the values the
// panel carries, not the labels. The phone layout comes from a platform stub
// that switchLayout drives the way platform.js does: a mode change runs each
// onChange callback.
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
  const panel = new FakeElement('DIV', {id: 'chat-status-panel', className: 'hidden'});
  const view = new FakeElement('DIV', {id: 'chat-status-view', className: 'hidden'});
  const elements = new Map([
    ['messages', root], ['streaming-msg', stream], ['chat-status-panel', panel], ['chat-status-view', view],
  ]);
  const page = {web_status: null, descendants: [], children: []};
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
  return {context, panel, view, page, statusGets, layout, FakeWebSocket: context.WebSocket};
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

// The strip's rows, in order: a row names its kind.
function stripKinds(panel) {
  return panel.children.map((el) => el.dataset.statusRow);
}

function stripRow(panel, kind) {
  return panel.children.find((el) => el.dataset.statusRow === kind);
}

// The strip's line 1.
function summaryLine(panel) {
  return stripRow(panel, 'summary');
}

// Line 1's segments without the dot separators.
function summarySegments(line) {
  return line.children.filter((el) => el.textContent !== '·');
}

// The Status view's rows, in order.
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

test('the strip line 1 holds the waiting count, the counts, the Now text and the Status link in order', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(2, 1);
  await openSession(rig, {web_status: WITH_REMAINING});
  assert.equal(rig.panel.classList.contains('hidden'), false);
  assert.deepEqual(stripKinds(rig.panel), ['summary', 'waiting-item', 'waiting-item']);

  const segments = summarySegments(summaryLine(rig.panel)).map((el) => el.textContent);
  assert.deepEqual(segments, [
    'Waiting on you · 2',
    'In child sessions · 3',
    '1 of 2 left',
    'Now: ' + WITH_REMAINING.now,
    'Status ›',
  ], 'the segments read in order');

  const line = summaryLine(rig.panel);
  assert.equal(line.children[0].dataset.waitingCount, '2');
  assert.equal(line.querySelector('[data-child-count]').dataset.childCount, '3');
  assert.equal(line.querySelector('[data-remaining-count]').dataset.remainingCount, '1');
  assert.equal(line.querySelector('[data-now-text]').dataset.nowText, WITH_REMAINING.now);
  assert.equal(line.querySelectorAll('[data-status-view-link]').length, 1, 'line 1 ends with one Status link');
});

test('an empty waiting list reads "Nothing" and a status without Remaining carries no count', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {...STATUS, waiting: []}});
  const line = summaryLine(rig.panel);
  assert.equal(line.children[0].textContent, 'Waiting on you · Nothing');
  assert.equal(line.children[0].dataset.waitingCount, '0');
  assert.equal(line.querySelectorAll('[data-remaining-count]').length, 0);
  assert.equal(line.querySelectorAll('[data-child-count]').length, 0);
});

test('the Now segment drops out when the status has no Now text', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {...STATUS, now: ''}});
  const segments = summarySegments(summaryLine(rig.panel)).map((el) => el.textContent);
  assert.deepEqual(segments, ['Waiting on you · 2', 'Status ›']);
});

test('three own items draw three item lines under line 1, four lines in all', async () => {
  const rig = panelRig();
  const items = [1, 2, 3].map((n) => ({text: 'item ' + n, link: n === 1 ? 'https://example.com/a' : null}));
  await openSession(rig, {web_status: {...STATUS, waiting: items}});
  assert.deepEqual(stripKinds(rig.panel), ['summary', 'waiting-item', 'waiting-item', 'waiting-item']);
  assert.equal(stripRow(rig.panel, 'waiting-item').textContent.includes('item 1'), true);
  assert.equal(stripRow(rig.panel, 'waiting-item').querySelectorAll('[data-open-link]').length, 1,
      'a safe link ends the item line');
  assert.equal(rig.panel.querySelectorAll('[data-more-count]').length, 0, 'three items need no more line');
});

test('five own items draw the first two and a "+3 more" line that opens the Status view', async () => {
  const rig = panelRig();
  const items = [1, 2, 3, 4, 5].map((n) => ({text: 'item ' + n, link: null}));
  await openSession(rig, {web_status: {...STATUS, waiting: items}});
  assert.deepEqual(stripKinds(rig.panel), ['summary', 'waiting-item', 'waiting-item', undefined],
      'two item lines and the more line');
  const more = rig.panel.children[3];
  assert.equal(more.dataset.moreCount, '3');
  assert.equal(more.textContent, '+3 more');

  more.onclick();
  assert.equal(rig.panel.classList.contains('hidden'), true, 'the more line selects the Status view, so the strip hides');
  assert.equal(rig.view.classList.contains('hidden'), false, 'the Status view shows');
});

test('an item line cuts its text to one line, carries the waited age and renders the text literally', async () => {
  const rig = panelRig();
  const since = new Date(Date.now() - 6.5 * 60 * 60 * 1000).toISOString();
  const text = 'Retire <b>old</b> server & keep the log';
  await openSession(rig, {web_status: {...STATUS, waiting: [{text, link: null, since, since_exact: true}]}});
  const line = stripRow(rig.panel, 'waiting-item');
  assert.equal(line.children[0].textContent, text, 'the text goes in through textContent');
  assert.equal(line.children[0].classList.contains('truncate'), true, 'the text cuts to one line');
  assert.equal(line.querySelector('[data-waiting-age]').textContent, '6h');
});

test('an item whose link is not a web or host-absolute URL gets no link', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {...STATUS, waiting: [{text: 'Run it', link: 'javascript:alert(1)'}]}});
  assert.equal(stripRow(rig.panel, 'waiting-item').querySelectorAll('[data-open-link]').length, 0);
});

test('a phone strip is one line with the counts and the Status link, without the Now text or item lines', async () => {
  const rig = panelRig();
  switchLayout(rig, 'mobile');
  rig.page.descendants = descendantRows(2, 1);
  await openSession(rig, {web_status: WITH_REMAINING});
  assert.deepEqual(stripKinds(rig.panel), ['summary']);
  const line = summaryLine(rig.panel);
  const segments = summarySegments(line).map((el) => el.textContent);
  assert.deepEqual(segments, ['Waiting on you · 2', 'In child sessions · 3', '1 of 2 left', 'Status ›']);
  assert.equal(line.querySelectorAll('[data-now-text]').length, 0, 'a phone leaves out the Now text');
});

test('a session with no status shows "No status yet" on both platforms', async () => {
  for (const mode of ['desktop', 'mobile']) {
    const rig = panelRig();
    switchLayout(rig, mode);
    await openSession(rig, {web_status: null});
    assert.deepEqual(stripKinds(rig.panel), ['none']);
    assert.equal(stripRow(rig.panel, 'none').textContent, 'No status yet');
  }
});

test('the strip has no Goal row, no context fold and no height cap', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: {...STATUS, waiting: [{text: 'Decide', context: 'Why now.', link: null}]}});
  assert.equal(stripRow(rig.panel, 'goal'), undefined);
  assert.equal(rig.panel.querySelectorAll('[data-context]').length, 0);
  assert.equal(rig.panel.classList.contains('max-h-48'), false);
  assert.equal(rig.panel.classList.contains('overflow-y-auto'), false);
});

test('the Status view shows the Goal, Remaining, Now, Waiting on you, children and child Now rows in order', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(2, 1);
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: WITH_REMAINING});
  rig.context.setChatView('status');
  assert.equal(rig.view.classList.contains('hidden'), false);
  assert.deepEqual(viewKinds(rig.view), ['goal', 'remaining', 'now', 'waiting', 'children', 'children-now']);
  assert.equal(viewRow(rig.view, 'goal').children[1].textContent, WITH_REMAINING.goal);
  assert.equal(rig.panel.classList.contains('hidden'), true, 'the strip hides while the Status view shows');
});

test('selecting Status through the toggle holds for the session across a re-open', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  rig.context.setChatView('status');
  assert.equal(rig.view.classList.contains('hidden'), false);

  await openSession(rig, {id: 'session-b', web_status: STATUS});
  assert.equal(rig.view.classList.contains('hidden'), true, 'another session keeps its default, the strip');

  await openSession(rig, {web_status: STATUS});
  assert.equal(rig.view.classList.contains('hidden'), false, 'the first session re-opens in its Status choice');
});

test('the Status view lists open Remaining items first and folds the done ones closed by default', async () => {
  const rig = panelRig();
  const remaining = [
    {text: 'open one', done: false},
    {text: 'done one', done: true},
    {text: 'open two', done: false},
    {text: 'done two', done: true},
  ];
  await openSession(rig, {web_status: {...STATUS, remaining,
    remaining_updated_at: WITH_REMAINING.remaining_updated_at}});
  rig.context.setChatView('status');
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
  rig.context.setChatView('status');
  const row = viewRow(rig.view, 'remaining');
  assert.equal(row.children[1].children[0].textContent, '0 of 1 left');
  assert.equal(viewRemainingItems(rig.view).filter((el) => el.dataset.remainingItem === 'open').length, 0);
  assert.equal(row.querySelectorAll('[data-remaining-done-toggle]').length, 1);
});

test('a status without Remaining draws no Remaining row in the Status view', async () => {
  for (const status of [STATUS, {...STATUS, remaining: []}]) {
    const rig = panelRig();
    await openSession(rig, {web_status: status});
    rig.context.setChatView('status');
    assert.equal(viewRow(rig.view, 'remaining'), undefined);
  }
});

test('the Now row carries the full text and its update time', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  rig.context.setChatView('status');
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
  rig.context.setChatView('status');
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
  empty.context.setChatView('status');
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
  rig.context.setChatView('status');
  const body = viewWaitingItems(rig.view)[0].querySelector('[data-context-body]');
  const anchors = allElements(body).filter((el) => el.tagName === 'A');
  assert.deepEqual(anchors.map((el) => el.href), ['https://example.com/plan', '/absolute_filepath/tmp/page.html']);
});

test('an item without context renders no fold', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: {...STATUS, waiting: [{text: 'Approve the deploy', link: null}]}});
  rig.context.setChatView('status');
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
  rig.context.setChatView('status');
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

test('the child Now rows list each open child with its Now, update time and Open link', async () => {
  const rig = panelRig();
  rig.page.children = NOW_CHILDREN;
  await openSession(rig, {web_status: STATUS});
  rig.context.setChatView('status');
  const items = viewChildNowItems(rig.view);
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
  rig.context.setChatView('status');
  assert.deepEqual(viewKinds(rig.view), ['goal', 'now', 'waiting']);
});

test('a session with no status shows "No status yet" in the Status view, with the child rows beside it', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: null});
  rig.context.setChatView('status');
  assert.deepEqual(viewKinds(rig.view), ['none', 'children']);
  assert.equal(viewRow(rig.view, 'none').textContent, 'No status yet');
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
  rig.context.setChatView('status');
  assert.equal(viewWaitingItems(rig.view)[0].querySelector('[data-waiting-age]').textContent, '6h');
  assert.equal(viewChildItems(rig.view)[0].querySelector('[data-waiting-age]').textContent, '至少 3d');
});

test('the strip hides while the Status view shows and returns after the switch back', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  rig.context.setChatView('everything');
  assert.equal(rig.panel.classList.contains('hidden'), false, 'Everything shows the strip');
  rig.context.setChatView('status');
  assert.equal(rig.panel.classList.contains('hidden'), true);
  assert.equal(rig.view.classList.contains('hidden'), false);
  rig.context.setChatView('foryou');
  assert.equal(rig.panel.classList.contains('hidden'), false, 'For you shows the strip again');
  assert.equal(rig.view.classList.contains('hidden'), true);
});

test('a live web_status message redraws the strip and the shown Status view', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w1', content: 'Status updated: Now, Waiting on you', web_status: NEWER,
      timestamp: '2026-10-16T21:00:00Z'},
  });
  assert.equal(summaryLine(rig.panel).querySelector('[data-now-text]').dataset.nowText, NEWER.now);
  assert.equal(rig.panel.querySelectorAll('[data-waiting-item]').length, 1);

  rig.context.setChatView('status');
  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w2', content: 'Status updated: Now', web_status: OLDER, timestamp: 't2'},
  });
  assert.equal(viewRow(rig.view, 'now').children[1].children[0].textContent, NEWER.now,
      'an older message does not move the Status view back');
});

test('a web_status message from history replaces the status only when it is newer', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: NEWER}, [
    {role: 'web_status', id: 'w0', content: 'Status updated: Now', web_status: OLDER, timestamp: 't0'},
  ]);
  assert.equal(summaryLine(rig.panel).querySelector('[data-now-text]').dataset.nowText, NEWER.now,
      'an older message in history does not move the panel back');

  await openSession(rig, {id: 'session-b', web_status: null}, [
    {role: 'web_status', id: 'w1', content: 'Status updated: Now', web_status: OLDER, timestamp: 't1'},
    {role: 'web_status', id: 'w2', content: 'Status updated: Now', web_status: NEWER, timestamp: 't2'},
  ]);
  assert.equal(summaryLine(rig.panel).querySelector('[data-now-text]').dataset.nowText, NEWER.now,
      'the newest message in history sets the status');
});

test('a web_status_subtree notice triggers one GET and redraws the strip count and the Status view', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {web_status: STATUS});
  assert.equal(rig.statusGets.length, 1, 'the open fetches the child rows once');
  assert.equal(summaryLine(rig.panel).querySelector('[data-child-count]').dataset.childCount, '1');

  rig.page.descendants = descendantRows(2, 2);
  await deliver(rig, {type: 'web_status_subtree', source_session_id: OTHER_CHILD_ID});
  assert.equal(rig.statusGets.length, 2, 'the notice triggers exactly one GET');
  assert.equal(summaryLine(rig.panel).querySelector('[data-child-count]').dataset.childCount, '4');

  rig.context.setChatView('status');
  assert.equal(viewChildItems(rig.view).length, 4, 'the Status view redraws with the new rows');
});

test('a session switch redraws the strip for the session it opens and resets the Status view', async () => {
  const rig = panelRig();
  rig.page.descendants = descendantRows(1);
  await openSession(rig, {id: 'session-a', web_status: STATUS});
  assert.equal(summaryLine(rig.panel).querySelector('[data-now-text]').dataset.nowText, STATUS.now);

  rig.context.setChatView('status');
  rig.page.descendants = [];
  await openSession(rig, {id: 'session-b', web_status: NEWER});
  assert.equal(rig.view.classList.contains('hidden'), true, 'the Status view resets to the strip');
  assert.equal(summaryLine(rig.panel).querySelector('[data-now-text]').dataset.nowText, NEWER.now);
  assert.equal(rig.panel.querySelectorAll('[data-child-count]').length, 0, 'the child count of session-a is gone');
  assert.equal(rig.statusGets.length, 2);
});

test('a thread session keeps both surfaces hidden when a web_status message lands', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  await openSession(rig, {id: 'session-b', slack_origin: {team_id: 'T1', channel_id: 'C1', thread_ts: '1.5'}});
  assert.equal(rig.panel.classList.contains('hidden'), true);
  assert.equal(rig.view.classList.contains('hidden'), true);
  await deliver(rig, {
    type: 'message',
    message: {role: 'web_status', id: 'w1', content: 'Status updated: Now', web_status: NEWER, timestamp: 't'},
  });
  assert.equal(rig.panel.classList.contains('hidden'), true, 'a thread session keeps no strip');
  assert.equal(rig.statusGets.length, 1, 'and makes no descendant GET');
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
  rig.context.setChatView('status');
  assert.equal(allElements(rig.view).filter((el) => el.classList.contains('w-28')).length, 0);
  assert.equal(viewRow(rig.view, 'goal').classList.contains('flex-col'), true);
  assert.equal(viewWaitingItems(rig.view).length, 2);
});

test('a platform.onChange switch redraws both surfaces in the new layout', async () => {
  const rig = panelRig();
  await openSession(rig, {web_status: STATUS});
  assert.deepEqual(stripKinds(rig.panel), ['summary', 'waiting-item', 'waiting-item']);

  const panel = rig.panel;
  switchLayout(rig, 'mobile');
  assert.equal(rig.panel, panel, 'the panel element is the same');
  assert.deepEqual(stripKinds(rig.panel), ['summary']);

  switchLayout(rig, 'desktop');
  assert.deepEqual(stripKinds(rig.panel), ['summary', 'waiting-item', 'waiting-item']);
});
