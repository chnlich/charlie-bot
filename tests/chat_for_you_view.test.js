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
// Everything suites. Rows are read back by role, fold count and state, so a
// case asserts structure, not wording. The status-block rule loads from the
// page's markdown-renderer.js with a stub marked, which its load never calls.
// ---------------------------------------------------------------------------

// The page's script order for these modules: markdown-renderer.js first (the
// progress strip reads its status-block labels), then the chat modules.
function loadPageModules(context) {
  vm.createContext(context);
  vm.runInContext(readStatic('markdown-renderer.js'), context, {filename: 'markdown-renderer.js'});
  runStaticModules(context, chatModules());
}

function forYouRig(messages, {view = 'foryou', clientHeight = 900} = {}) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = clientHeight;
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
    fetch: async () => ({ok: false, status: 500, json: async () => ({})}),
    document: {
      addEventListener() {},
      createElement: (tag) => new FakeElement(tag),
      createTreeWalker: () => ({}),
      querySelectorAll: () => [],
      getElementById(id) {
        if (id === 'messages') return root;
        if (id === 'streaming-msg') return stream;
        return null;
      },
    },
  };
  loadPageModules(context);
  installEngineTimers(context, timers);
  context.Chat.buildTurnEngineMessageNode = fakeEngineNode;
  const engine = context.Chat.TurnEngine.mountIfAvailable(root, messages, 'sess-fy', {view});
  assert.ok(engine && engine.alive, 'the engine mounts the chat container');
  return {context, root, stream, timers, engine};
}

// One row per child of the list: the role of a message, `fold:N` for a fold line
// with N counted messages, and the structural rows by name.
function rowsOf(root) {
  return root.children.map((el) => {
    if (el.classList.contains('for-you-strip')) return 'strip';
    if (el.classList.contains('for-you-fold')) return 'fold:' + el.dataset.count;
    if (el.classList.contains('for-you-working')) return 'working';
    if (el.classList.contains('hidden')) return 'holder';
    if (el.id === 'load-more-sentinel') return 'sentinel';
    return el.dataset.messageRole;
  });
}

function foldLines(root) {
  return root.children.filter((el) => el.classList.contains('for-you-fold'));
}

function messageIds(el) {
  return el.dataset.messageId;
}

// The fixture the cases read: a first turn with a web reply mid-steps, a notice
// after it, a second turn that opens with a status block, an error row and a
// child report under a trailing fold.
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

const FIXTURE_ROWS = [
  'strip', 'user', 'fold:1', 'web_reply', 'fold:1', 'web_reply_notice', 'user', 'fold:1', 'system', 'fold:2', 'holder',
];

test('For you shows the user, web reply, notice and error rows and folds the rest per run', () => {
  const rig = forYouRig(fixture());
  assert.deepEqual(rowsOf(rig.root), FIXTURE_ROWS);
});

test('a fold counts its messages and leaves separators out of the count', () => {
  const rig = forYouRig(fixture());
  // The run between r1 and n1 holds a2 and s1; the separator is not a message.
  assert.equal(foldLines(rig.root)[1].dataset.count, '1');
  // The trailing run holds o1, c1 and s2.
  assert.equal(foldLines(rig.root)[3].dataset.count, '2');
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

test('a fold opens in place and closes again', () => {
  const rig = forYouRig(fixture());
  const fold = foldLines(rig.root)[0];
  assert.equal(fold.getAttribute('aria-expanded'), 'false');
  fold.onclick();
  assert.equal(fold.getAttribute('aria-expanded'), 'true');
  assert.deepEqual(rowsOf(rig.root).slice(0, 4), ['strip', 'user', 'fold:1', 'assistant']);
  assert.equal(messageIds(rig.root.children[3]), 'a1', 'the hidden message sits right after its fold line');
  assert.equal(rig.root.children[4].dataset.messageId, 'r1', 'the next shown message follows it');

  foldLines(rig.root)[0].onclick();
  assert.deepEqual(rowsOf(rig.root), FIXTURE_ROWS);
  assert.equal(foldLines(rig.root)[0].getAttribute('aria-expanded'), 'false');
});

test('the fold counts recompute when an older page lands, and the strip stays on the newest status block', () => {
  const rig = forYouRig(fixture());
  rig.engine.prependMessages([
    eMsg('user', 'p0', 'older question'),
    eMsg('assistant', 'pa', '**Goal**: old\n**Now**: ancient\n**Waiting on you**: none'),
    eMsg('separator', 'ps', '', {thinking_seconds: 5, event_index: 99}),
  ]);
  settle(rig.timers);
  assert.deepEqual(rowsOf(rig.root), [
    'strip', 'user', 'fold:1', 'user', 'fold:1', 'web_reply', 'fold:1', 'web_reply_notice',
    'user', 'fold:1', 'system', 'fold:2', 'holder',
  ]);
  const values = rig.root.children[0].children.map((row) => row.children[1].textContent);
  assert.deepEqual(values, ['patching the parser', 'nothing yet']);
});

test('the progress strip takes the newest status block and updates when a newer one lands', () => {
  const rig = forYouRig(fixture());
  const strip = rig.root.children[0];
  const values = () => strip.children.map((row) => row.children[1].textContent);
  // a3 is folded in For you, and the strip still reads it: the status block is
  // the newest one in the loaded history.
  assert.deepEqual(values(), ['patching the parser', 'nothing yet']);

  rig.context.appendMessageObject(
      eMsg('assistant', 'a4', '**Goal**: fix it\n**Now**: tests running\n**Waiting on you**: review the diff'),
      'sess-fy', false);
  settle(rig.timers);
  assert.equal(rig.root.children[0], strip, 'the strip is the same element, updated in place');
  assert.deepEqual(values(), ['tests running', 'review the diff']);
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
  assert.deepEqual(rowsOf(rig.root).slice(-3), ['fold:2', 'web_reply', 'holder']);
});

test('the working line shows while a turn runs and goes at its end, with the stream output kept hidden', () => {
  const rig = forYouRig(fixture());
  assert.ok(!rowsOf(rig.root).includes('working'));

  rig.context.masterThinking = true;
  rig.context.Chat.ForYou.refresh();
  assert.deepEqual(rowsOf(rig.root).slice(-2), ['working', 'holder']);
  const holder = rig.root.children[rig.root.children.length - 1];
  assert.equal(rig.stream.parentElement, holder, 'the stream output sits in the hidden holder');

  rig.context.masterThinking = false;
  rig.context.Chat.ForYou.refresh();
  assert.deepEqual(rowsOf(rig.root), FIXTURE_ROWS);
});

test('a refresh with nothing to move keeps every row element', () => {
  const rig = forYouRig(fixture());
  const before = rig.root.children.slice();
  rig.engine.refreshForYou();
  const after = rig.root.children;
  assert.equal(after.length, before.length);
  after.forEach((el, i) => assert.equal(el, before[i], `row ${i} is the same element`));
});

test('Everything is unchanged after a For you round trip', () => {
  const rig = forYouRig(fixture(), {view: 'everything'});
  rig.engine.setOverride('u1|a2|s1', true);
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
