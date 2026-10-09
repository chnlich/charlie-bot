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
  scrollTo,
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
function openRig({messages, session = {}, thinkingSince = null, pendingDraft = null, clientHeight = CLIENT_HEIGHT} = {}) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = clientHeight;
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
  // the maximum scroll. The open does not pin there — the engine records
  // "follow the next turn" when the mount restore leaves the view at the
  // bottom, so late layout changes cannot drag the view down, and the next
  // append re-arms the pin when it lands.
  const messages = [];
  for (let i = 0; i < 8; i++) messages.push(...turn(`t${i}_`, i));
  const rig = openRig({messages});
  assert.ok(rig.root.scrollHeight > rig.root.clientHeight + 80, 'the history overflows the viewport');
  assert.ok(rig.engine.lastRestoreClamp, 'the anchor restore asked past the scroll range');
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the clamped restore landed at the bottom');
  assert.equal(rig.engine.pinnedIntent, false, 'the open does not re-arm the pin');
  assert.equal(rig.engine.followNextTurn, true, 'the open records the deferred follow');
  settle(rig.timers);
  assert.equal(rig.engine.pinnedIntent, false, 'the idle slices do not pin');
  assert.equal(rig.engine.followNextTurn, true, 'the deferred follow survives the idle slices');

  // A WS-shaped arrival (no force) follows through the record.
  rig.context.appendMessageObject(eMsg('user', 'live-1', 'next question'), 'session-a', false);
  settle(rig.timers);
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the append kept the view at the bottom');
  assert.equal(rig.engine.pinnedIntent, true, 'the append re-armed the pin');
  assert.equal(rig.engine.followNextTurn, false, 'the append consumed the record');
});

test('a short reply whose tail placeholder shrinks follows the next turn', () => {
  // Eight finished turns, then one trailing system message after the last
  // separator: the engine groups it into a pending flat segment that opens
  // as a placeholder at its estimate (64 + 14 × 21 = 358px for 1000 chars),
  // far taller than the 54px it renders at. The mount restore leaves the
  // view above the bottom and sets no record; the placeholder resolving
  // shrinks the content, the restore clamps the view to the bottom, and the
  // engine sets the record there.
  const messages = [];
  for (let i = 0; i < 8; i++) messages.push(...turn(`t${i}_`, i));
  messages.push(eMsg('system', 'tail-1', 'x'.repeat(1000), {fakeHeight: 30}));
  const rig = openRig({messages});
  assert.ok(rig.root.querySelector('.turn-placeholder'), 'the tail segment opened as a placeholder');
  assert.ok(distanceFromBottom(rig.root) > 1, 'the mount restore left the view above the bottom');
  assert.equal(rig.engine.followNextTurn, false, 'no follow record before the placeholder resolves');
  settle(rig.timers);
  assert.equal(rig.root.querySelector('.turn-placeholder'), null, 'every placeholder resolved');
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the shrink left the view at the bottom');
  assert.equal(rig.engine.followNextTurn, true, 'the restore at the bottom set the record');

  rig.context.appendMessageObject(eMsg('user', 'live-1', 'next question'), 'session-a', false);
  settle(rig.timers);
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the append brought the view to the bottom');
  assert.equal(rig.engine.pinnedIntent, true, 'the append re-armed the pin');
});

test('a tall reply keeps its top and sets no record when the tail placeholder shrinks', () => {
  // The same trailing placeholder under a reply taller than the viewport:
  // the shrink lands below the anchored reply, the restore holds the reply
  // top at the viewport top, and the view never reaches the bottom, so the
  // engine sets no record.
  const messages = [];
  for (let i = 0; i < 8; i++) messages.push(...turn(`t${i}_`, i, i === 7 ? {fakeHeight: 900} : {}));
  messages.push(eMsg('system', 'tail-1', 'x'.repeat(1000), {fakeHeight: 30}));
  const rig = openRig({messages});
  settle(rig.timers);
  assert.equal(rig.root.querySelector('.turn-placeholder'), null, 'every placeholder resolved');
  assert.equal(rig.engine.followNextTurn, false, 'the view never reached the bottom');
  assert.ok(Math.abs(anchorY(rig.root, 't7_c7')) <= 1, 'the reply top held the viewport top');
});

test('an idle open of a fitting reply keeps the reply top when the view shrinks', () => {
  // Size the viewport to the content below the reply top, measured by a probe
  // open: the reply top opens exactly at the viewport top with the view
  // exactly at the bottom — the shape a late usage strip or trigger tray
  // then shrinks. A bottom-pinned view would follow the shrink and push the
  // reply top off the first screen; the deferred follow holds the position.
  const messages = [];
  for (let i = 0; i < 8; i++) messages.push(...turn(`t${i}_`, i));
  const probe = openRig({messages});
  settle(probe.timers);
  const replyContentTop = probe.root.scrollTop + anchorY(probe.root, 't7_c7');
  const belowReply = probe.root.scrollHeight - replyContentTop;

  const rig = openRig({messages, clientHeight: belowReply});
  settle(rig.timers);
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the open landed at the bottom');
  assert.ok(Math.abs(anchorY(rig.root, 't7_c7')) <= 1, 'the reply top opens at the viewport top');

  const beforeTop = anchorY(rig.root, 't7_c7');
  rig.root.clientHeight = belowReply - 60;
  rig.engine.handleResize();
  settle(rig.timers);
  const afterTop = anchorY(rig.root, 't7_c7');
  assert.ok(
      Math.abs(afterTop - beforeTop) <= 1,
      `the reply top held its place across the shrink (${beforeTop} -> ${afterTop})`);
  assert.ok(afterTop >= 0, 'the reply top stayed on the first screen');
  assert.equal(rig.engine.pinnedIntent, false, 'the shrink did not pin');
  assert.equal(rig.engine.followNextTurn, true, "the open's deferred follow survived the shrink");
});

test('a fitting reply opened at the bottom follows an append that lands after a shrink', () => {
  const messages = [];
  for (let i = 0; i < 8; i++) messages.push(...turn(`t${i}_`, i));
  const rig = openRig({messages});
  settle(rig.timers);
  assert.equal(rig.engine.followNextTurn, true, 'the open records the deferred follow');

  rig.root.clientHeight = CLIENT_HEIGHT - 60;
  rig.engine.handleResize();
  settle(rig.timers);
  assert.equal(rig.engine.pinnedIntent, false, 'the shrink did not pin');
  assert.equal(rig.engine.followNextTurn, true, 'the shrink kept the deferred follow');

  rig.context.appendMessageObject(eMsg('user', 'live-1', 'next question'), 'session-a', false);
  settle(rig.timers);
  assert.ok(distanceFromBottom(rig.root) <= 1, 'the append brought the view to the bottom');
  assert.equal(rig.engine.pinnedIntent, true, 'the append re-armed the pin');
  assert.equal(rig.engine.followNextTurn, false, 'the append consumed the record');
});

test('a reader scroll after a fitting open cancels the deferred follow', () => {
  const messages = [];
  for (let i = 0; i < 8; i++) messages.push(...turn(`t${i}_`, i));
  const rig = openRig({messages});
  settle(rig.timers);
  assert.equal(rig.engine.followNextTurn, true, 'the open records the deferred follow');

  scrollTo(rig.timers, rig.root, rig.root.scrollTop - 100);
  settle(rig.timers);
  assert.equal(rig.engine.followNextTurn, false, 'the reader scroll cleared the record');
  assert.equal(rig.engine.pinnedIntent, false, 'an upward scroll does not pin');

  const parked = rig.root.scrollTop;
  rig.context.appendMessageObject(eMsg('user', 'live-1', 'next question'), 'session-a', false);
  settle(rig.timers);
  assert.equal(rig.root.scrollTop, parked, 'the append left the reader where they parked');
});

test('an idle open of a tall reply keeps the reply top when the view shrinks', () => {
  const messages = [...turn('t0_', 0), ...turn('t1_', 1, {fakeHeight: 900})];
  const rig = openRig({messages});
  settle(rig.timers);
  assert.ok(Math.abs(anchorY(rig.root, 't1_c1')) <= 1, 'the tall reply opens at the viewport top');

  rig.root.clientHeight = CLIENT_HEIGHT - 60;
  rig.engine.handleResize();
  settle(rig.timers);
  assert.ok(Math.abs(anchorY(rig.root, 't1_c1')) <= 1, 'the reply top stayed at the viewport top');
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
