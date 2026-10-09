// ---------------------------------------------------------------------------
// Late artifact layout — a plan card inserted after the render, an HTML
// artifact frame growing to its document height — follows the turn engine's
// pin intent when an engine hosts #messages, never the 150px geometry band:
// a fitting reply opens at the bottom unpinned, and a late card or frame
// landing there must not yank the view down. The stub has no real engine, so
// the cases install a minimal fake under Chat.TurnEngine.activeFor that
// exposes pinnedIntent and records jumpToBottom calls; the real
// restoreBottomPin (chat/scroll.js) routes the jump through it.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadArtifactsScript } = require('./artifacts_context_stub');
const { makeAnchor, makeProseRoot } = require('./chat_prose_stub');
const { SESSION_DIR } = require('./sessions_root_stub');

const ARTIFACT_HREF = '/absolute_filepath' + SESSION_DIR + '/artifacts/report.html';

// A chat container parked 100px above the bottom: inside shouldAutoScroll's
// 150px band, so the legacy geometry read would follow — the engine's intent
// must be the only follow input.
function makeContainer() {
  return { scrollTop: 600, scrollHeight: 1000, clientHeight: 300 };
}

function installFakeEngine(context, container, pinnedIntent) {
  const engine = {
    pinnedIntent,
    jumpCalls: 0,
    jumpToBottom() {
      this.jumpCalls++;
      container.scrollTop = container.scrollHeight;
    },
  };
  context.Chat.TurnEngine = { activeFor: (el) => (el === container ? engine : null) };
  return engine;
}

// The card markup the renderer builds, read back as an element: the compact
// card flow reads its classes and writes dataset ordinals.
function parseCardElement(html) {
  const classMatch = html.match(/^<div class="([^"]*)"/);
  return { className: classMatch ? classMatch[1] : '', dataset: {}, innerHTML: html };
}

function makeTemplate() {
  return {
    content: { firstElementChild: null },
    set innerHTML(value) {
      this.content.firstElementChild = parseCardElement(String(value));
    },
  };
}

function makeDocument(container, frame) {
  return {
    addEventListener: () => {},
    getElementById: (id) => (id === 'messages' ? container : null),
    querySelector: () => frame || null,
    querySelectorAll: () => [],
    createElement: (tag) => (tag === 'template' ? makeTemplate() : { dataset: {}, style: {} }),
    createTextNode: (value) => ({ nodeType: 3, nodeValue: String(value) }),
    createDocumentFragment: () => ({ nodeType: 11, childNodes: [] }),
  };
}

function loadWithEngine(container, pinnedIntent, frame) {
  const messageHandlers = [];
  const context = loadArtifactsScript({
    document: makeDocument(container, frame),
    withScroll: true,
    window: {
      innerHeight: 800,
      location: { href: 'https://charliebot.example/' },
      addEventListener: (type, fn) => {
        if (type === 'message') messageHandlers.push(fn);
      },
    },
  });
  const engine = installFakeEngine(context, container, pinnedIntent);
  // The geometry helper the no-engine branch reads (chat/shared.js is not
  // loaded here): only reached without an engine, but its absence would turn
  // a base-file run of these cases into a ReferenceError instead of the
  // behavioral failure they pin.
  context.shouldAutoScroll = (el) => el.scrollHeight - el.scrollTop - el.clientHeight < 150;
  return { context, engine, messageHandlers };
}

async function insertPlanCard(context) {
  const { root, parent } = makeProseRoot({ anchors: [makeAnchor(ARTIFACT_HREF)] });
  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(parent.inserted.length, 1, 'the compact card was inserted');
}

test('a plan card insertion with an unpinned engine leaves the scroll position alone', async () => {
  const container = makeContainer();
  const { context, engine } = loadWithEngine(container, false);

  await insertPlanCard(context);

  assert.equal(container.scrollTop, 600, 'the card did not yank the view to the bottom');
  assert.equal(engine.jumpCalls, 0, 'no jump to the bottom went through the engine');
});

test('a plan card insertion with a pinned engine lands at the bottom through jumpToBottom', async () => {
  const container = makeContainer();
  const { context, engine } = loadWithEngine(container, true);

  await insertPlanCard(context);

  assert.equal(engine.jumpCalls, 1, 'the follow routed through the engine');
  assert.equal(container.scrollTop, container.scrollHeight, 'the view ended at the bottom');
});

test('an html-artifact-height message with an unpinned engine leaves the scroll position alone', () => {
  const container = makeContainer();
  const frame = { dataset: { frameId: 'f1' }, style: {} };
  const { engine, messageHandlers } = loadWithEngine(container, false, frame);
  assert.equal(messageHandlers.length, 1, 'the frame height listener is installed');

  messageHandlers[0]({ data: { type: 'html-artifact-height', id: 'f1', height: 400 } });

  assert.equal(frame.style.height, '402px', 'the frame still grew to its document height');
  assert.equal(container.scrollTop, 600, 'the growth did not yank the view to the bottom');
  assert.equal(engine.jumpCalls, 0, 'no jump to the bottom went through the engine');
});

test('an html-artifact-height message with a pinned engine lands at the bottom through jumpToBottom', () => {
  const container = makeContainer();
  const frame = { dataset: { frameId: 'f1' }, style: {} };
  const { engine, messageHandlers } = loadWithEngine(container, true, frame);

  messageHandlers[0]({ data: { type: 'html-artifact-height', id: 'f1', height: 400 } });

  assert.equal(frame.style.height, '402px', 'the frame still grew to its document height');
  assert.equal(engine.jumpCalls, 1, 'the follow routed through the engine');
  assert.equal(container.scrollTop, container.scrollHeight, 'the view ended at the bottom');
});
