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
  openAllTurns,
  fakeEngineNode,
  loadChatContext,
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

// The same Y without the presence assert: null is the folded-out-of-DOM state
// a manual collapse puts the reading message in.
function messageYOrHidden(root, id) {
  const el = root.querySelector(`[data-message-id="${id}"]`);
  return el ? el.getBoundingClientRect().top - root.getBoundingClientRect().top : null;
}

// Field-wise anchor comparison: the engine's anchor objects come from the vm
// context, so deepStrictEqual's prototype check would reject its own twins.
function assertSameAnchor(actual, expected, label) {
  assert.equal(actual.kind, expected.kind, `${label}: kind`);
  assert.equal(actual.id ?? null, expected.id ?? null, `${label}: id`);
  assert.equal(actual.key ?? null, expected.key ?? null, `${label}: key`);
  assert.ok(Math.abs(actual.offset - expected.offset) <= 0.5, `${label}: offset`);
}

// A mid-history reader: genuine scroll events (the wheel path), settled
// pre-render, and the anchor message sitting just below the viewport top.
// The scroll target is the message's real box (a wheel gesture moves real
// pixels; the height model's estimates are not where content lives). The rig
// opens every turn the reader's way, matching a reader who opened their
// history; the fold-row anchor case has its own test.
function readingRig(prefix, turns, anchorTurn = Math.floor(turns / 2)) {
  const state = mountEngine(ePage(prefix, turns), {clientHeight: 300, clampScrollTop: true});
  openAllTurns(state.engine);
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
  openAllTurns(state.engine);
  settle(state.timers);
  assert.equal(state.engine.pinnedIntent, true, 'mounted pinned');
  const pinnedTop = state.root.scrollTop;

  state.root.scrollHeight = state.root.scrollHeight - 100;
  state.root.fire('scroll');
  assert.ok(state.root.scrollTop < pinnedTop, 'the clamp moved the reported position up');
  assert.equal(state.engine.pinnedIntent, true, 'the shrink echo kept the pin intent');

  // The same clamp over a grown viewport: the scroll range shrinks while the
  // content stands still (the status strip hiding pulls maxScroll down under
  // a bottom reader). The reported move is the clamp, not the wheel.
  state.root.clientHeight += 42;
  state.root.fire('scroll');
  assert.equal(state.engine.pinnedIntent, true, 'a range clamp over a bottom reader kept the pin');

  // With the range stable again, the same-sized move is a genuine wheel-up.
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
  openAllTurns(state.engine);
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
  openAllTurns(state.engine);
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
  // node a real reader would be holding. A collapsed steps band's rows have
  // no box (their rect reads (0, 0)), so they are not holders.
  const cTop = state.root.getBoundingClientRect().top;
  const anchorEl = [...state.root.querySelectorAll('[data-message-id]')]
    .filter((el) => el.getBoundingClientRect().height > 0)
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
  openAllTurns(state.engine);
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
  const engine = context.Chat.activeTurnEngineFor(root);
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
  openAllTurns(engine);
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

  const remounted = context.Chat.activeTurnEngineFor(root);
  assert.ok(remounted && remounted.alive, 'a fresh engine owns the view after the reset');
  assert.notEqual(remounted, engine, 'the stale engine was replaced, not reused');
  // The remount's fresh registry folds the turns the reader had opened back
  // to one row per turn: the reading message hides and its fold row carries
  // the place at the reading offset.
  assertSameAnchor(remounted.readingAnchor, {kind: 'message', id: anchorId, offset: before},
      'the reset kept the reading identity');
  assert.equal(messageYOrHidden(root, anchorId), null, 'the reset folded the reading message away');
  const wrapKey = 'tx_t3_h0|tx_t3_c0|tx_t3_p0';
  const rowY = () => {
    const wrap = root.querySelector(`[data-turn-key="${wrapKey}"]`);
    assert.ok(wrap, 'the read turn\'s fold row is in the DOM');
    return wrap.getBoundingClientRect().top - root.getBoundingClientRect().top;
  };
  assert.ok(Math.abs(rowY() - before) <= 0.5, 'the fold row took the reading offset');
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
  assert.ok(Math.abs(rowY() - before) <= 0.5, 'the live append moved nothing on screen');
});

test('a transcript reset while pinned stays at the bottom', async () => {
  const {context, root} = transcriptRig('txp_', 8);
  settle(timersOf(context));
  assert.equal(distanceFromBottom(root), 0, 'fresh mount is bottom-pinned');

  await resetTranscript(context, 'txp_', 8, 'reset-1');
  settle(timersOf(context));
  assert.equal(distanceFromBottom(root), 0, 'a pinned reader stays pinned through the reset');
  const remounted = context.Chat.activeTurnEngineFor(root);
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

// The reading place across a collapse and reopen is the reading content's
// identity, not whatever node the previous collapse left at the top edge:
// the message holds its viewport offset whenever it is rendered, its fold row
// takes the offset while the turn hides it, and the message returns to the
// offset when the turn reopens — with no user scroll in between.
function foldRowRig(prefix) {
  const rig = readingRig(prefix, 10, 5);
  const wrapKey = `${prefix}t5_h0|${prefix}t5_c0|${prefix}t5_p0`;
  const rowY = () => {
    const wrap = rig.root.querySelector(`[data-turn-key="${wrapKey}"]`);
    assert.ok(wrap, 'the read turn wrap is in the DOM');
    return wrap.getBoundingClientRect().top - rig.root.getBoundingClientRect().top;
  };
  return {...rig, wrapKey, rowY};
}

test('collapsing and reopening the read turn keeps the reading identity through repeats', () => {
  const rig = foldRowRig('dp_');
  const anchor = {...rig.engine.readingAnchor};
  assert.equal(anchor.kind, 'message');
  assert.equal(anchor.id, rig.anchorId);
  assert.equal(anchor.offset, rig.anchorY);
  for (const open of [false, true, false]) {
    rig.engine.setOverride(rig.wrapKey, open);
    settle(rig.timers);
    assertSameAnchor(rig.engine.readingAnchor, anchor,
        `${open ? 'reopen' : 'collapse'} kept the reading identity`);
    if (!open) {
      assert.equal(messageYOrHidden(rig.root, rig.anchorId), null,
          'the collapse folded the reading message away');
      assert.ok(Math.abs(rig.rowY() - rig.anchorY) <= 0.5,
          'the fold row took the reading message\'s viewport offset');
    } else {
      const held = messageYOrHidden(rig.root, rig.anchorId);
      assert.ok(held != null && Math.abs(held - rig.anchorY) <= 0.5,
          'the reopen put the reading message back at its offset');
    }
  }
});

test('a fold that cannot reach the reading offset records the clamp and still restores', () => {
  // The reader collapses their open history from the read turn down, and the
  // fold shrinks the document below one viewport: no scrollTop can deliver
  // the reading offset. The engine records the limit against the target
  // instead of letting the next reopen re-anchor on whatever row the clamp
  // left.
  const state = mountEngine(ePage('cl_', 6), {clientHeight: 1200, clampScrollTop: true});
  openAllTurns(state.engine);
  settle(state.timers);
  scrollTo(state.timers, state.root, state.root.scrollTop - 600);
  settle(state.timers);
  const anchor = {...state.engine.readingAnchor};
  assert.equal(anchor.kind, 'message');
  const msgY = () => messageYOrHidden(state.root, anchor.id);

  const turnKeys = state.engine.segments.filter((seg) => seg.kind === 'turn').map((seg) => seg.key);
  const anchorTurn = state.engine.segments.find((seg) => seg.kind === 'turn'
      && seg.entries.some((entry) => entry.msg && entry.msg.id === anchor.id)).key;
  const folded = turnKeys.slice(turnKeys.indexOf(anchorTurn));
  folded.forEach((key) => state.engine.setOverride(key, false));
  settle(state.timers);
  assertSameAnchor(state.engine.readingAnchor, anchor, 'the fold kept the identity');
  assert.equal(msgY(), null, 'the fold hid the reading message');
  const clamp = state.engine.lastRestoreClamp;
  assert.ok(clamp, 'the impossible restore was recorded');
  assert.equal(clamp.id, anchor.id);
  assert.equal(clamp.offset, anchor.offset);
  assert.ok(clamp.requestedScrollTop > clamp.limitedScrollTop,
      `the record shows the limit against the target (${JSON.stringify(clamp)})`);
  assert.equal(clamp.limitedScrollTop, state.root.scrollTop,
      'the recorded limit is the position the browser kept');

  folded.forEach((key) => state.engine.setOverride(key, true));
  settle(state.timers);
  assertSameAnchor(state.engine.readingAnchor, anchor, 'the reopen kept the identity');
  const held = msgY();
  assert.ok(held != null && Math.abs(held - anchor.offset) <= 0.5,
      'the reading message returned to its offset once the space came back');
});

test('a user scroll between a collapse and a reopen re-decides the reading place', () => {
  const state = mountEngine(ePage('us_', 12), {clientHeight: 300, clampScrollTop: true});
  openAllTurns(state.engine);
  settle(state.timers);
  // The reader collapses their history back to one row per turn, newest open.
  const newest = state.engine.newestFinishedTurnKey();
  state.engine.segments.forEach((seg) => {
    if (seg.kind === 'turn' && seg.key !== newest) state.engine.setOverride(seg.key, false);
  });
  settle(state.timers);
  assert.equal(distanceFromBottom(state.root), 0,
      'a pinned reader stays at the bottom through the collapse');
  // The reader wheels on: the scroll capture reads the fold row they park
  // on. The wheel journey materializes the rows on the way, and the park
  // target is the row's laid-out box (a wheel moves real pixels; the height
  // model's folded-row estimates are not where the rows live).
  const parkedKey = 'us_t4_h0|us_t4_c0|us_t4_p0';
  scrollTo(state.timers, state.root, 300);
  settle(state.timers);
  const row = state.root.querySelector(`[data-turn-key="${parkedKey}"]`);
  assert.ok(row, 'a fold row is rendered to park on');
  scrollTo(state.timers, state.root,
      row.getBoundingClientRect().top + state.root.scrollTop);
  settle(state.timers);
  const parked = {...state.engine.readingAnchor};
  assert.equal(parked.kind, 'turn', 'the reader parked on a fold row');
  assert.equal(parked.key, parkedKey);
  assert.ok(Math.abs(parked.offset) <= 0.5, 'the parked row sits at the top edge');
  const wrapY = (openExpected) => {
    const wrap = state.root.querySelector(`[data-turn-key="${parked.key}"]`);
    assert.ok(wrap, 'the parked turn is projected');
    assert.equal(wrap.dataset.turnOpen, String(openExpected));
    return wrap.getBoundingClientRect().top - state.root.getBoundingClientRect().top;
  };

  state.engine.setOverride(parkedKey, true);
  settle(state.timers);
  assertSameAnchor(state.engine.readingAnchor, parked,
      'the reopen kept the row the reader chose');
  assert.ok(Math.abs(wrapY('true') - parked.offset) <= 0.5,
      'the reopen opened the parked turn at the parked offset');
  state.engine.setOverride(parkedKey, false);
  settle(state.timers);
  assertSameAnchor(state.engine.readingAnchor, parked, 'the identity survives the fold back');
  assert.ok(Math.abs(wrapY('false') - parked.offset) <= 0.5,
      'the collapse folded the parked turn back to the parked offset');
});

test('folding the read turn parks its row at the reading offset and reopening restores the message', () => {
  const rig = foldRowRig('sf_');
  const anchor = {...rig.engine.readingAnchor};
  assert.equal(anchor.id, rig.anchorId);
  rig.engine.setOverride(rig.wrapKey, false);
  settle(rig.timers);
  assertSameAnchor(rig.engine.readingAnchor, anchor, 'the collapse kept the identity');
  assert.equal(messageYOrHidden(rig.root, rig.anchorId), null, 'the collapse hid the message');
  assert.ok(Math.abs(rig.rowY() - rig.anchorY) <= 0.5,
      'the fold row took the reading offset while the turn stayed collapsed');

  rig.engine.setOverride(rig.wrapKey, true);
  settle(rig.timers);
  assertSameAnchor(rig.engine.readingAnchor, anchor, 'the reopen kept the identity');
  const held = messageYOrHidden(rig.root, rig.anchorId);
  assert.ok(held != null && Math.abs(held - rig.anchorY) <= 0.5,
      'reopening put the reading message back at its offset');
});

// ---------------------------------------------------------------------------
// The status strip sits above the list in the page column: selecting the
// Status view hides it and moves the list's top edge by the strip's height
// plus its margin. The reading anchor must be a node that moves with the
// content — a hidden row (a collapsed steps band's messages) has no box, its
// rect reads (0, 0), and anchoring it makes every strip move come back as a
// scroll correction of exactly the strip's height.
// ---------------------------------------------------------------------------

// A column rig: the strip element above the list, inside one parent, so
// hiding the strip moves the container's top the way the page's does. The
// message builder hides the step messages the way the N-steps band does.
function stripRig(prefix, turns) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  root.clientHeight = 300;
  installScrollTopClamp(root);
  const stream = new FakeElement('DIV', {id: 'streaming-msg'});
  root.appendChild(stream);
  const strip = new FakeElement('DIV', {id: 'chat-status-panel'});
  strip.__baseHeight = 42;
  const column = new FakeElement('DIV');
  column.appendChild(strip);
  column.appendChild(root);
  const timers = makeEngineTimers();
  const {context} = loadChatContext({
    createTreeWalker() {
      return {};
    },
    getElementById(id) {
      if (id === 'messages') return root;
      if (id === 'streaming-msg') return stream;
      return null;
    },
  });
  installEngineTimers(context, timers);
  context.fetch = async () => ({ok: false, status: 500, json: async () => ({})});
  context.Chat.buildTurnEngineMessageNode = (msg) => {
    const el = fakeEngineNode(msg);
    if (/^step/.test(msg.content)) el.classList.add('hidden');
    return el;
  };
  const engine = context.Chat.TurnEngine.mountIfAvailable(root, ePage(prefix, turns), 'sess-strip');
  assert.ok(engine && engine.alive, 'the engine mounts the chat container');
  return {context, root, strip, timers, engine};
}

// A mid-history reader whose top edge cuts an open turn between its head and
// its conclusion, with the turn's hidden step rows between them in tree
// order — the exact scan state the collapsed steps band creates.
function stripReadingRig(prefix, turns, anchorTurn = Math.floor(turns / 2)) {
  const rig = stripRig(prefix, turns);
  openAllTurns(rig.engine);
  settle(rig.timers);
  const anchorId = `${prefix}t${anchorTurn}_h0`;
  // Stage one: a coarse genuine scroll toward the anchor's model offset, the
  // way a wheel journey materializes the turns around it. Stage two: the fine
  // scroll that parks the top edge inside the anchor turn.
  const coarse = engineDebug(rig.context, rig.root);
  scrollTo(rig.timers, rig.root, Math.max(0, coarse.offsets[anchorTurn] - 100));
  settle(rig.timers);
  const head = rig.root.querySelector(`[data-message-id="${anchorId}"]`);
  assert.ok(head, 'the anchor turn rendered after the coarse scroll');
  const contentTop = head.getBoundingClientRect().top
    - rig.root.getBoundingClientRect().top + rig.root.scrollTop;
  scrollTo(rig.timers, rig.root, Math.floor(contentTop) + 20);
  settle(rig.timers);
  return {...rig, anchorId};
}

const stripAnchorY = (root, id) => {
  const el = root.querySelector(`[data-message-id="${id}"]`);
  assert.ok(el, `message ${id} is in the DOM`);
  return el.getBoundingClientRect().top - root.getBoundingClientRect().top;
};

test('a hidden step row never carries the reading position across a strip move', () => {
  const rig = stripReadingRig('st_', 10, 5);
  const anchor = rig.engine.readingAnchor;
  assert.equal(anchor.kind, 'message', 'the reader holds a message');
  assert.equal(anchor.id, rig.anchorId, 'the anchor is the visible straddling head');
  const before = rig.root.scrollTop;
  const headY = stripAnchorY(rig.root, rig.anchorId);

  // Status selected: the strip hides and the list's top edge moves up.
  rig.strip.classList.add('hidden');
  rig.engine.handleResize();
  settle(rig.timers);
  assert.equal(rig.root.scrollTop, before, 'the strip hide moved the scroll nothing');
  assert.equal(stripAnchorY(rig.root, rig.anchorId), headY,
      'the read message held its viewport offset');

  // Back to the message view: the strip shows again.
  rig.strip.classList.remove('hidden');
  rig.engine.handleResize();
  settle(rig.timers);
  assert.equal(rig.root.scrollTop, before, 'the round trip returned the exact scrollTop');
  assert.equal(stripAnchorY(rig.root, rig.anchorId), headY,
      'the read message held its viewport offset through the round trip');
});

test('a bottom-pinned reader stays pinned through a strip round trip', () => {
  const rig = stripRig('stp_', 8);
  settle(rig.timers);
  assert.equal(distanceFromBottom(rig.root), 0, 'fresh mount is bottom-pinned');

  rig.strip.classList.add('hidden');
  rig.engine.handleResize();
  settle(rig.timers);
  assert.equal(distanceFromBottom(rig.root), 0, 'the strip hide kept the pin');

  rig.strip.classList.remove('hidden');
  rig.engine.handleResize();
  settle(rig.timers);
  assert.equal(distanceFromBottom(rig.root), 0, 'the strip show kept the pin');
});

// ---------------------------------------------------------------------------
// The Status view covers the laid-out list, so the engine keeps projecting
// under it and a side row (the thinking row, the tray) showing or hiding
// changes the list's height the way any resize does. The page saves the
// reader's place at open (readingPlace) and writes it back at close
// (restoreReadingPlace); these cases drive the two engine methods the way the
// status panel does. A side row's height stands in for the row: the list
// grows when the row disappears and shrinks when it comes back.
// ---------------------------------------------------------------------------

// A mid-history reader whose place the page would save at a Status open.
function statusOpenRig(prefix, turns, anchorTurn = Math.floor(turns / 2)) {
  const state = mountEngine(ePage(prefix, turns), {clientHeight: 300, clampScrollTop: true});
  openAllTurns(state.engine);
  settle(state.timers);
  const coarse = engineDebug(state.context, state.root);
  scrollTo(state.timers, state.root, Math.max(0, coarse.offsets[anchorTurn] - 100));
  settle(state.timers);
  const anchorId = `${prefix}t${anchorTurn}_h0`;
  const target = state.root.querySelector(`[data-message-id="${anchorId}"]`);
  assert.ok(target, 'the anchor turn rendered after the coarse scroll');
  const contentTop = target.getBoundingClientRect().top
    - state.root.getBoundingClientRect().top + state.root.scrollTop;
  scrollTo(state.timers, state.root, Math.max(0, Math.floor(contentTop) - 20));
  settle(state.timers);
  const place = state.engine.readingPlace();
  assert.equal(place.pinned, false, 'the reader is in history');
  return {...state, anchorId, place,
    beforeTop: state.root.scrollTop, beforeY: anchorY(state.root, anchorId)};
}

test('a side row appearing and disappearing under the Status view moves nothing on the return', () => {
  const rig = statusOpenRig('sv_', 10, 5);

  // The thinking row appears (the message region shrinks), then disappears.
  rig.root.clientHeight -= 36;
  rig.engine.handleResize();
  settle(rig.timers);
  rig.root.clientHeight += 36;
  rig.engine.handleResize();
  settle(rig.timers);

  rig.engine.restoreReadingPlace(rig.place);
  assert.equal(rig.root.scrollTop, rig.beforeTop, 'the return wrote the open-time scrollTop back');
  assert.equal(anchorY(rig.root, rig.anchorId), rig.beforeY,
      'the read message holds its viewport offset');
});

// A near-bottom reader for the clamp cases: mounted pinned, then one upward
// wheel to 10px above the bottom — the only way a reader sits that close to
// the bottom with the pin intent off.
function nearBottomRig(prefix) {
  const state = mountEngine(ePage(prefix, 8), {clientHeight: 300, clampScrollTop: true});
  openAllTurns(state.engine);
  settle(state.timers);
  const bottom = state.root.scrollHeight - state.root.clientHeight;
  scrollTo(state.timers, state.root, bottom - 10);
  settle(state.timers);
  const place = state.engine.readingPlace();
  assert.equal(place.pinned, false, 'the wheel-up cleared the pin');
  return {...state, place, bottom, openTop: state.root.scrollTop};
}

test('a row lost below the list clamps the return to the reachable bottom, pin intent stays off', () => {
  const rig = nearBottomRig('sw_');
  const anchorId = rig.place.readingAnchor.id;

  // Status open: a 36px row below the list disappears; the browser clamps the
  // out-of-range position and reports the echo, and the resize re-projects.
  rig.root.clientHeight += 36;
  rig.root.fire('scroll');
  settle(rig.timers);
  rig.engine.handleResize();
  settle(rig.timers);

  rig.engine.restoreReadingPlace(rig.place);
  const max = rig.root.scrollHeight - rig.root.clientHeight;
  assert.equal(rig.root.scrollTop, max, 'the return stopped at the reachable bottom');
  assert.equal(rig.engine.pinnedIntent, false, 'the clamp did not re-arm the pin');
  const clamp = rig.engine.lastRestoreClamp;
  assert.ok(clamp, 'the impossible restore was recorded');
  assert.equal(clamp.id, anchorId);
  assert.ok(Math.abs((clamp.requestedScrollTop - clamp.limitedScrollTop) - 26) <= 0.5,
      `the requested position exceeded the limit by the lost 26px (${JSON.stringify(clamp)})`);
  const heldY = anchorY(rig.root, anchorId);
  assert.ok(heldY >= 0 && heldY < rig.root.clientHeight, 'the anchored message stays in the viewport');
  assert.notEqual(rig.root.scrollTop, rig.openTop, 'the reachable bottom sits below the open-time position');
});

test('the lost row returning before the switch restores the exact open-time position', () => {
  const rig = nearBottomRig('sx_');
  const anchorId = rig.place.readingAnchor.id;
  const openY = anchorY(rig.root, anchorId);

  // The row disappears, then comes back before the reader returns.
  rig.root.clientHeight += 36;
  rig.root.fire('scroll');
  settle(rig.timers);
  rig.root.clientHeight -= 36;
  rig.engine.handleResize();
  settle(rig.timers);

  rig.engine.restoreReadingPlace(rig.place);
  assert.equal(rig.root.scrollTop, rig.openTop, 'the return sat at the open-time scrollTop');
  assert.equal(anchorY(rig.root, anchorId), openY, 'the read message holds its open-time offset');
});

test('a pinned reader returns onto the newest message after arrivals under the Status view', () => {
  const state = mountEngine(ePage('pn_', 6), {clientHeight: 300, clampScrollTop: true});
  settle(state.timers);
  assert.equal(state.engine.pinnedIntent, true, 'mounted pinned');
  const place = state.engine.readingPlace();
  assert.equal(place.pinned, true, 'the saved place holds the pin');

  state.engine.appendMessage(eMsg('assistant', 'pn-live', 'arrived under the Status view'), false);
  settle(state.timers);

  state.engine.restoreReadingPlace(place);
  assert.equal(state.engine.pinnedIntent, true, 'the return kept the follow');
  assert.equal(distanceFromBottom(state.root), 0, 'the return sits on the newest message');
});

test('a For-you history reader returns to the saved message and offset after a round trip', () => {
  const state = mountEngine(ePage('fp_', 8), {clientHeight: 300, clampScrollTop: true, view: 'foryou'});
  settle(state.timers);
  assert.equal(state.engine.view, 'foryou', 'mounted in For you');
  // The reader wheels up into history.
  scrollTo(state.timers, state.root, 200);
  settle(state.timers);
  assert.equal(state.engine.pinnedIntent, false, 'the reader left the bottom');
  const cTop = state.root.getBoundingClientRect().top;
  const anchorEl = [...state.root.querySelectorAll('[data-message-id]')]
    .filter((el) => el.getBoundingClientRect().height > 0)
    .find((el) => el.getBoundingClientRect().top - cTop >= -4);
  assert.ok(anchorEl, 'the viewport shows a message to hold');
  const anchorId = anchorEl.dataset.messageId;
  const anchorTop = anchorEl.getBoundingClientRect().top - cTop;

  state.engine.setView('everything');
  settle(state.timers);
  state.engine.setView('foryou');
  settle(state.timers);

  const held = state.root.querySelector(`[data-message-id="${anchorId}"]`);
  assert.ok(held, 'the read message shows in For you again');
  const heldY = held.getBoundingClientRect().top - cTop;
  assert.ok(Math.abs(heldY - anchorTop) <= 0.5, 'the return restored the saved message and offset');
});

test('a first entry into For you has no saved place and pins the bottom', () => {
  const state = mountEngine(ePage('fe_', 8), {clientHeight: 300, clampScrollTop: true});
  settle(state.timers);
  assert.equal(state.engine.view, 'everything', 'mounted in Everything');
  state.engine.setView('foryou');
  settle(state.timers);
  assert.equal(state.engine.pinnedIntent, true, 'no saved place: the entry pins');
  assert.equal(distanceFromBottom(state.root), 0, 'the entry landed at the newest message');
});
