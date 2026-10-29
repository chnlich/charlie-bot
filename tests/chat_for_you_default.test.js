const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {
  FakeElement,
  eMsg,
  fakeEngineNode,
  makeEngineTimers,
  installEngineTimers,
  installScrollTopClamp,
} = require('./turn_engine_harness');
const {baseSessionContext, createChatSidebarContext} = require('./session_context_stub');
const {readStatic} = require('./read_static');

// ---------------------------------------------------------------------------
// The session page's view at open: a web session, root or child, opens in For you;
// a thread session and a worker session open in Everything; a manual switch holds
// for its session until the page reloads. These cases drive the real
// renderSessionView through the sidebar context, the way a first load and a
// session switch land.
// ---------------------------------------------------------------------------

const SLACK_ORIGIN = {team_id: 'T1', channel_id: 'C1', thread_ts: '1.5'};
const DISCORD_ORIGIN = {guild_id: 'G1', channel_id: 'C2', thread_id: 'th-2'};

function under(el, ancestor) {
  for (let node = el; node; node = node.parentNode) {
    if (node === ancestor) return true;
  }
  return false;
}

// One rig is one page load: a fresh context, so the manual view choices start
// empty, as a reload leaves them.
function pageRig() {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = 300;
  installScrollTopClamp(root);
  root.appendChild(new FakeElement('DIV', {id: 'streaming-msg'}));
  const stream = root.children[0];
  const panel = new FakeElement('DIV', {id: 'chat-status-panel', className: 'hidden'});
  const elements = new Map([
    ['messages', root],
    ['streaming-msg', stream],
    ['thinking', new FakeElement('DIV', {id: 'thinking', className: 'hidden'})],
    ['thinking-time', new FakeElement('SPAN', {id: 'thinking-time'})],
    ['chat-status-panel', panel],
  ]);
  const toggles = ['foryou', 'everything', 'status'].map((option) => {
    const btn = new FakeElement('BUTTON');
    btn.dataset.chatViewOption = option;
    return btn;
  });
  const {context} = baseSessionContext();
  context.document.createElement = (tag) => new FakeElement(tag);
  context.document.createTreeWalker = () => ({});
  // The stream element counts as on the page only while the messages container holds it, as in the browser.
  context.document.getElementById = (id) => {
    const el = elements.get(id) || null;
    if (el) return (el !== stream || under(stream, root)) ? el : null;
    // An element no fixture entry names (the pagination sentinel) is on the page
    // only while the messages container holds it.
    const walk = (node) => {
      for (const child of node.children) {
        if (child.id === id) return child;
        const found = walk(child);
        if (found) return found;
      }
      return null;
    };
    return walk(root);
  };
  context.document.querySelector = () => null;
  context.document.querySelectorAll = () => toggles;
  const timers = makeEngineTimers();
  installEngineTimers(context, timers);
  context.setInterval = () => 0;
  context.clearInterval = () => {};
  context.fetch = async () => ({ok: false, status: 500, json: async () => ({})});
  context.marked = {parse: (text) => text, Renderer: function Renderer() {}, use() {}};
  createChatSidebarContext(context);
  // markdown-renderer.js loads after the chat modules, as the page loads it; a stub
  // marked stands in for the CDN build, which the load never calls.
  vm.runInContext(readStatic('markdown-renderer.js'), context, {filename: 'markdown-renderer.js'});
  context.Chat.buildTurnEngineMessageNode = fakeEngineNode;
  return {context, root, stream, timers, toggles, panel};
}

function sessionPayload(session, messages = fixtureMessages()) {
  const payloadSession = Object.assign({
      id: 'session-a',
      name: 'Session a',
      profile: 'manager',
      backend: 'claude-opus-4.6',
      round_ratings: {},
      task_parent_id: null,
    }, session);
  const payload = {
    session: payloadSession,
    messages,
    pending_draft: null,
    event_count: 40,
    oldest_message_ordinal: 0,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    switchable_backends: [],
    has_more: false,
  };
  if (payloadSession.profile === 'worker') {
    payload.worker_summary = {task_state: 'open', runs: [], closing: {}};
  }
  return payload;
}

function fixtureMessages() {
  return [
    eMsg('user', 'u1', 'the question'),
    eMsg('assistant', 'a1', '**Goal**: answer it\n**Now**: working\n**Waiting on you**: nothing'),
    eMsg('web_reply', 'r1', 'Here is the answer.'),
    eMsg('separator', 's1', '', {thinking_seconds: 12, event_index: 101}),
  ];
}

function openedView(rig, session) {
  rig.context.renderSessionView(sessionPayload(session));
  const engine = rig.context.Chat.activeTurnEngineFor(rig.root);
  assert.ok(engine && engine.alive, 'the turn engine mounted for the open');
  return engine.view;
}

function pressed(rig) {
  return rig.toggles.map((btn) => btn.getAttribute('aria-pressed'));
}

test('a root web session opens in For you', () => {
  const rig = pageRig();
  assert.equal(openedView(rig, {}), 'foryou');
  assert.deepEqual(pressed(rig), ['true', 'false', 'false']);
});

test('a child web session opens in For you', () => {
  const rig = pageRig();
  assert.equal(openedView(rig, {task_parent_id: 'parent-1'}), 'foryou');
  assert.deepEqual(pressed(rig), ['true', 'false', 'false']);
});

test('a Slack thread session opens in Everything', () => {
  const rig = pageRig();
  assert.equal(openedView(rig, {slack_origin: SLACK_ORIGIN}), 'everything');
});

test('a Discord thread session opens in Everything', () => {
  const rig = pageRig();
  assert.equal(openedView(rig, {discord_origin: DISCORD_ORIGIN}), 'everything');
});

test('a worker session opens in For you, root or child', () => {
  const rig = pageRig();
  assert.equal(openedView(rig, {profile: 'worker'}), 'foryou');
  assert.equal(openedView(rig, {profile: 'worker', task_parent_id: 'parent-1'}), 'foryou');
});

test('a web session shows the status panel in both views', () => {
  const rig = pageRig();
  openedView(rig, {});
  assert.equal(rig.panel.classList.contains('hidden'), false, 'For you shows the panel');
  rig.context.setChatView('everything');
  assert.equal(rig.panel.classList.contains('hidden'), false, 'Everything shows the panel');
  openedView(rig, {id: 'child-a', task_parent_id: 'parent-1'});
  assert.equal(rig.panel.classList.contains('hidden'), false, 'a child web session shows the panel');
});

test('a thread session and a worker session show no status panel', () => {
  const rig = pageRig();
  openedView(rig, {slack_origin: SLACK_ORIGIN});
  assert.equal(rig.panel.classList.contains('hidden'), true, 'a Slack thread session has no panel');
  openedView(rig, {discord_origin: DISCORD_ORIGIN});
  assert.equal(rig.panel.classList.contains('hidden'), true, 'a Discord thread session has no panel');
  openedView(rig, {profile: 'worker'});
  assert.equal(rig.panel.classList.contains('hidden'), true, 'a worker session has no panel');
});

// The toggle's third button: index 2 in the rig's data-chat-view-option list.
test('the Status option shows for a web session and stays hidden for thread and worker sessions', () => {
  const rig = pageRig();
  openedView(rig, {});
  assert.equal(rig.toggles[2].classList.contains('hidden'), false, 'a web session shows the Status option');
  openedView(rig, {id: 'child-a', task_parent_id: 'parent-1'});
  assert.equal(rig.toggles[2].classList.contains('hidden'), false, 'a child web session shows it too');

  openedView(rig, {id: 'thread-a', slack_origin: SLACK_ORIGIN});
  assert.equal(rig.toggles[2].classList.contains('hidden'), true, 'a Slack thread session keeps two options');
  openedView(rig, {id: 'thread-d', discord_origin: DISCORD_ORIGIN});
  assert.equal(rig.toggles[2].classList.contains('hidden'), true, 'a Discord thread session keeps two options');
  openedView(rig, {id: 'worker-a', profile: 'worker'});
  assert.equal(rig.toggles[2].classList.contains('hidden'), true, 'a worker session keeps two options');
});

test('selecting Status keeps the turn engine on its message view and holds the choice for the session', () => {
  const rig = pageRig();
  openedView(rig, {});
  const engine = rig.context.Chat.activeTurnEngineFor(rig.root);
  rig.context.setChatView('status');
  assert.equal(engine.view, 'foryou', 'the engine keeps its message view under the Status overlay');
  assert.equal(rig.root.classList.contains('hidden'), false, '#messages stays laid out');
  assert.equal(rig.context.Chat.ForYou.viewFor({id: 'session-a'}), 'status', 'the choice holds for the session');
  assert.deepEqual(pressed(rig), ['false', 'false', 'true']);

  assert.equal(openedView(rig, {}), 'foryou', 'the engine still mounts a message view');
  assert.deepEqual(pressed(rig), ['false', 'false', 'true'], 'the re-open carries the Status choice');

  rig.context.setChatView('everything');
  assert.equal(rig.context.Chat.activeTurnEngineFor(rig.root).view, 'everything', 'the return switches the engine');
  assert.equal(rig.context.Chat.ForYou.viewFor({id: 'session-a'}), 'everything');
});

test('a Status choice forgotten by a reload and never set for other sessions', () => {
  const first = pageRig();
  openedView(first, {});
  first.context.setChatView('status');

  const reloaded = pageRig();
  assert.equal(openedView(reloaded, {}), 'foryou', 'a reload returns the session to its default');
});

test('a manual switch holds for its session across a re-open, and other sessions keep their default', () => {
  const rig = pageRig();
  assert.equal(openedView(rig, {}), 'foryou');
  rig.context.setChatView('everything');
  assert.deepEqual(pressed(rig), ['false', 'true', 'false']);

  assert.equal(openedView(rig, {}), 'everything', 'the same session re-opens in its choice');
  assert.equal(openedView(rig, {id: 'session-b'}), 'foryou', 'another root session keeps the default');
  assert.equal(openedView(rig, {}), 'everything', 'and the first session still holds its choice');
});

test('a reload forgets the manual choice and returns each session to its default', () => {
  const first = pageRig();
  openedView(first, {});
  first.context.setChatView('everything');

  const reloaded = pageRig();
  assert.equal(openedView(reloaded, {}), 'foryou');
  assert.equal(openedView(reloaded, {id: 'child-a', task_parent_id: 'parent-1'}), 'foryou',
      'a child web session opens in For you after a reload');
});

test('a view switch keeps the pagination sentinel on the page', () => {
  const rig = pageRig();
  const data = sessionPayload({});
  data.has_more = true;
  rig.context.renderSessionView(data);
  const found = () => rig.context.document.getElementById('load-more-sentinel');
  const sentinel = found();
  assert.ok(sentinel, 'the sentinel is on the page while older pages remain');

  rig.context.setChatView('everything');
  assert.equal(found(), sentinel, 'the switch to Everything keeps the sentinel');

  rig.context.setChatView('foryou');
  assert.equal(found(), sentinel, 'the switch back to For you keeps the sentinel');
  assert.equal(rig.root.children[0], sentinel, 'the For you list leads with the sentinel');
});

test('a worker page shows no sentinel in For you, and none in Everything once a reset loads the full history',
    async () => {
  const rig = pageRig();
  // The transcript poll is module-private; the page drives it through the
  // 'transcript-poll' interval, so the rig keeps the fn the open registers.
  let poll = null;
  rig.context.setInterval = (fn) => {
    poll = fn;
    return 0;
  };
  const data = sessionPayload({profile: 'worker'});
  data.has_more = true;
  rig.context.renderSessionView(data);
  const found = () => rig.context.document.getElementById('load-more-sentinel');
  assert.equal(found(), null, 'For you holds only the summary card');

  rig.context.setChatView('everything');
  assert.ok(found(), 'Everything shows the sentinel while older messages remain');
  assert.equal(rig.root.children[0], found(), 'Everything leads with the sentinel');
  rig.context.setChatView('foryou');
  assert.equal(found(), null);

  const messages = fixtureMessages();
  rig.context.fetch = async () => ({
    ok: true,
    json: async () => ({reset: true, messages, total: messages.length, revision: 'r1', summary: data.worker_summary}),
  });
  await poll({force: true});
  assert.equal(found(), null, 'For you stays without a sentinel after the reset');
  rig.context.setChatView('everything');
  assert.equal(found(), null, 'the reset loaded the full history, so Everything has no sentinel');
});

test('a second open of a For you session keeps the stream element on the page', () => {
  const rig = pageRig();
  openedView(rig, {});
  assert.equal(openedView(rig, {}), 'foryou', 'the re-open renders without throwing in hideStreaming');
  assert.ok(under(rig.stream, rig.root), 'the stream element is still in the messages container');
});
