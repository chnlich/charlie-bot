const assert = require('node:assert/strict');
const test = require('node:test');

const {
  FakeElement,
  fakeEngineNode,
  buildDepthControl,
  makeEngineTimers,
  installEngineTimers,
  installScrollTopClamp,
  settle,
  eMsg,
} = require('./turn_engine_harness');
const {baseSessionContext, createChatSidebarContext} = require('./session_context_stub');

// ---------------------------------------------------------------------------
// Open landing position: an idle session opens at the top of its latest
// assistant reply (the status block's home), a running or non-prose open
// keeps the bottom pin. These cases drive the real renderSessionView through
// the sidebar context, the way the first load and a session switch land.
// ---------------------------------------------------------------------------

const CLIENT_HEIGHT = 300;

// The engine's message-node factory with a per-message height override: the
// tall-reply case needs one message taller than the viewport, which the
// default leaf heights cannot express.
function heightNodeFactory(msg) {
  const el = fakeEngineNode(msg);
  if (msg.fakeHeight != null) el.__baseHeight = msg.fakeHeight;
  return el;
}

// One closed turn: user head, assistant conclusion, separator. `extra` rides
// on the assistant message so a turn can carry the height override.
function turn(prefix, i, extra = {}) {
  return [
    eMsg('user', `${prefix}h${i}`, `question ${i}`),
    eMsg('assistant', `${prefix}c${i}`, `answer ${i}`, extra),
    eMsg('separator', `${prefix}p${i}`, '', {thinking_seconds: 30 + i, event_index: 1000 + i}),
  ];
}

// Mirrors transcriptRig in chat_scroll_reading_position.test.js: a chat
// container with the turn engine supported, driving the real renderSessionView
// with a master session's bootstrap payload.
function openRig({messages, session = {}, thinkingSince = null, pendingDraft = null} = {}) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = CLIENT_HEIGHT;
  installScrollTopClamp(root);
  const stream = new FakeElement('DIV', {id: 'streaming-msg'});
  root.appendChild(stream);
  const {context} = baseSessionContext();
  // The page sets THINKING_SINCE from the payload before every open path
  // renders; the rig takes it as the scenario's flight state.
  context.THINKING_SINCE = thinkingSince;
  context.document.createElement = (tag) => new FakeElement(tag);
  context.document.createTreeWalker = () => ({});
  context.document.getElementById = (id) => {
    if (id === 'messages') return root;
    if (id === 'streaming-msg') return stream;
    if (id === 'page-depth-control') return buildDepthControl();
    for (const child of root.children) {
      if (child.id === id) return child;
    }
    return null;
  };
  context.document.querySelector = () => null;
  context.document.querySelectorAll = () => [];
  const timers = makeEngineTimers();
  installEngineTimers(context, timers);
  context.setInterval = () => 0;
  context.clearInterval = () => {};
  context.fetch = async () => ({ok: false, status: 500, json: async () => ({})});
  createChatSidebarContext(context);
  context.Chat.buildTurnEngineMessageNode = heightNodeFactory;
  context.renderSessionView({
    session: Object.assign(
        {id: 'session-a', name: 'open position', backend: 'claude-opus-4.6', round_ratings: {}},
        session),
    messages,
    pending_draft: pendingDraft,
    event_count: 40,
    oldest_message_ordinal: 0,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    switchable_backends: [],
    has_more: false,
  });
  const engine = context.Chat.TurnEngine.activeFor(root);
  assert.ok(engine && engine.alive, 'the turn engine mounted for the open');
  return {context, root, engine, timers};
}

// Viewport-relative top of a message node in the fake layout (container top
// = 0); the anchored reply must sit at 0.
function anchorY(root, id) {
  const el = root.querySelector(`[data-message-id="${id}"]`);
  assert.ok(el, `message ${id} is in the DOM`);
  return el.getBoundingClientRect().top - root.getBoundingClientRect().top;
}

function distanceFromBottom(root) {
  return root.scrollHeight - root.scrollTop - root.clientHeight;
}

test('an idle open lands at the top of a last reply taller than the viewport', () => {
  const messages = [...turn('t0_', 0), ...turn('t1_', 1, {fakeHeight: 900})];
  const rig = openRig({messages});
  settle(rig.timers);
  assert.ok(distanceFromBottom(rig.root) > 1, 'the tall reply left the view above the bottom');
  assert.ok(Math.abs(anchorY(rig.root, 't1_c1')) <= 1, 'the last reply opens at the viewport top');
  assert.equal(rig.engine.pinnedIntent, false, 'an anchored open does not follow appends');
});

test('an idle open of a fitting reply lands at the bottom and follows the next turn', () => {
  // Eight finished turns: the last reply (70px) fits the 300px viewport while
  // the folded history around it does not, so the anchor restore clamps to
  // the maximum scroll and the open re-arms the pin there.
  const messages = [];
  for (let i = 0; i < 8; i++) messages.push(...turn(`t${i}_`, i));
  const rig = openRig({messages});
  assert.ok(rig.root.scrollHeight > rig.root.clientHeight + 80, 'the history overflows the viewport');
  assert.ok(rig.engine.lastRestoreClamp, 'the anchor restore asked past the scroll range');
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the clamped restore landed at the bottom');
  assert.equal(rig.engine.pinnedIntent, true, 'a bottom landing re-armed the pin');
  settle(rig.timers);
  assert.equal(rig.engine.pinnedIntent, true, 'the pin survives the idle slices');

  // A WS-shaped arrival (no force) follows only through the pin.
  rig.context.appendMessageObject(eMsg('user', 'live-1', 'next question'), 'session-a', false);
  settle(rig.timers);
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the append kept the view at the bottom');
  assert.equal(rig.engine.pinnedIntent, true, 'the append kept the pin');
});

test('a session with a turn in flight keeps the bottom-pinned mount', () => {
  const messages = [...turn('t0_', 0), ...turn('t1_', 1, {fakeHeight: 900})];
  const rig = openRig({messages, thinkingSince: '2026-04-02T10:00:00.000Z'});
  settle(rig.timers);
  assert.equal(rig.engine.pinnedIntent, true, 'the default mount is bottom-pinned');
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the open landed at the bottom');
});

test('a worker session keeps the bottom-pinned mount', () => {
  const messages = [...turn('t0_', 0), ...turn('t1_', 1, {fakeHeight: 900})];
  const rig = openRig({messages, session: {profile: 'worker'}});
  settle(rig.timers);
  assert.equal(rig.engine.pinnedIntent, true, 'the default mount is bottom-pinned');
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the open landed at the bottom');
});

test('openMountOptions anchors the last assistant message and defaults otherwise', () => {
  const rig = openRig({messages: [...turn('t0_', 0)]});
  const openMountOptions = rig.context.openMountOptions;
  assert.ok(openMountOptions, 'openMountOptions is exposed for the open path');

  // Messages after the reply carry other roles (the closed turn's separator,
  // a system notice); the anchor stays on the reply.
  const messages = [
    eMsg('user', 'h1', 'question'),
    eMsg('assistant', 'c1', 'answer'),
    eMsg('separator', 'p1', '', {thinking_seconds: 30, event_index: 1001}),
    eMsg('system', 's1', 'notice'),
  ];
  const session = {id: 'session-a', name: 'open position', round_ratings: {}};
  const data = {pending_draft: null};

  // Field-wise comparison: options come from the vm context, whose Object
  // prototype deepStrictEqual's realm check would reject.
  const options = openMountOptions(session, data, messages);
  assert.equal(options && options.pinned, false, 'the anchor mount is unpinned');
  assert.equal(options && options.readingAnchor && options.readingAnchor.kind, 'message', 'anchor kind');
  assert.equal(options && options.readingAnchor && options.readingAnchor.id, 'c1', 'anchor is the last assistant message');
  assert.equal(options && options.readingAnchor && options.readingAnchor.offset, 0, 'anchor offset');

  assert.equal(openMountOptions({profile: 'worker'}, data, messages), null, 'worker session');
  assert.equal(
      openMountOptions(session, {thread_view: {session_id: 'session-a', thread_id: 't'}}, messages),
      null, 'thread view');
  rig.context.THINKING_SINCE = '2026-04-02T10:00:00.000Z';
  assert.equal(openMountOptions(session, data, messages), null, 'turn in flight');
  rig.context.THINKING_SINCE = null;
  assert.equal(openMountOptions(session, {pending_draft: {content: 'partial'}}, messages), null, 'draft content');
  assert.equal(openMountOptions(session, {pending_draft: {thinking: 'mid-thought'}}, messages), null, 'draft thinking');
  assert.equal(
      openMountOptions(session, data, [eMsg('user', 'h1', 'question')]),
      null, 'no assistant message');
});
