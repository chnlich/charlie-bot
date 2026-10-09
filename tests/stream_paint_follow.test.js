// ---------------------------------------------------------------------------
// The stream paint's follow decision: with a turn engine hosting #messages,
// paintStreamDraft reads the engine's shouldFollow() — the pin intent OR the
// follow the open deferred to the next turn — and the paint's restore routes
// through the engine's jumpToBottom. The harness loads the real usage.js,
// renderer and scroll seam; the engine is a minimal fake under
// Chat.TurnEngine.activeFor exposing the method and recording jumpToBottom
// calls.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadStreamPaintContext } = require('./marked_renderer_harness');

function makeStreamDocument(container) {
  const streamingMsg = { classList: { add() {}, remove() {} } };
  const streamingContent = { innerHTML: '' };
  return {
    addEventListener() {},
    querySelectorAll: () => [],
    getElementById(id) {
      if (id === 'streaming-msg') return streamingMsg;
      if (id === 'streaming-content') return streamingContent;
      if (id === 'messages') return container;
      return null;
    },
  };
}

function installFakeEngine(context, container, { pinnedIntent, followNextTurn }) {
  const engine = {
    pinnedIntent,
    followNextTurn,
    jumpCalls: 0,
    shouldFollow() {
      return this.pinnedIntent || this.followNextTurn;
    },
    jumpToBottom() {
      this.jumpCalls++;
      container.scrollTop = container.scrollHeight;
    },
  };
  context.Chat.TurnEngine = { activeFor: (el) => (el === container ? engine : null) };
  return engine;
}

test('a stream paint follows when the open deferred the follow to the next turn', async () => {
  const container = { scrollTop: 100, scrollHeight: 1000, clientHeight: 300 };
  const context = await loadStreamPaintContext({ document: makeStreamDocument(container) });
  const engine = installFakeEngine(context, container, { pinnedIntent: false, followNextTurn: true });

  context.paintStreamDraft({ content: 'a growing reply', thinking: '' });

  assert.equal(engine.jumpCalls, 1, 'the paint followed through the engine');
  assert.equal(container.scrollTop, container.scrollHeight, 'the view ended at the bottom');
});

test('a stream paint does not follow when neither the pin nor the deferred follow is set', async () => {
  const container = { scrollTop: 100, scrollHeight: 1000, clientHeight: 300 };
  const context = await loadStreamPaintContext({ document: makeStreamDocument(container) });
  const engine = installFakeEngine(context, container, { pinnedIntent: false, followNextTurn: false });

  context.paintStreamDraft({ content: 'a growing reply', thinking: '' });

  assert.equal(engine.jumpCalls, 0, 'the paint left the reader where they parked');
  assert.equal(container.scrollTop, 100, 'the scroll position is unchanged');
});
