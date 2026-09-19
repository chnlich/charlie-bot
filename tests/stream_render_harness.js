'use strict';
// Stream-draft harness: loads the checkout's usage.js (+ markdown-renderer.js)
// in a node vm against a stub DOM, a virtual clock, and a manual timer queue, so
// render scheduling replays deterministically. markedSource selects the marked
// build (collector: the page's CDN build; tests: a fake). stats().frames records
// every painted frame's HTML.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const { hljsStub } = require('./hljs_stub');

// Fake marked for behavior tests: parse marks its input so frames stay
// deterministic; the Renderer/use surface is what markdown-renderer.js touches
// at load, and the lexer/parser pair is what parseStreamDraft's recorder path
// drives.
const FAKE_MARKED_SRC =
  'globalThis.marked = { Renderer: function() { return {}; }, use() {}, parse: (s) => `<p>${s}</p>`, ' +
  'lexer: (s) => [{ type: "paragraph", raw: s, text: s }], ' +
  'parser: (tokens) => tokens.map((t) => `<p>${t.text}</p>`).join("") };';

// Line-cutting fake marked: paragraph tokens carry the line body only and each
// newline rides its own space token, so streamSafeCut's sequential indexOf walk
// locates every raw and freezes cuts at line ends — the incremental reuse path
// engages (FAKE_MARKED_SRC's single paragraph token never cuts, and streaming
// paints drive lexer/parser, never parse).
const CUTTING_MARKED_SRC =
  'globalThis.marked = { Renderer: function() { return {}; }, use() {}, ' +
  'parse: (s) => s.split("\\n").filter(Boolean).map((l) => `<p>${l}</p>`).join(""), ' +
  'lexer: (s) => s.split("\\n").filter(Boolean).flatMap((l) => ' +
  '[{ type: "paragraph", raw: l, text: l }, { type: "space", raw: "\\n" }]), ' +
  'parser: (tokens) => tokens.map((t) => t.type === "space" ? "" : `<p>${t.text}</p>`).join("") };';

// CHECKOUT overrides the code under test (the M33 A/B protocol); the default is
// this harness's own repo root so behavior tests exercise their own checkout.
const CHECKOUT = process.env.CHECKOUT || path.join(__dirname, '..');
const read = (name) => fs.readFileSync(path.join(CHECKOUT, 'web/static/js', name), 'utf8');

function buildStreamHarness(markedSource, options = {}) {
  let virtualNow = 0, paintMs = 0, nextTimerId = 1, htmlStore = '';
  const timers = new Map(), elements = new Map(), frames = [];
  const idleQueue = [];
  let nextIdleId = 1;
  const mkEl = () => ({ classList: { add() {}, remove() {} }, scrollTop: 0, scrollHeight: 0 });
  const context = {
    console: { error() {}, warn() {}, log() {} },
    document: {
      getElementById(id) {
        if (!elements.has(id)) elements.set(id, mkEl());
        return elements.get(id);
      },
      querySelectorAll: () => [],
    },
    shouldAutoScroll: () => false,
    showScrollToBottom() {},
    restoreBottomPin() {},
    // Length-proportional escape stub: the real thinkingToggleHtml's cost shape
    // (regex passes over the full thinking text) without a DOM.
    thinkingToggleHtml: (id, thinking) =>
      '<div>' + String(thinking).replace(/&/g, '&amp;').replace(/</g, '&lt;') + '</div>',
    // KaTeX's per-paint DOM re-walk is stubbed identically wherever the harness
    // runs, so comparisons under- rather than over-state the coalescing win.
    renderMathInElement() {},
    platform: {},
    Date: { now: () => virtualNow },
    setTimeout(fn, ms) {
      const id = nextTimerId++;
      timers.set(id, { fn, due: virtualNow + ms });
      return id;
    },
    clearTimeout(id) { timers.delete(id); },
    // Idle callbacks queue outside the timer map: replayDraft flushes them
    // once after load (the page's idle pre-warm) so they never land inside a
    // timed paint, and tests that never flush simply never run them.
    requestIdleCallback(fn) {
      idleQueue.push(fn);
      return nextIdleId++;
    },
    cancelIdleCallback() {},
  };
  // options.hljsSource loads the page's real highlight.js build; without it the
  // stub keeps the M33 metric on the marked-parse cost alone.
  if (!options.hljsSource) {
    context.hljs = hljsStub;
  }
  vm.createContext(context);
  if (options.hljsSource) {
    vm.runInContext(options.hljsSource, context, { filename: 'highlight.min.js' });
  }
  vm.runInContext(markedSource, context, { filename: 'marked.js' });
  // The checkout's page load order: math-scanner.js defines the mathSpan
  // global the renderer's math tokenizer reads.
  vm.runInContext(read('math-scanner.js'), context, { filename: 'math-scanner.js' });
  vm.runInContext(read('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  vm.runInContext(read('usage.js'), context, { filename: 'usage.js' });

  Object.defineProperty(context.document.getElementById('streaming-content'), 'innerHTML', {
    get() { return htmlStore; },
    set(v) { htmlStore = v; if (v) frames.push(v); },
  });

  const origShow = context.showStreaming;
  const showStreaming = (draft) => {
    const t0 = performance.now();
    origShow(draft);
    paintMs += performance.now() - t0;
  };
  const advance = (ms) => {
    virtualNow += ms;
    for (const [id, timer] of Array.from(timers)) {
      if (timer.due <= virtualNow) {
        timers.delete(id);
        const t0 = performance.now();
        timer.fn();
        paintMs += performance.now() - t0;
      }
    }
  };
  const flushIdle = () => {
    while (idleQueue.length) {
      const fn = idleQueue.shift();
      fn();
    }
  };
  return { context, showStreaming, advance, flushIdle, stats: () => ({ paintMs, timerCount: timers.size, frames }) };
}

module.exports = { buildStreamHarness, FAKE_MARKED_SRC, CUTTING_MARKED_SRC };
