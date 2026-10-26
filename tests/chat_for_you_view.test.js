const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {
  ELEMENT_NODE,
  FakeElement,
  fakeEngineNode,
  makeEngineTimers,
  installEngineTimers,
  installScrollTopClamp,
  scrollTo,
  settle,
  eMsg,
} = require('./turn_engine_harness');
const {readStatic, chatModules, runStaticModules} = require('./read_static');

// ---------------------------------------------------------------------------
// The For-you projection of the turn engine, on the same fake DOM as the
// Everything suites. Rows are read back by role, so a case asserts structure,
// not wording. The page's markdown-renderer.js loads with a stub marked, which
// its load never calls.
// ---------------------------------------------------------------------------

// The page's script order for these modules: markdown-renderer.js first, then
// the chat modules. The message rows render through its prose rules.
function loadPageModules(context) {
  vm.createContext(context);
  vm.runInContext(readStatic('markdown-renderer.js'), context, {filename: 'markdown-renderer.js'});
  runStaticModules(context, chatModules());
}

function forYouRig(
    messages, {view = 'foryou', clientHeight = 900, profile = null, workerSummary = null, hasMore = false} = {}) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = clientHeight;
  const sentinel = hasMore ? new FakeElement('DIV', {id: 'load-more-sentinel'}) : null;
  if (sentinel) root.appendChild(sentinel);
  const stream = new FakeElement('DIV', {id: 'streaming-msg'});
  root.appendChild(stream);
  const timers = makeEngineTimers();
  const context = {
    CSS: {escape: (value) => String(value)},
    SESSION_ID: 'sess-fy',
    masterThinking: false,
    console: {error() {}, log() {}},
    marked: {Renderer: function Renderer() {}, use() {}},
    window: {addEventListener() {}},
    platform: {isMobile: false, onChange() {}},
    fetch: async () => ({ok: false, status: 500, json: async () => ({})}),
    document: {
      addEventListener() {},
      createElement: (tag) => new FakeElement(tag),
      createTreeWalker: () => ({}),
      querySelectorAll: () => [],
      getElementById(id) {
        if (id === 'messages') return root;
        if (id === 'streaming-msg') return stream;
        if (id === 'load-more-sentinel') return sentinel && sentinel.parentElement ? sentinel : null;
        return null;
      },
    },
  };
  loadPageModules(context);
  if (profile) context.Chat.ForYou.setSession({profile});
  if (workerSummary) context.Chat.applyWorkerSummary(workerSummary);
  installEngineTimers(context, timers);
  context.Chat.buildTurnEngineMessageNode = fakeEngineNode;
  const engine = context.Chat.TurnEngine.mountIfAvailable(root, messages, 'sess-fy', {view});
  assert.ok(engine && engine.alive, 'the engine mounts the chat container');
  // A worker page hands its sentinel to the engine after the mount, as
  // session-view.js renderSessionView does.
  if (sentinel && profile === 'worker') engine.holdWorkerSentinel(sentinel);
  return {context, root, stream, sentinel, timers, engine};
}

test('worker For you holds one summary card and no transcript or empty-state rows', () => {
  const rig = forYouRig(fixture(), {
    profile: 'worker',
    hasMore: true,
    workerSummary: {task_state: 'open', runs: [], closing: {work: null, review: null}, links: {}},
  });
  const contentNodes = rig.root.children.filter((child) => child !== rig.engine.forYouHolderNode());

  assert.equal(contentNodes.length, 1);
  assert.match(contentNodes[0].className, /worker-summary/);
  assert.equal(contentNodes[0].classList.contains('for-you-empty'), false);
  assert.equal(contentNodes.some((child) => child.dataset.messageRole), false);
  assert.equal(rig.root.children.includes(rig.sentinel), false);

  rig.engine.setView('everything');
  assert.equal(rig.root.children[0], rig.sentinel, 'Everything restores the older-message sentinel');
});

// One row per child of the list: the role of a message, `empty` for the empty-state
// row, and `holder` and `sentinel` for the structural rows. A row no case expects
// reads as undefined.
function rowsOf(root) {
  return root.children.map((el) => {
    if (el.classList.contains('for-you-empty')) return 'empty';
    if (el.classList.contains('hidden')) return 'holder';
    if (el.id === 'load-more-sentinel') return 'sentinel';
    return el.dataset.messageRole;
  });
}

// The fixture the cases read: a first turn with a web reply mid-steps, a notice
// after it, a second turn with a status block, an error row, a plain system line
// and a child report behind the last separator.
function fixture() {
  return [
    eMsg('user', 'u1', 'first question'),
    eMsg('assistant', 'a1', 'looking into it'),
    eMsg('web_reply', 'r1', 'Found the cause.'),
    eMsg('assistant', 'a2', 'writing the fix'),
    eMsg('separator', 's1', '', {thinking_seconds: 30, event_index: 101}),
    eMsg('web_reply_notice', 'n1', 'No reply was posted for this message.'),
    eMsg('user', 'u2', 'second question'),
    eMsg('assistant', 'a3', '**Goal**: fix it\n**Now**: patching the parser\n**Waiting on you**: nothing yet'),
    eMsg('system', 'e1', 'Error: backend refused the launch'),
    eMsg('system', 'o1', 'Backend switched: one to two'),
    eMsg('child_report', 'c1', 'child finished'),
    eMsg('separator', 's2', '', {thinking_seconds: 9, event_index: 102}),
  ];
}

// The shown rows of the fixture: the user's messages, the reply, the notice and the error row.
const FIXTURE_ROWS = ['user', 'web_reply', 'web_reply_notice', 'user', 'system', 'holder'];

test('For you shows only the user, web reply, notice and error rows, in order', () => {
  const rig = forYouRig(fixture());
  assert.deepEqual(rowsOf(rig.root), FIXTURE_ROWS);
});

test('For you shows an error row with every line of its text', () => {
  const text = "Error: Agent error: Usage: charlie-code [OPTIONS]\n"
      + "Try 'charlie-code --help' for help.\n\n"
      + "Error: Invalid value: --image: '/tmp/example.jpg' is 5000000 bytes";
  const rig = forYouRig([eMsg('user', 'u1', 'attach the photo'), eMsg('system', 'e2', text)]);
  const row = rig.root.children.find((el) => el.dataset.messageRole === 'system');
  assert.ok(row, 'the error row is shown');
  assert.equal(row.textContent, text, 'the row keeps the whole text, line breaks included');
});

test('a web reply mid-turn shows in For you and sits inside the folded steps in Everything', () => {
  const rig = forYouRig(fixture());
  assert.ok(rowsOf(rig.root).includes('web_reply'), 'For you shows the reply');

  const everything = forYouRig(fixture(), {view: 'everything'});
  everything.engine.setOverride('u1|a2|s1', true);
  settle(everything.timers);
  const reply = everything.root.querySelectorAll('[data-message-id="r1"]');
  assert.equal(reply.length, 1, 'the reply is in the Everything DOM');
  const steps = reply[0].parentElement;
  assert.ok(steps.classList.contains('turn-fold-content'), 'the reply sits in the steps region');
  assert.ok(steps.classList.contains('hidden'), 'the steps region is folded');
});

test('no fold row shows for any mix of entries', () => {
  const work = [
    eMsg('assistant', 'x1', 'looking'),
    eMsg('separator', 'x2', '', {thinking_seconds: 4, event_index: 201}),
  ];
  const status = [
    eMsg('web_status', 'x3', 'Status updated: Now'),
    eMsg('child_report', 'x4', 'child finished'),
  ];
  const notes = [
    eMsg('system', 'x5', 'Backend switched: one to two'),
    eMsg('assistant', 'x6', 'more'),
  ];
  const turn = [
    eMsg('user', 'x7', 'hi'),
    eMsg('assistant', 'x8', 'working'),
    eMsg('separator', 'x9', '', {thinking_seconds: 2, event_index: 202}),
    eMsg('web_reply', 'x10', 'done'),
  ];
  assert.deepEqual(rowsOf(forYouRig(work).root), ['empty', 'holder']);
  assert.deepEqual(rowsOf(forYouRig(status).root), ['empty', 'holder']);
  assert.deepEqual(rowsOf(forYouRig(notes).root), ['empty', 'holder']);
  assert.deepEqual(rowsOf(forYouRig(turn).root), ['user', 'web_reply', 'holder']);
  assert.deepEqual(rowsOf(forYouRig([...work, ...turn, ...status]).root), ['user', 'web_reply', 'holder']);
});

test('the empty state is one button row when no message shows', () => {
  const rig = forYouRig([
    eMsg('assistant', 'a1', 'only work'),
    eMsg('separator', 's1', '', {thinking_seconds: 3, event_index: 1}),
  ]);
  assert.deepEqual(rowsOf(rig.root), ['empty', 'holder']);
  assert.equal(rig.root.children[0].tagName, 'BUTTON');
});

test('a click on the empty-state row switches to Everything, and the switch holds', () => {
  const rig = forYouRig([eMsg('assistant', 'a1', 'only work')]);
  rig.root.children[0].onclick();
  assert.equal(rig.engine.view, 'everything');
  assert.equal(rig.context.Chat.ForYou.viewFor({id: 'sess-fy'}), 'everything');
});

test('an older page lands above the shown rows', () => {
  const rig = forYouRig(fixture());
  rig.engine.prependMessages([
    eMsg('user', 'p0', 'older question'),
    eMsg('assistant', 'pa', '**Goal**: old\n**Now**: ancient\n**Waiting on you**: none'),
    eMsg('separator', 'ps', '', {thinking_seconds: 5, event_index: 99}),
  ]);
  settle(rig.timers);
  assert.deepEqual(rowsOf(rig.root), [
    'user', 'user', 'web_reply', 'web_reply_notice', 'user', 'system', 'holder',
  ]);
});

test('a web status message adds no row to For you', () => {
  const rig = forYouRig(fixture());
  rig.context.appendMessageObject(eMsg('web_status', 'w1', 'Status updated: Now'), 'sess-fy', false);
  settle(rig.timers);
  assert.deepEqual(rowsOf(rig.root), FIXTURE_ROWS);
});

test('a reader scrolled into history keeps their message in place when an older page lands', () => {
  const rig = forYouRig(fixture(), {clientHeight: 60});
  installScrollTopClamp(rig.root);
  scrollTo(rig.timers, rig.root, 0);
  assert.equal(rig.engine.pinnedIntent, false, 'an upward scroll unpins the reader');
  const placeOf = (id) => {
    const el = rig.root.querySelector(`[data-message-id="${id}"]`);
    return el.getBoundingClientRect().top - rig.root.getBoundingClientRect().top;
  };
  const before = placeOf('u1');

  rig.engine.prependMessages([
    eMsg('user', 'p0', 'older question'),
    eMsg('assistant', 'pa', 'older answer'),
    eMsg('separator', 'ps', '', {thinking_seconds: 5, event_index: 99}),
  ]);
  settle(rig.timers);
  assert.equal(placeOf('u1'), before, 'u1 keeps its place in the viewport');
});

test('a reader at the bottom follows a new web reply', () => {
  const rig = forYouRig(fixture(), {clientHeight: 60});
  installScrollTopClamp(rig.root);
  rig.context.appendMessageObject(eMsg('web_reply', 'r2', 'Deployed.'), 'sess-fy', false);
  settle(rig.timers);
  assert.equal(rig.engine.pinnedIntent, true);
  assert.equal(rig.root.scrollHeight - rig.root.scrollTop - rig.root.clientHeight <= 1, true,
      'the view sits at the bottom, where the reply is');
});

test('a live web reply lands at the bottom of For you', () => {
  const rig = forYouRig(fixture());
  rig.context.appendMessageObject(eMsg('web_reply', 'r2', 'Deployed.'), 'sess-fy', false);
  settle(rig.timers);
  assert.deepEqual(rowsOf(rig.root).slice(-3), ['system', 'web_reply', 'holder']);
});

test('no Working row shows while a turn runs, and the stream output stays in the hidden holder', () => {
  const rig = forYouRig(fixture());
  rig.context.masterThinking = true;
  rig.context.appendMessageObject(eMsg('assistant', 'a4', 'still going'), 'sess-fy', false);
  settle(rig.timers);
  assert.deepEqual(rowsOf(rig.root), FIXTURE_ROWS, 'the running turn adds no row');
  const holder = rig.root.children[rig.root.children.length - 1];
  assert.equal(rig.stream.parentElement, holder, 'the stream output sits in the hidden holder');
});

test('a reprojection with nothing to move keeps every row element', () => {
  const rig = forYouRig(fixture());
  const before = rig.root.children.slice();
  rig.engine.reproject('resize');
  const after = rig.root.children;
  assert.equal(after.length, before.length);
  after.forEach((el, i) => assert.equal(el, before[i], `row ${i} is the same element`));
});

test('Everything is unchanged after a For you round trip', () => {
  const rig = forYouRig(fixture(), {view: 'everything'});
  rig.engine.setOverride('u1|a2|s1', true);
  rig.context.appendMessageObject(eMsg('web_status', 'w1', 'Status updated: Now'), 'sess-fy', false);
  settle(rig.timers);
  const snapshot = () => domSnapshot(rig.root);
  const registry = () => JSON.stringify([...rig.engine.registry].map(([key, state]) => [
    key, state.override, state.nStepsExpanded,
  ]));
  const before = snapshot();
  const registryBefore = registry();

  rig.engine.setView('foryou');
  settle(rig.timers);
  assert.deepEqual(rowsOf(rig.root), FIXTURE_ROWS);
  rig.engine.setView('everything');
  settle(rig.timers);

  assert.equal(snapshot(), before, 'the Everything DOM is the same');
  assert.equal(registry(), registryBefore, 'the turn-fold state is the same');
});

// One synthetic turn: a user message, its reply and the separator that closes it.
function userTurn(tag) {
  return [
    eMsg('user', `${tag}-u`, `question ${tag}`),
    eMsg('assistant', `${tag}-a`, `answer ${tag}`),
    eMsg('separator', `${tag}-s`, '', {thinking_seconds: 4}),
  ];
}

// The user messages a projection shows, in document order.
function userIdsIn(el) {
  const ids = [];
  (function walk(node) {
    node.children.forEach((child) => {
      if (child.dataset && child.dataset.messageRole === 'user') ids.push(child.dataset.messageId);
      walk(child);
    });
  })(el);
  return ids;
}

// The message ids the list shows, in document order.
function shownIds(el) {
  return el.querySelectorAll('[data-message-id]').map((node) => node.dataset.messageId);
}

// The rendered user rows must read in server order. Everything shows messages
// inside turns, so every turn opens before that read; For you renders the rows
// it shows directly.
function assertTimeOrder(rig, expected) {
  if (rig.engine.view === 'everything') {
    rig.engine.segments.forEach((seg) => {
      if (seg.kind === 'turn') rig.engine.setOverride(seg.key, true);
    });
    settle(rig.timers);
  }
  const expectedUsers = expected.filter((id) => id.endsWith('-u'));
  assert.deepEqual(userIdsIn(rig.root), expectedUsers, `the ${rig.engine.view} rows read in server order`);
}

function turnMessageIds(tags) {
  return tags.flatMap((tag) => [`${tag}-u`, `${tag}-a`, `${tag}-s`]);
}

test('one older page prepended in For you keeps user rows in time order across a round trip', () => {
  const rig = forYouRig([...userTurn('n1'), ...userTurn('n2')]);
  rig.engine.prependMessages(userTurn('p1'));
  settle(rig.timers);
  const order = turnMessageIds(['p1', 'n1', 'n2']);
  assertTimeOrder(rig, order);

  rig.engine.setView('everything');
  settle(rig.timers);
  assertTimeOrder(rig, order);

  rig.engine.setView('foryou');
  settle(rig.timers);
  assertTimeOrder(rig, order);
});

test('three older pages prepended in For you leave the newest messages last in Everything', () => {
  const rig = forYouRig([...userTurn('n1'), ...userTurn('n2')]);
  rig.engine.prependMessages(userTurn('p1'));
  rig.engine.prependMessages(userTurn('p2'));
  rig.engine.prependMessages(userTurn('p3'));
  settle(rig.timers);
  const order = turnMessageIds(['p3', 'p2', 'p1', 'n1', 'n2']);
  assertTimeOrder(rig, order);

  rig.engine.setView('everything');
  settle(rig.timers);
  assertTimeOrder(rig, order);

  rig.engine.setView('foryou');
  settle(rig.timers);
  assertTimeOrder(rig, order);
});

test('a live turn appended after the view switches lands last in both views', () => {
  const rig = forYouRig([...userTurn('n1'), ...userTurn('n2')]);
  rig.engine.prependMessages(userTurn('p1'));
  rig.engine.setView('everything');
  rig.engine.setView('foryou');
  settle(rig.timers);
  userTurn('live').forEach((msg) => rig.context.appendMessageObject(msg, 'sess-fy', false));
  settle(rig.timers);
  const order = turnMessageIds(['p1', 'n1', 'n2', 'live']);
  assertTimeOrder(rig, order);

  rig.engine.setView('everything');
  settle(rig.timers);
  assertTimeOrder(rig, order);
});

// A turn whose user message arrives after the turn's first assistant message
// and before a later web reply. The shared span rule carries that input into
// the next turn, but the server sent it above the reply; For you reads the
// store's server order, so the reply never rises above it.
function midTurnMessages() {
  return [
    eMsg('user', 'u1', 'first question'),
    eMsg('assistant', 'a1', 'looking into it'),
    eMsg('user', 'mid', 'also check the logs'),
    eMsg('web_reply', 'r1', 'Found the cause.'),
    eMsg('assistant', 'a2', 'writing the fix'),
  ];
}

function midTurnSeparator() {
  return eMsg('separator', 's1', '', {thinking_seconds: 30, event_index: 101});
}

// Server order: the mid-turn input sits above the web reply that followed it.
const MID_TURN_ROWS = ['u1', 'mid', 'r1'];

test('a mid-turn input shows above the later web reply after mount', () => {
  const rig = forYouRig([...midTurnMessages(), midTurnSeparator()]);
  assert.deepEqual(shownIds(rig.root), MID_TURN_ROWS);
});

test('a mid-turn input stays above the later web reply when the running turn closes live', () => {
  const rig = forYouRig(midTurnMessages());
  assert.deepEqual(shownIds(rig.root), MID_TURN_ROWS, 'the pending turn shows server order');
  rig.engine.appendMessage(midTurnSeparator(), false);
  settle(rig.timers);
  assert.deepEqual(shownIds(rig.root), MID_TURN_ROWS, 'closing the turn keeps server order');
});

test('a mid-turn input stays above the later web reply when a later turn closes behind it', () => {
  const rig = forYouRig([...midTurnMessages(), midTurnSeparator()]);
  rig.engine.appendMessage(eMsg('assistant', 'a3', 'handling it'), false);
  rig.engine.appendMessage(eMsg('assistant', 'a4', 'handled'), false);
  rig.engine.appendMessage(eMsg('separator', 's2', '', {thinking_seconds: 5, event_index: 102}), false);
  settle(rig.timers);
  assert.deepEqual(shownIds(rig.root), MID_TURN_ROWS);
});

// The subtree's tags, classes, data attributes, text and spacer heights in order:
// two equal snapshots mean two equal DOMs for the cases above.
function domSnapshot(el) {
  const kids = el.childNodes.map((node) => (node.nodeType === ELEMENT_NODE
    ? domSnapshot(node)
    : `#text(${node.textContent})`));
  const style = el.style && el.style.height ? ` h=${el.style.height}` : '';
  return `<${el.tagName} class="${el.className}" data=${JSON.stringify(el.dataset)}${style}>${kids.join('')}`
    + `</${el.tagName}>`;
}
