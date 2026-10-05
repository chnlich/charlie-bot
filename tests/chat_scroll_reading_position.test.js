const assert = require('node:assert/strict');
const test = require('node:test');

const {
  FakeElement,
  mountEngine,
  settle,
  scrollTo,
  ePage,
  eMsg,
  engineDebug,
  assertEngineInvariants,
  distanceFromBottom,
  loadChatContext,
  buildDepthControl,
  fakeEngineNode,
  makeEngineTimers,
  installEngineTimers,
  installScrollTopClamp,
} = require('./turn_engine_harness');
const {baseSessionContext, createChatSidebarContext} = require('./session_context_stub');

// Each transcript rig owns its timer queue; the helpers read it back off the
// context the harness installed.
const rigTimers = new WeakMap();
function timersOf(context) {
  return rigTimers.get(context);
}

// ---------------------------------------------------------------------------
// Reading-position regression: any upward read of history owns the viewport.
// The anchor is one message's identity plus its offset from the container's
// visible top; every update restores it. These cases reproduce the reported
// jumps on the pre-fix engine: pagination snapping to the new bottom, the
// segment-top anchor drifting the message, appends and stream growth yanking
// a reader paused inside the 150px band, the transcript reset repaint, and
// the hidden container's lost scroll position.
// ---------------------------------------------------------------------------

// Viewport-relative Y of a message node in the fake layout (container top = 0).
function anchorY(root, id) {
  const el = root.querySelector(`[data-message-id="${id}"]`);
  assert.ok(el, `message ${id} is in the DOM`);
  return el.getBoundingClientRect().top - root.getBoundingClientRect().top;
}

// A mid-history reader: genuine scroll events (the wheel path), settled
// pre-render, and the anchor message sitting just below the viewport top.
// The scroll target is the message's real box (a wheel gesture moves real
// pixels; the height model's estimates are not where content lives). Compact
// depth keeps every turn's body rendered, matching a reader who opened their
// history; the outline-mode anchor case has its own test.
function readingRig(prefix, turns, anchorTurn = Math.floor(turns / 2)) {
  const state = mountEngine(ePage(prefix, turns), {clientHeight: 300, clampScrollTop: true});
  state.context.setPageDepth('compact');
  settle(state.timers);
  const anchorId = `${prefix}t${anchorTurn}_h0`;
  // Stage one: a coarse genuine scroll toward the anchor's model offset —
  // the migration materializes the turns around it, the way a wheel journey
  // would. Stage two: the message node exists now; scroll its real box to
  // just below the viewport top.
  const coarse = engineDebug(state.context, state.root);
  scrollTo(state.timers, state.root, Math.max(0, coarse.offsets[anchorTurn] - 100));
  settle(state.timers);
  const target = state.root.querySelector(`[data-message-id="${anchorId}"]`);
  assert.ok(target, 'the anchor turn rendered after the coarse scroll');
  // rect tops are viewport-relative; the scroll target is the content
  // position (rect relative to the container plus its current scroll).
  const contentTop = target.getBoundingClientRect().top
    - state.root.getBoundingClientRect().top + state.root.scrollTop;
  scrollTo(state.timers, state.root, Math.max(0, Math.floor(contentTop) - 20));
  settle(state.timers);
  return {...state, anchorId, anchorY: anchorY(state.root, anchorId)};
}

test('a clamp echo after a shrink does not read as an upward wheel', () => {
  // The reported mount-time unpin: the engine pins to the bottom, an update
  // shrinks the content under the pinned position, and the browser reports
  // the clamped position as a scroll event. That event moves up while the
  // container shrinks — it is the clamp, not the user's wheel.
  const state = mountEngine(ePage('shrink_', 4), {clientHeight: 300, clampScrollTop: true});
  state.context.setPageDepth('compact');
  settle(state.timers);
  assert.equal(state.engine.pinnedIntent, true, 'mounted pinned');
  const pinnedTop = state.root.scrollTop;

  state.root.scrollHeight = state.root.scrollHeight - 100;
  state.root.fire('scroll');
  assert.ok(state.root.scrollTop < pinnedTop, 'the clamp moved the reported position up');
  assert.equal(state.engine.pinnedIntent, true, 'the shrink echo kept the pin intent');

  // With the height stable again, the same-sized move is a genuine wheel-up.
  state.root.scrollTop -= 100;
  state.root.fire('scroll');
  assert.equal(state.engine.pinnedIntent, false, 'a real upward wheel still unpins');
});

test('pagination prepend keeps the reading message at its viewport offset', () => {
  const rig = readingRig('rp_', 12, 6);
  const beforeTop = rig.root.scrollTop;
  assert.ok(distanceFromBottom(rig.root) > 150, 'reader is in history');

  const shift = rig.engine.prependMessages(ePage('older_', 4));
  // The write the old engine made here landed past the old scroll range and
  // the clamp echo misread as pinned, snapping the reader to the new bottom.
  assert.ok(rig.root.scrollTop < rig.root.scrollHeight - rig.root.clientHeight,
      'prepend does not park the reader at the bottom');
  settle(rig.timers);

  assert.equal(anchorY(rig.root, rig.anchorId), rig.anchorY,
      `the read message holds its viewport offset (shift=${shift})`);
  assert.equal(rig.engine.pinnedIntent, false, 'pagination leaves the reader unpinned');
  assert.ok(rig.root.scrollTop !== beforeTop || shift !== 0,
      'the position moved by the height added above the reader');
  assertEngineInvariants(rig.context, rig.root, rig.stream, 'after prepend');
});

test('a page merging into the leading turn keeps the reading position once rendered', () => {
  // The mounted tail never closes, so the page's trailing span merges into
  // the leading segment — the reader's message leaves the DOM inside the
  // placeholder period and must come back at the same viewport offset.
  const tail = [eMsg('assistant', 'tail-a', 'the read answer'), eMsg('user', 'tail-u', 'next ask')];
  const state = mountEngine([...ePage('mg_', 6), ...tail], {clientHeight: 300, clampScrollTop: true});
  state.context.setPageDepth('compact');
  settle(state.timers);
  scrollTo(state.timers, state.root, 0);
  settle(state.timers);
  const anchorId = 'mg_t0_h0';
  const before = anchorY(state.root, anchorId);

  state.engine.prependMessages(ePage('mgold_', 2));
  settle(state.timers);

  assert.equal(anchorY(state.root, anchorId), before,
      'the merged-in history above the message moved nothing on screen');
  const debug = engineDebug(state.context, state.root);
  assert.ok(debug.keys.some((key) => key && key.startsWith('mgold_')), 'older turns are in the store');
});

test('a genuine wheel-up inside the 150px band stops both append and stream follow', () => {
  const state = mountEngine(ePage('band_', 8), {clientHeight: 300, clampScrollTop: true});
  state.context.setPageDepth('compact');
  settle(state.timers);
  state.root.scrollTop = state.root.scrollHeight;
  state.root.fire('scroll');
  settle(state.timers);

  state.root.scrollTop -= 100;
  state.root.fire('scroll');
  settle(state.timers);
  assert.equal(state.engine.pinnedIntent, false, 'the wheel-up cleared the pin intent');

  const pausedTop = state.root.scrollTop;
  // The reader's place is the first message at or below the viewport top, the
  // node a real reader would be holding.
  const cTop = state.root.getBoundingClientRect().top;
  const anchorEl = [...state.root.querySelectorAll('[data-message-id]')]
    .find((el) => el.getBoundingClientRect().top - cTop >= -4);
  assert.ok(anchorEl, 'the viewport shows a message to hold');
  const anchorId = anchorEl.dataset.messageId;
  const anchorTop = anchorEl.getBoundingClientRect().top - cTop;
  state.engine.appendMessage(eMsg('assistant', 'band-append', 'arriving while reading'), false);
  const heldTop = state.root.querySelector(`[data-message-id="${anchorId}"]`)
    .getBoundingClientRect().top - state.root.getBoundingClientRect().top;
  assert.equal(Math.abs(heldTop - anchorTop) <= 2, true,
      'the append kept the read message at its viewport offset');
  assert.ok(state.root.scrollHeight > pausedTop + state.root.clientHeight,
      'the append grew the container below the reader');
  assert.equal(state.engine.pinnedIntent, false, 'the append did not re-pin');

  // The streaming paint routes through the same intent via restoreBottomPin.
  state.context.restoreBottomPin(state.root, state.engine.pinnedIntent, false);
  const stillTop = state.root.querySelector(`[data-message-id="${anchorId}"]`)
    .getBoundingClientRect().top - state.root.getBoundingClientRect().top;
  assert.equal(Math.abs(stillTop - anchorTop) <= 2, true,
      'a stream paint under an unpinned reader leaves the viewport alone');
});

test('restoreBottomPin routes the jump through the engine when one is mounted', () => {
  const state = mountEngine(ePage('route_', 4), {clientHeight: 300, clampScrollTop: true});
  state.context.setPageDepth('compact');
  settle(state.timers);
  scrollTo(state.timers, state.root, 50);
  assert.equal(state.engine.pinnedIntent, false, 'reader unpinned by the scroll');

  state.context.restoreBottomPin(state.root, true, false);
  assert.equal(state.engine.pinnedIntent, true, 'the engine jump re-armed the pin');
  assert.equal(distanceFromBottom(state.root), 0, 'the engine jump landed at the bottom');
});

// The transcript reset runs inside session-view.js, so these cases drive the
// real poll path through the sidebar context: renderSessionView mounts the
// engine, the worker-transcript mode points the poll at this session, and a
// stubbed fetch answers with a reset.
function transcriptRig(prefix, turns) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = 300;
  installScrollTopClamp(root);
  const stream = new FakeElement('DIV', {id: 'streaming-msg'});
  root.appendChild(stream);
  const {context} = baseSessionContext();
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
  rigTimers.set(context, timers);
  // page-timers.js drives its pollers through the interval globals; the test
  // reads the registered fn back out of the registry map.
  context.setInterval = (fn) => 0;
  context.clearInterval = () => {};
  context.fetch = async () => ({ok: false, status: 500, json: async () => ({})});
  createChatSidebarContext(context);
  context.Chat.buildTurnEngineMessageNode = fakeEngineNode;
  context.renderSessionView({
    session: {id: 'session-a', name: 'transcript session', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: ePage(prefix, turns),
    pending_draft: null,
    event_count: 40,
    oldest_message_ordinal: 0,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: false,
  });
  const engine = context.Chat.TurnEngine.activeFor(root);
  assert.ok(engine && engine.alive, 'the engine mounted for the transcript view');
  return {context, root, engine};
}

// pollTranscript is module-private; the page drives it through the
// 'transcript-poll' interval, so the rig captures the interval fn at
// registration and the test fires it the way the 2s cadence would.
async function resetTranscript(context, prefix, turns, revision) {
  const messages = ePage(prefix, turns);
  context.fetch = async () => ({
    ok: true,
    json: async () => ({reset: true, messages, total: messages.length, revision}),
  });
  let poll = null;
  const prevInterval = context.setInterval;
  context.setInterval = (fn) => {
    poll = fn;
    return prevInterval(fn);
  };
  context.setWorkerTranscriptMode({sessionId: context.SESSION_ID});
  await poll({force: true});
  // stopTranscriptPolling is module-private too; the teardown path drops the
  // timer by name through the registry's public stop.
  context.stopPageTimer('transcript-poll');
}

test('a transcript reset repaint keeps the reading position and a live engine', async () => {
  const {context, root, engine} = transcriptRig('tx_', 8);
  context.setPageDepth('compact');
  settle(timersOf(context));
  const anchorId = 'tx_t3_h0';
  const coarse = engineDebug(context, root);
  scrollTo(timersOf(context), root, Math.max(0, coarse.offsets[3] - 100));
  settle(timersOf(context));
  const target = root.querySelector(`[data-message-id="${anchorId}"]`);
  assert.ok(target, 'the anchor turn rendered after the coarse scroll');
  const contentTop = target.getBoundingClientRect().top
    - root.getBoundingClientRect().top + root.scrollTop;
  scrollTo(timersOf(context), root, Math.max(0, Math.floor(contentTop) - 20));
  settle(timersOf(context));
  const before = anchorY(root, anchorId);

  await resetTranscript(context, 'tx_', 8, 'reset-1');
  settle(timersOf(context));

  const remounted = context.Chat.TurnEngine.activeFor(root);
  assert.ok(remounted && remounted.alive, 'a fresh engine owns the view after the reset');
  assert.notEqual(remounted, engine, 'the stale engine was replaced, not reused');
  assert.equal(anchorY(root, anchorId), before, 'the reader kept their message on screen');
  assertEngineInvariants(context, root, root.children[root.children.length - 1], 'after reset');

  // The remounted engine still ingests: the append joins the live tail (out
  // of the reading window, so it materializes when the reader returns there)
  // and the reading position does not move.
  const entriesBefore = engineDebug(context, root).entries;
  remounted.appendMessage(eMsg('assistant', 'tx-after-reset', 'live again'), false);
  settle(timersOf(context));
  const debugAfter = engineDebug(context, root);
  assert.equal(debugAfter.entries, entriesBefore + 1, 'the engine still ingests');
  assert.ok(debugAfter.keys.includes(null), 'the appended message joined a live segment');
  assert.equal(anchorY(root, anchorId), before, 'the live append moved nothing on screen');
});

test('a transcript reset while pinned stays at the bottom', async () => {
  const {context, root} = transcriptRig('txp_', 8);
  settle(timersOf(context));
  assert.equal(distanceFromBottom(root), 0, 'fresh mount is bottom-pinned');

  await resetTranscript(context, 'txp_', 8, 'reset-1');
  settle(timersOf(context));
  assert.equal(distanceFromBottom(root), 0, 'a pinned reader stays pinned through the reset');
  const remounted = context.Chat.TurnEngine.activeFor(root);
  assert.ok(remounted && remounted.pinnedIntent, 'the remount kept the follow intent');
});

test('a hidden container recovers at the reading anchor when shown again', () => {
  const rig = readingRig('hd_', 10, 5);
  // A real browser resets scrollTop across display:none; the fake keeps it,
  // so the reset is applied by hand before the re-show.
  rig.root.clientHeight = 0;
  rig.root.scrollTop = 0;
  rig.root.clientHeight = 300;
  rig.engine.handleResize();
  settle(rig.timers);

  assert.equal(anchorY(rig.root, rig.anchorId), rig.anchorY,
      'the re-shown view re-entered at the reading position');
  assert.ok(rig.root.scrollTop > 0, 'the recovery left the top of the history');
});

test('a viewport resize keeps the reading message where it was', () => {
  const rig = readingRig('rs2_', 10, 5);
  rig.root.clientHeight = 500;
  rig.engine.handleResize();
  assert.equal(anchorY(rig.root, rig.anchorId), rig.anchorY,
      'the message held its viewport offset through the resize');
});

test('folding a turn above the reader keeps the reading message in place', () => {
  const rig = readingRig('fd_', 10, 6);
  const aboveKey = 'fd_t3_h0|fd_t3_c0|fd_t3_p0';
  rig.engine.setOverride(aboveKey, false);
  settle(rig.timers);
  assert.equal(anchorY(rig.root, rig.anchorId), rig.anchorY,
      'the fold above the viewport was compensated');
});

test('reading folded history in outline mode anchors the turn row', () => {
  const state = mountEngine(ePage('ol_', 12), {clientHeight: 300, clampScrollTop: true});
  settle(state.timers);
  const debug = engineDebug(state.context, state.root);
  scrollTo(state.timers, state.root, Math.max(0, debug.offsets[5] - 100));
  settle(state.timers);
  const wrapKey = 'ol_t5_h0|ol_t5_c0|ol_t5_p0';
  const wrapY = () => {
    const wrap = state.root.querySelector(`[data-turn-key="${wrapKey}"]`);
    assert.ok(wrap, 'the read fold row is in the DOM');
    return wrap.getBoundingClientRect().top - state.root.getBoundingClientRect().top;
  };
  const before = wrapY();

  state.engine.prependMessages(ePage('ololder_', 3));
  settle(state.timers);
  assert.equal(wrapY(), before, 'the fold row the reader sat on held its viewport offset');
  assert.ok(state.root.scrollTop < state.root.scrollHeight - state.root.clientHeight,
      'outline pagination did not park the reader at the bottom');
});

test('a depth change keeps the reader on their turn row', () => {
  const rig = readingRig('dp_', 10, 5);
  const wrapKey = 'dp_t5_h0|dp_t5_c0|dp_t5_p0';
  const wrapY = () => {
    const wrap = rig.root.querySelector(`[data-turn-key="${wrapKey}"]`);
    assert.ok(wrap, 'the read turn wrap is in the DOM');
    return wrap.getBoundingClientRect().top - rig.root.getBoundingClientRect().top;
  };
  const before = wrapY();
  for (const depth of ['outline', 'expanded', 'compact']) {
    rig.context.setPageDepth(depth);
    settle(rig.timers);
    assert.ok(Math.abs(wrapY() - before) <= 0.5,
        `depth ${depth} kept the read turn's row at its viewport offset`);
  }
});
