// Load core shared by the chat/rendering.js unit harnesses: namespace.js,
// shared.js, no-op stubs for rendering.js's two cross-module dependencies,
// then rendering.js. The stubs must land before rendering.js loads:
// rendering.js is an IIFE that binds Chat.renderRoundRatingButtons (defined in
// chat/ratings-recap.js) and Chat.embedLinkedHtmlArtifacts (chat/artifacts.js)
// into module-scope consts at load, and these harnesses load neither defining
// module. A harness that needs another module between shared.js and the stubs,
// or a non-empty rating-button stub, keeps its own sequence
// (tailwind_class_coverage.test.js).
const vm = require('node:vm');

const { createEscapingElement } = require('./dom_element_stub');
const { FakeElement } = require('./fake_dom');
const { readStatic } = require('./read_static');

const NAMESPACE_JS = readStatic('chat/namespace.js');
const SHARED_JS = readStatic('chat/shared.js');
const RENDERING_JS = readStatic('chat/rendering.js');

function loadChatRenderingModules(context) {
  vm.runInContext(NAMESPACE_JS, context, {filename: 'chat/namespace.js'});
  vm.runInContext(SHARED_JS, context, {filename: 'chat/shared.js'});
  context.Chat.renderRoundRatingButtons = () => '';
  context.Chat.embedLinkedHtmlArtifacts = () => {};
  vm.runInContext(RENDERING_JS, context, {filename: 'chat/rendering.js'});
}

// The harnesses that assert rendered markup share this context: createElement
// routes through createEscapingElement so the escape path stays observable,
// and attachments.js is not loaded, so renderUserMessageBubble stays a no-op.
// A harness needing a FakeElement document or page globals uses
// makeChatRenderContext instead.
function loadChatRendering() {
  const context = {
    CSS: {escape: (value) => String(value)},
    document: {
      createElement: (tag) => createEscapingElement(tag),
      getElementById: () => null,
      querySelector: () => null,
    },
    marked: {parse: (value) => String(value || '')},
    fixNestedFences: (value) => String(value || ''),
    renderProseMarkdown: (value) => String(value || ''),
    renderChatMath: () => {},
    scheduleCodeHighlightFlush: () => {},
    // Identity stand-in for markdown-renderer.js's wrapWideChars (not loaded
    // here); these fixtures carry no wide chars.
    wrapWideChars: (html) => html,
    renderUserMessageBubble: () => '',
  };
  vm.createContext(context);
  loadChatRenderingModules(context);
  return context;
}

// makeChatRenderContext builds the sandbox shared by the chat/rendering.js vm
// harnesses: a FakeElement-backed document over an element map, plus the page
// globals the chat render paths read, so they run without a browser
// (marked/fixNestedFences/renderProseMarkdown/renderChatMath stand in for
// markdown-renderer.js).
function makeChatRenderContext(elements = new Map()) {
  return {
    document: {
      getElementById(id) { return elements.get(id) || null; },
      createElement(tag) { return new FakeElement(tag); },
      querySelector() { return null; },
      querySelectorAll() { return []; },
    },
    console: { error: () => {}, log: () => {} },
    marked: { parse: (v) => '<p>' + String(v || '') + '</p>' },
    fixNestedFences: (v) => String(v || ''),
    parseStreamDraft: (v) => '<p>' + String(v || '') + '</p>',
    renderProseMarkdown: (v) => '<p>' + String(v || '') + '</p>',
    renderChatMath: () => {},
    scheduleCodeHighlightFlush: () => {},
    // Stand-in for markdown-renderer.js's wrapWideChars (not loaded here):
    // these harnesses' fixtures carry no wide chars, so identity reproduces
    // the real function's bytes on them; wide-char behavior is covered in
    // code_block_wc2ch.test.js against the real renderer.
    wrapWideChars: (html) => html,
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'sess-1',
    confirm: () => true,
    fetch: () => Promise.resolve({ ok: true }),
  };
}

// loadToggleHarness covers the toggle suites (show_more_toggle,
// thinking_toggle): a FakeElement-backed document and the marked/fence/math
// stubs the toggle render paths touch, then the module-load sequence above,
// then one extra module (workers.js or usage.js) whose own deps arrive via
// extraStubs. extraStubs spread onto the context before createContext, so a
// stub is a context global when the extra module's render path reads it
// (usage.js reads the bare identifier showScrollToBottom).
function loadToggleHarness(extraModule, extraStubs = {}) {
  const elements = new Map();
  const context = {
    document: {
      getElementById(id) { return elements.get(id) || null; },
      createElement(tag) { return new FakeElement(tag); },
      querySelector() { return null; },
      querySelectorAll() { return []; },
    },
    console: { error: () => {}, log: () => {}, warn: () => {} },
    marked: { parse: (v) => '<p>' + String(v || '') + '</p>' },
    fixNestedFences: (v) => String(v || ''),
    parseStreamDraft: (v) => '<p>' + String(v || '') + '</p>',
    renderProseMarkdown: (v) => '<p>' + String(v || '') + '</p>',
    renderChatMath: () => {},
    scheduleCodeHighlightFlush: () => {},
    // Identity stand-in for markdown-renderer.js's wrapWideChars — see
    // makeChatRenderContext above.
    wrapWideChars: (html) => html,
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'sess-1',
    fetch: () => Promise.resolve({ ok: true }),
    _elements: elements,
    ...extraStubs,
  };
  vm.createContext(context);
  // Deterministic toggle ids: 0.5.toString(36).slice(2) === 'i'.
  vm.runInContext('Math.random = () => 0.5', context);
  loadChatRenderingModules(context);
  vm.runInContext(readStatic(extraModule), context, {filename: extraModule});
  return context;
}

module.exports = {loadChatRendering, loadChatRenderingModules, loadToggleHarness, makeChatRenderContext};
