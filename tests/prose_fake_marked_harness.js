'use strict';
// Harness for the prose-renderer memo suites: a fake marked whose parse embeds
// the body text, plus the loader that runs the real markdown-renderer.js against
// it in a vm context. The fake's Renderer/use surface is what
// markdown-renderer.js touches at load.
const vm = require('node:vm');
const { readStatic } = require('./read_static');
const { buildRendererContext } = require('./renderer_vm_context');

// parse output embeds the body, deterministic per body, so a repeat render
// serves identical bytes; parseCallCount exposes the memo's hit/miss counts.
const FAKE_MARKED_SRC = `
let parseCalls = 0;
globalThis.marked = {
  Renderer: function() { return {}; },
  use() {},
  parse: (s) => { parseCalls++; return '<p>' + s + '</p>'; },
  parseCallCount: () => parseCalls,
};`;

// Routes every parse through the registered code renderer — the surface the
// highlight deferral and its flush settle touch.
const FAKE_MARKED_CODE_SRC = `
let parseCalls = 0;
let codeRenderer = null;
globalThis.marked = {
  Renderer: function() { return {}; },
  use(opts) { if (opts.renderer && opts.renderer.code) codeRenderer = opts.renderer.code; },
  parse: (s) => { parseCalls++; return '<pre>' + codeRenderer({ text: s, lang: '', raw: '' }) + '</pre>'; },
  parseCallCount: () => parseCalls,
};`;

// Loads markdown-renderer.js (plus math-scanner.js, whose mathSpan global the
// renderer's math tokenizer reads) into a fresh renderer context. withTimers
// installs the manual timer queue the flush tests drive; codeParser swaps in
// FAKE_MARKED_CODE_SRC.
function loadRenderer({ withTimers = false, codeParser = false } = {}) {
  const context = buildRendererContext({ withTimers });
  vm.createContext(context);
  vm.runInContext(codeParser ? FAKE_MARKED_CODE_SRC : FAKE_MARKED_SRC, context,
      { filename: 'marked-fake.js' });
  vm.runInContext(readStatic('math-scanner.js'), context, { filename: 'math-scanner.js' });
  vm.runInContext(readStatic('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  return context;
}

module.exports = { FAKE_MARKED_SRC, FAKE_MARKED_CODE_SRC, loadRenderer };
