const assert = require('node:assert/strict');
const test = require('node:test');

const {
  mountEngine,
  settle,
  scrollTo,
  flushRaf,
  ePage,
  engineDebug,
  enableRowGapEmulation,
  styleHeightPx,
} = require('./turn_engine_harness');

// ---------------------------------------------------------------------------
// Row-gap height model: with the container's 12px row gap emulated, a
// wheel-driven window migration must leave the content where the scroll put
// it (the spacer trades height with the migrated rows at their layout
// extent), and jumpToBottom must project the pinned bottom at once. Both
// tests fail on the pre-fix engine: there the model counts no margin (the
// migration steals 12px per migrated row) and the jump writes the scroll
// position without reprojecting the window.
// ---------------------------------------------------------------------------

// Viewport-relative Y of one element in the fake layout (container top = 0).
function viewportY(root, el) {
  return el.getBoundingClientRect().top - root.getBoundingClientRect().top;
}

// The folded row whose model offset straddles `depth` below the window top.
function rowAtDepth(root, debug, depth) {
  const winTop = debug.offsets[debug.window.start];
  let index = debug.window.start;
  while (index < debug.window.end
      && debug.offsets[index] + debug.heights[index] < winTop + depth) index++;
  const key = debug.keys[index];
  const el = root.querySelector(`[data-turn-key="${key}"]`);
  assert.ok(el, `row ${key} is in the DOM`);
  return el;
}

test("a 'scroll' reprojection across the window top keeps the reference row's viewport offset", () => {
  enableRowGapEmulation();
  const state = mountEngine(ePage('gap_', 40), {clientHeight: 300, clampScrollTop: true});
  settle(state.timers);
  assert.ok(state.engine.pinnedIntent, 'mounted pinned');

  // The reference row sits mid-viewport after the scroll and stays inside
  // both windows, so only the migration itself can move it.
  const before = engineDebug(state.context, state.root);
  const winTop = before.offsets[before.window.start];
  const refEl = rowAtDepth(state.root, before, 150);

  // One wheel journey: the scroll position moves first (the gesture's own
  // motion), then the engine's scroll frame migrates the window across it.
  state.root.scrollTop = Math.max(0, winTop - 100);
  const yBefore = viewportY(state.root, refEl);
  state.root.fire('scroll');
  state.timers.now += 16;
  flushRaf(state.timers);
  const after = engineDebug(state.context, state.root);
  assert.ok(after.window.start < before.window.start, 'rows crossed the window top edge');
  const yAfter = viewportY(state.root, refEl);
  assert.ok(
      Math.abs(yAfter - yBefore) <= 1,
      `the migration moved the reference row ${yBefore} -> ${yAfter}`);
});

test('jumpToBottom from far history projects the last segment and snaps to the bottom', () => {
  enableRowGapEmulation();
  const state = mountEngine(ePage('gap_', 40), {clientHeight: 300, clampScrollTop: true});
  settle(state.timers);

  // Park the reader at the very top of the loaded history.
  scrollTo(state.timers, state.root, 0);
  settle(state.timers);
  const parked = engineDebug(state.context, state.root);
  assert.ok(parked.window.end < parked.segments - 1, 'the window left the last segment');

  state.engine.jumpToBottom();
  const jumped = engineDebug(state.context, state.root);
  assert.equal(jumped.window.end, parked.segments - 1, 'the window holds the last segment');
  assert.equal(styleHeightPx(state.engine.bottomSpacer), 0, 'the bottom spacer is 0px');
  assert.equal(
      state.root.scrollTop, state.root.scrollHeight - state.root.clientHeight,
      'the scroll position is the bottom');

  // The write's scroll event is the engine's own echo: it changes nothing.
  state.root.fire('scroll');
  flushRaf(state.timers);
  assert.equal(state.engine.pinnedIntent, true, 'the echo kept the pin');
  assert.equal(
      state.root.scrollTop, state.root.scrollHeight - state.root.clientHeight,
      'the echo moved nothing');
});
