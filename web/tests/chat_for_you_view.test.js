const assert = require('node:assert/strict');
const crypto = require('node:crypto');
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
const {readStatic, readPackageStatic, chatModules, runStaticModules} = require('./read_static');

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

function appendButtonMarkup(parent, html, context) {
  const buttons = /<button\b([^>]*)>([\s\S]*?)<\/button>/g;
  let match;
  while ((match = buttons.exec(html))) {
    const button = new FakeElement('BUTTON');
    const attributes = /([a-zA-Z_:][\w:.-]*)="([^"]*)"/g;
    let attribute;
    let onclick = null;
    while ((attribute = attributes.exec(match[1]))) {
      const [, name, value] = attribute;
      button.setAttribute(name, value);
      if (name === 'class') button.className = value;
      if (name.startsWith('data-')) {
        button.dataset[name.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = value;
      }
      if (name === 'onclick') onclick = value;
    }
    button.innerHTML = match[2];
    if (onclick) button.onclick = () => vm.runInContext(onclick, context);
    parent.appendChild(button);
  }
}

function installButtonHtmlParser(context) {
  const createElement = context.document.createElement;
  context.document.createElement = (tag) => {
    const element = createElement(tag);
    let html = element.innerHTML;
    Object.defineProperty(element, 'innerHTML', {
      get() { return html; },
      set(value) {
        html = String(value);
        if (element.classList.contains('separator-line') || element.classList.contains('round-buttons')) {
          appendButtonMarkup(element, html, context);
        }
      },
    });
    return element;
  };
}

function loadRoundActionModules(context, wsHandlers) {
  context.Sidebar = {wire: (globals) => Object.assign(context, globals)};
  context.registerSidebarControl = () => {};
  context.registerWsMessageHandler = (type, handler) => wsHandlers.set(type, handler);
  context.BACKEND_OPTIONS = {test: 'Test backend'};
  context.JSON_HEADERS = {'Content-Type': 'application/json'};
  context.getActiveBackendId = () => 'test';
  context.getDefaultBackendId = () => 'test';
  context.location = {};
  context.alert = () => {};
  vm.runInContext(readStatic('sidebar/modals.js'), context, {filename: 'sidebar/modals.js'});
  vm.runInContext(readPackageStatic('features', 'explain', 'explain.js'), context, {filename: 'explain.js'});
}

function forYouRig(
    messages, {view = 'foryou', clientHeight = 900, profile = null, workerSummary = null, hasMore = false} = {}) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = clientHeight;
  const sentinel = hasMore ? new FakeElement('DIV', {id: 'load-more-sentinel'}) : null;
  if (sentinel) root.appendChild(sentinel);
  const stream = new FakeElement('DIV', {id: 'streaming-msg'});
  root.appendChild(stream);
  const modalElements = new Map([
    'session-action-modal-overlay',
    'session-action-modal-title',
    'session-action-modal-body',
    'session-action-modal-confirm',
    'session-action-backend',
  ].map((id) => [id, new FakeElement(id === 'session-action-backend' ? 'SELECT' : 'DIV', {id})]));
  modalElements.get('session-action-modal-overlay').className = 'hidden';
  const wsHandlers = new Map();
  const fetchCalls = [];
  const timers = makeEngineTimers();
  const context = {
    CSS: {escape: (value) => String(value)},
    SESSION_ID: 'sess-fy',
    // index.html's host fact; the displayed-session module restates it in the tab title.
    PAGE_HOSTNAME: 'localhost:8000',
    SESSION_NAME: 'Session sess-fy',
    masterThinking: false,
    console: {error() {}, log() {}},
    marked: {Renderer: function Renderer() {}, use() {}},
    window: {addEventListener() {}},
    platform: {isMobile: false, onChange() {}},
    fetch: async (url, options = {}) => {
      fetchCalls.push({url, options});
      if (String(url).includes('/explain?upto=')) return new Promise(() => {});
      return {ok: true, status: 200, json: async () => ({id: 'new-session', round_ratings: {}})};
    },
    document: {
      addEventListener() {},
      createElement: (tag) => new FakeElement(tag),
      createTreeWalker: () => ({}),
      querySelector: (selector) => root.querySelector(selector),
      querySelectorAll: (selector) => root.querySelectorAll(selector),
      getElementById(id) {
        if (id === 'messages') return root;
        if (id === 'streaming-msg') return stream;
        if (id === 'load-more-sentinel') return sentinel && sentinel.parentElement ? sentinel : null;
        return modalElements.get(id) || null;
      },
    },
  };
  loadPageModules(context);
  loadRoundActionModules(context, wsHandlers);
  installButtonHtmlParser(context);
  if (profile) context.Chat.ForYou.setSession({profile});
  if (workerSummary) context.Chat.applyWorkerSummary(workerSummary);
  installEngineTimers(context, timers);
  context.Chat.buildTurnEngineMessageNode = (msg, sessionId) => {
    const node = fakeEngineNode(msg);
    if (msg.role === 'separator') {
      if (msg.event_index != null) node.dataset.eventIndex = String(msg.event_index);
      // The Everything separator's inner markup (rendering.js renderMessage's
      // separator row): a hairline, the round's button group when the round
      // carries buttons, a second hairline, and no label.
      node.appendChild(new FakeElement('DIV', {className: 'flex-1 border-t border-slate-600/40 separator-hairline'}));
      if (msg.kind !== 'run_end') {
        const group = context.Chat.buildRoundButtonGroup(sessionId, msg);
        if (group.children.length) node.appendChild(group);
      }
      node.appendChild(new FakeElement('DIV', {className: 'flex-1 border-t border-slate-600/40 separator-hairline'}));
    }
    return node;
  };
  const engine = context.Chat.TurnEngine.mountIfAvailable(root, messages, 'sess-fy', {view});
  assert.ok(engine && engine.alive, 'the engine mounts the chat container');
  // A worker page hands its sentinel to the engine after the mount, as
  // session-view.js renderSessionView does.
  if (sentinel && profile === 'worker') engine.holdWorkerSentinel(sentinel);
  return {context, root, stream, sentinel, timers, engine, modalElements, wsHandlers, fetchCalls};
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

function replyRound() {
  return [
    eMsg('user', 'round-u', 'question'),
    eMsg('assistant', 'round-a1', 'working'),
    eMsg('web_reply', 'round-r1', 'first reply'),
    eMsg('assistant', 'round-a2', 'more work'),
    eMsg('web_reply', 'round-r2', 'last reply'),
    eMsg('separator', 'round-s', '', {thinking_seconds: 4, event_index: 701}),
  ];
}

// The one round-button group of a round, by the round marker: For you hosts it
// in the last reply's bubble time row, Everything on the round's separator.
function roundGroupFor(root, eventIndex) {
  return root.querySelectorAll('.round-buttons')
    .find((el) => el.dataset.roundEventIndex === String(eventIndex)) || null;
}

// The reply bubble a For-you group hangs in: the group's message ancestor.
function replyOfGroup(group) {
  for (let node = group; node; node = node.parentElement) {
    if (node.dataset && node.dataset.messageId) return node;
  }
  return null;
}

function separatorLineFor(root, eventIndex) {
  return root.querySelectorAll('.separator-line').find((el) => el.dataset.eventIndex === String(eventIndex)) || null;
}

// The buttons a group or a row that hosts one carries.
function buttonsOf(host) {
  const group = host.classList.contains('round-buttons') ? host : host.querySelector('.round-buttons');
  return (group || host).children.filter((el) => el.tagName === 'BUTTON');
}

function buttonInventory(group) {
  return {
    roundEventIndex: group.dataset.roundEventIndex,
    roundId: group.dataset.roundId,
    buttons: buttonsOf(group).filter((button) => !button.classList.contains('turn-collapse')).map((button) => ({
      title: button.getAttribute('title'),
      className: button.className,
      onclick: button.getAttribute('onclick'),
      rating: button.dataset.roundRating || null,
      ratingSession: button.dataset.roundRatingSession || null,
      ratingEvent: button.dataset.roundRatingEvent || null,
    })),
  };
}

test('For you renders no separator line', () => {
  const rig = forYouRig(fixture());
  assert.equal(rig.root.querySelectorAll('.separator-line').length, 0);
});

test('one finished round puts its button group in the last reply bubble of that round', () => {
  const rig = forYouRig(replyRound());
  assert.deepEqual(rowsOf(rig.root), ['user', 'web_reply', 'web_reply', 'holder']);

  const group = roundGroupFor(rig.root, 701);
  assert.ok(group, 'the finished round carries its button group');
  assert.equal(group.dataset.roundId, 'round-s');
  assert.equal(replyOfGroup(group).dataset.messageId, 'round-r2', 'the group rides the last reply');
  assert.ok(group.parentElement.classList.contains('bubble-time-row'), 'the group sits in the time row');
  assert.equal(buttonsOf(group).length, 5);

  // The first reply of the round carries no group: one round, one group, on the last reply.
  const firstReply = rig.root.querySelector('[data-message-id="round-r1"]');
  assert.equal(firstReply.querySelectorAll('.round-buttons').length, 0);
  assert.equal(rig.root.querySelectorAll('.round-buttons').length, 1, 'one group per round');
});

test('a live separator adds the button group to the last reply bubble when the round closes', () => {
  const messages = replyRound().slice(0, -1);
  const rig = forYouRig(messages);
  assert.equal(roundGroupFor(rig.root, 701), null, 'the running round has no button group');

  rig.engine.appendMessage(replyRound().at(-1), false);
  settle(rig.timers);

  const group = roundGroupFor(rig.root, 701);
  assert.ok(group, 'the closed round carries its button group');
  assert.equal(replyOfGroup(group).dataset.messageId, 'round-r2');
});

test('rounds without a reply and run_end rounds add no group, and worker For you stays a summary', () => {
  const noReply = forYouRig([
    eMsg('user', 'no-reply-u', 'question'),
    eMsg('assistant', 'no-reply-a', 'working'),
    eMsg('separator', 'no-reply-s', '', {event_index: 702}),
  ]);
  assert.equal(roundGroupFor(noReply.root, 702), null);

  const runEnd = forYouRig([
    eMsg('web_reply', 'run-end-r', 'reply'),
    eMsg('separator', 'run-end-s', '', {kind: 'run_end', event_index: 703}),
  ]);
  assert.equal(runEnd.root.querySelectorAll('.round-buttons').length, 0);

  const worker = forYouRig(replyRound(), {
    profile: 'worker',
    workerSummary: {task_state: 'open', runs: [], closing: {work: null, review: null}, links: {}},
  });
  assert.equal(worker.root.querySelectorAll('.round-buttons').length, 0);
  assert.equal(worker.root.children.filter((child) => child.classList.contains('worker-summary')).length, 1);
});

test('For-you and Everything use the same round buttons and action boundaries', async () => {
  const messages = replyRound();
  const forYou = forYouRig(messages);
  const forYouGroup = roundGroupFor(forYou.root, 701);
  const separatorMessage = messages[messages.length - 1];

  forYou.engine.setView('everything');
  settle(forYou.timers);
  const separatorLine = separatorLineFor(forYou.root, separatorMessage.event_index);
  const separatorGroup = separatorLine.querySelector('.round-buttons');
  assert.ok(separatorGroup, 'the Everything separator hosts the round group');
  assert.deepEqual(buttonInventory(forYouGroup), buttonInventory(separatorGroup));
  assert.equal(buttonsOf(separatorGroup).length, 5);
  assert.deepEqual(buttonsOf(separatorGroup).slice(-2).map((button) => button.dataset.roundRatingEvent), [
    'round-s', 'round-s',
  ]);

  async function actionRequest(view, title) {
    const rig = forYouRig(messages, {view});
    settle(rig.timers);
    const group = view === 'foryou'
      ? roundGroupFor(rig.root, 701)
      : separatorLineFor(rig.root, separatorMessage.event_index).querySelector('.round-buttons');
    const button = buttonsOf(group).find((candidate) => candidate.getAttribute('title') === title);
    assert.ok(button, `${title} exists in ${view}`);
    button.onclick();
    assert.equal(rig.modalElements.get('session-action-modal-overlay').classList.contains('flex'), true);
    const firstRequest = rig.fetchCalls.length;
    await rig.context.submitSessionActionModal();
    const post = rig.fetchCalls.slice(firstRequest).find((request) => request.options.method === 'POST');
    assert.ok(post, `${title} confirms through its session action request`);
    return {
      url: post.url,
      body: JSON.parse(post.options.body),
    };
  }

  for (const title of ['Clone to here', 'Elon-e: retry with a fresh perspective', 'Explain this round with a chosen model']) {
    assert.deepEqual(await actionRequest('foryou', title), await actionRequest('everything', title));
  }
});

test('Everything separator HTML carries the round group and no label', () => {
  const rig = forYouRig([]);
  const html = rig.context.renderMessage(eMsg('separator', 'round-s', '', {
    thinking_seconds: 4,
    event_index: 701,
  }), 'sess-fy');
  const hash = crypto.createHash('sha256').update(html).digest('hex');

  assert.equal(hash, '567d2c6d23350203d2bdfdf011fb7301a3f66bea4da786d828127c84a0a6b7a0');
  assert.doesNotMatch(html, /response complete/);
  assert.doesNotMatch(html, /separator-label/);
  assert.equal((html.match(/\bseparator-hairline\b/g) || []).length, 2, 'both hairlines stay');
  assert.match(html, /data-round-event-index="701"/);
  assert.match(html, /data-round-id="round-s"/);
});

// An element as it serializes: its class, its data attributes in the order the
// builder sets them, and its inner markup.
function serializedGroup(el) {
  const attrs = Object.entries(el.dataset)
      .map(([key, value]) => ` data-${key.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`)}="${value}"`);
  return `<div class="${el.className}"${attrs.join('')}>${el.innerHTML}</div>`;
}

test('a For-you bubble group is the Everything separator group', () => {
  const messages = replyRound();
  const rig = forYouRig(messages);
  const group = roundGroupFor(rig.root, 701);
  const separatorGroup = rig.context.Chat.buildRoundButtonGroup('sess-fy', messages[messages.length - 1]);

  assert.equal(serializedGroup(group), serializedGroup(separatorGroup));
});

// The Everything separator markup for the shapes the run-end row must leave unchanged: a Run's
// end keeps its exact markup; a round without an event index and a round without thinking
// seconds lose the label with the text.
test('run_end separator HTML is unchanged; ordinary shapes drop only the label', () => {
  const rig = forYouRig([]);
  const shapes = [
    // The run-end row's visible markup is byte-identical to the shape before the
    // change; the hash moves only because the plan deletes the row's unread
    // data-thinking-seconds attribute with its readers.
    [eMsg('separator', 'end-s', '', {kind: 'run_end', thinking_seconds: 65, event_index: 703}),
      '1244e816f399c551fc1dfc1b81668b21fb409b146f66c32bb3c7dfbdb2f712dc'],
    [eMsg('separator', 'noidx-s', '', {thinking_seconds: 4}),
      'c2edfa567e38cdd5988bbff0d3832c1eb66e38634dde6e594c39a66220cdc7ab'],
    [eMsg('separator', 'nosec-s', '', {event_index: 705}),
      '4c97edc8edd85f7a795d67bcd79050229159191bb318bed5e5fddd9116804975'],
  ];
  for (const [msg, expected] of shapes) {
    const html = rig.context.renderMessage(msg, 'sess-fy');
    assert.equal(crypto.createHash('sha256').update(html).digest('hex'), expected, msg.id);
  }
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

test('selecting Status leaves #messages laid out and keeps its scroll position through the return', () => {
  const rig = forYouRig(fixture(), {clientHeight: 60});
  installScrollTopClamp(rig.root);
  scrollTo(rig.timers, rig.root, 0);
  const place = rig.root.scrollTop;
  assert.equal(rig.engine.pinnedIntent, false, 'an upward scroll unpins the reader');

  rig.context.Chat.setChatView('status');
  assert.equal(rig.engine.view, 'foryou', 'the engine keeps its message view under the Status overlay');
  assert.equal(rig.root.classList.contains('hidden'), false, '#messages stays laid out');
  assert.equal(rig.root.scrollTop, place, 'the list keeps its scroll position while Status shows');

  rig.context.Chat.setChatView('foryou');
  assert.equal(rig.root.scrollTop, place, 'the return shows the list where it was');
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
  assert.ok(roundGroupFor(rig.root, 101), 'the finished reply carries its cached round group');
  rig.engine.reproject('resize');
  const after = rig.root.children;
  assert.equal(after.length, before.length);
  after.forEach((el, i) => assert.equal(el, before[i], `row ${i} is the same element`));
});

test('rating and Explain state repaint the round group each view places', async () => {
  const messages = replyRound();
  const rig = forYouRig(messages, {view: 'everything'});
  settle(rig.timers);
  const key = 'round-u|round-a2|round-s';
  const cachedSeparator = separatorLineFor(rig.root, 701);
  assert.ok(cachedSeparator, 'Everything rendered the separator before the fold');

  rig.engine.setOverride(key, false);
  settle(rig.timers);
  assert.equal(separatorLineFor(rig.root, 701), null, 'the folded turn hides its separator');
  rig.engine.setView('foryou');
  settle(rig.timers);
  const group = roundGroupFor(rig.root, 701);

  let ratings = {};
  rig.context.fetch = async (url, options = {}) => {
    const body = JSON.parse(options.body);
    ratings = Object.assign({}, ratings, {'round-s': body.rating});
    if (body.rating === null) delete ratings['round-s'];
    return {ok: true, status: 200, json: async () => ({round_ratings: ratings})};
  };
  await rig.context.rateRound('sess-fy', 'round-s', 'thumbs_down');
  assert.equal(buttonsOf(group).find((button) => button.dataset.roundRating === 'thumbs_down')
      .classList.contains('text-red-400'), true);
  rig.wsHandlers.get('explain_status')({upto: 701, state: 'ready', backend: 'test'});
  assert.equal(buttonsOf(group).find((button) => button.classList.contains('explain-toggle')).dataset.explainState, 'ready');

  rig.engine.setView('everything');
  settle(rig.timers);
  rig.engine.setOverride(key, true);
  settle(rig.timers);
  const reopenedSeparator = separatorLineFor(rig.root, 701);
  assert.equal(reopenedSeparator, cachedSeparator, 'the fold restores the cached separator node');
  assert.equal(buttonsOf(reopenedSeparator).find((button) => button.dataset.roundRating === 'thumbs_down')
      .classList.contains('text-red-400'), true);
  assert.equal(buttonsOf(reopenedSeparator).find((button) => button.classList.contains('explain-toggle')).dataset.explainState, 'ready');

  await rig.context.rateRound('sess-fy', 'round-s', 'thumbs_up');
  rig.wsHandlers.get('explain_status')({upto: 701, state: 'pending', backend: 'test'});
  assert.equal(buttonsOf(reopenedSeparator).find((button) => button.dataset.roundRating === 'thumbs_up')
      .classList.contains('text-green-400'), true);

  rig.engine.setView('foryou');
  settle(rig.timers);
  const regrouped = roundGroupFor(rig.root, 701);
  assert.notEqual(regrouped, group, 'For you rebuilds the group it places');
  assert.equal(buttonsOf(regrouped).find((button) => button.dataset.roundRating === 'thumbs_up')
      .classList.contains('text-green-400'), true);
  assert.equal(buttonsOf(regrouped).find((button) => button.classList.contains('explain-toggle')).dataset.explainState, 'pending');
});

test('an Explain click in For you opens the panel below the bubble and marks the bubble button pending', async () => {
  const rig = forYouRig(replyRound());
  const group = roundGroupFor(rig.root, 701);
  const replyNode = replyOfGroup(group);
  const explain = buttonsOf(group).find((button) => button.classList.contains('explain-toggle'));
  explain.onclick();
  await rig.context.submitSessionActionModal();
  rig.wsHandlers.get('explain_status')({upto: 701, state: 'pending', backend: 'test'});

  const slot = replyNode.nextElementSibling;
  assert.ok(slot && slot.classList.contains('for-you-panel-slot'), 'the panel slot follows the reply');
  assert.equal(slot.previousElementSibling, replyNode, 'the panel lays out below the bubble');
  const panel = slot.querySelector('.explain-panel');
  assert.ok(panel, 'the slot holds the open panel');
  assert.equal(buttonsOf(group).find((button) => button.classList.contains('explain-toggle')).dataset.explainState,
      'pending', 'the pending state paints on the bubble button');
});

test('a For-you panel survives the Everything round trip, and closing it drops the slot', async () => {
  const rig = forYouRig(replyRound());
  const group = roundGroupFor(rig.root, 701);
  const replyNode = replyOfGroup(group);
  buttonsOf(group).find((button) => button.classList.contains('explain-toggle')).onclick();
  await rig.context.submitSessionActionModal();
  const slot = replyNode.nextElementSibling;
  assert.ok(slot && slot.classList.contains('for-you-panel-slot'));

  rig.engine.setView('everything');
  settle(rig.timers);
  assert.equal(rig.root.querySelectorAll('.for-you-panel-slot').length, 0,
      'Everything shows no For-you panel slot');
  assert.ok(separatorLineFor(rig.root, 701).querySelector('.round-buttons'),
      'the separator keeps its own group');
  assert.equal(rig.root.querySelectorAll('.round-buttons').length, 1,
      'the round keeps one group in Everything, and the bubble carries none');

  rig.engine.setView('foryou');
  settle(rig.timers);
  assert.equal(rig.root.querySelectorAll('.round-buttons').length, 1,
      'the return puts the one group back, in the bubble');
  assert.equal(replyOfGroup(roundGroupFor(rig.root, 701)), replyNode,
      'the group rides the reply bubble again');
  const slotBack = replyNode.nextElementSibling;
  assert.equal(slotBack, slot, 'the cached slot returns with the reply');
  assert.ok(slotBack.querySelector('.explain-panel'), 'the panel is still open');

  // A second confirm on the open panel re-fetches in place; the slot holds one
  // panel and no empty slot takes a row gap.
  buttonsOf(roundGroupFor(rig.root, 701)).find((button) => button.classList.contains('explain-toggle')).onclick();
  await rig.context.submitSessionActionModal();
  assert.equal(replyNode.nextElementSibling.querySelectorAll('.explain-panel').length, 1);

  // Dropping the slot (the panel-close path) removes it from the list and the
  // cache, so later projections place no gap.
  rig.engine.dropForYouPanelSlot('round-s');
  assert.ok(replyNode.nextElementSibling.classList.contains('hidden'),
      'the dropped slot leaves the list, the hidden holder following the reply');
  rig.engine.reproject('resize');
  settle(rig.timers);
  assert.equal(rig.root.querySelectorAll('.for-you-panel-slot').length, 0, 'the dropped slot stays dropped');
});

test('a generation that completes while Everything is shown reads ready on the bubble button', () => {
  const rig = forYouRig(replyRound());
  rig.engine.setView('everything');
  settle(rig.timers);
  rig.wsHandlers.get('explain_status')({upto: 701, state: 'ready', backend: 'test'});
  assert.equal(buttonsOf(separatorLineFor(rig.root, 701).querySelector('.round-buttons'))
      .find((button) => button.classList.contains('explain-toggle')).dataset.explainState, 'ready');

  rig.engine.setView('foryou');
  settle(rig.timers);
  const group = roundGroupFor(rig.root, 701);
  assert.equal(buttonsOf(group).find((button) => button.classList.contains('explain-toggle')).dataset.explainState,
      'ready', 'the group For you places reads the current Explain state');
});

test('an older page can supply the reply for a separator already loaded', () => {
  const rig = forYouRig([
    eMsg('assistant', 'loaded-a', 'working'),
    eMsg('separator', 'loaded-s', '', {event_index: 704}),
  ]);
  assert.equal(roundGroupFor(rig.root, 704), null);

  rig.engine.prependMessages([
    eMsg('user', 'older-u', 'question'),
    eMsg('web_reply', 'older-r1', 'opening reply'),
    eMsg('assistant', 'older-a', 'more work'),
    eMsg('web_reply', 'older-r2', 'last reply'),
  ]);
  settle(rig.timers);

  const group = roundGroupFor(rig.root, 704);
  assert.ok(group, 'the prepended reply carries the group');
  assert.equal(replyOfGroup(group).dataset.messageId, 'older-r2');
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
