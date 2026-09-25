'use strict';
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');
const { readStatic } = require('./read_static');
const { buildRendererContext } = require('./renderer_vm_context');

// Fake marked: parse output embeds the body text, deterministic per body, so
// a repeat render hands renderChatMath the identical pre-walk HTML.
const FAKE_MARKED_SRC = `
let parseCalls = 0;
globalThis.marked = {
  Renderer: function() { return {}; },
  use() {},
  parse: (s) => { parseCalls++; return '<p>' + s + '</p>'; },
  parseCallCount: () => parseCalls,
};`;

// Fake marked that routes every parse through the registered code renderer,
// the surface the highlight deferral and its flush settle touch.
const FAKE_MARKED_CODE_SRC = `
let parseCalls = 0;
let codeRenderer = null;
globalThis.marked = {
  Renderer: function() { return {}; },
  use(opts) { if (opts.renderer && opts.renderer.code) codeRenderer = opts.renderer.code; },
  parse: (s) => { parseCalls++; return '<pre>' + codeRenderer({ text: s, lang: '', raw: '' }) + '</pre>'; },
  parseCallCount: () => parseCalls,
};`;

function loadRenderer({ withTimers = false, codeParser = false } = {}) {
  const context = buildRendererContext({ withTimers });
  vm.createContext(context);
  vm.runInContext(codeParser ? FAKE_MARKED_CODE_SRC : FAKE_MARKED_SRC, context,
      { filename: 'marked-fake.js' });
  vm.runInContext(readStatic('math-scanner.js'), context, { filename: 'math-scanner.js' });
  vm.runInContext(readStatic('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  return context;
}

// A walk stub that transforms the element the way auto-render does: the output
// is a pure function of the input HTML.
function stubWalk(c) {
  const state = { walks: 0 };
  c.renderMathInElement = (el) => {
    state.walks += 1;
    el.innerHTML = '<span class="katex-walked">' + el.innerHTML + '</span>';
  };
  return state;
}

const el = (c, src) => ({ dataset: { raw: src }, innerHTML: c.renderProseMarkdown(src) });

test('a repeat message render serves the walked bytes without re-walking', () => {
  const c = loadRenderer();
  const w = stubWalk(c);
  const src = 'math $a+b$ end';
  const first = el(c, src);
  c.renderChatMath(first);
  const cold = first.innerHTML;
  assert.equal(w.walks, 1);
  // The upgrade makes the parse memo serve walked bytes, so the rebuilt
  // element is born walked and even the swap is skipped.
  const second = el(c, src);
  c.renderChatMath(second);
  assert.equal(w.walks, 1, 'the repeat render re-ran the walk');
  assert.equal(second.innerHTML, cold, 'the served bytes diverge from the cold walk');
});

test('the walk upgrades the parse memo to the walked bytes', () => {
  const c = loadRenderer();
  const w = stubWalk(c);
  const src = 'math $a+b$ end';
  const plain = c.renderProseMarkdown(src);
  const walkedEl = { dataset: { raw: src }, innerHTML: plain };
  c.renderChatMath(walkedEl);
  assert.equal(w.walks, 1);
  assert.equal(c.renderProseMarkdown(src), walkedEl.innerHTML, 'the parse memo kept the plain bytes');
});

test('a plain-parsed element swaps the walked bytes in without a re-walk', () => {
  const c = loadRenderer();
  const w = stubWalk(c);
  const src = 'math $a+b$ end';
  const plain = c.renderProseMarkdown(src);
  const first = { dataset: { raw: src }, innerHTML: plain };
  c.renderChatMath(first);
  assert.equal(w.walks, 1);
  // A body rendered before the upgrade landed (or after a parse-entry
  // eviction) carries the plain bytes; the cache swaps without re-walking.
  const second = { dataset: { raw: src }, innerHTML: plain };
  c.renderChatMath(second);
  assert.equal(w.walks, 1, 'the swap re-ran the walk');
  assert.equal(second.innerHTML, first.innerHTML);
});

test('distinct pre-walk HTML never collides in the memo', () => {
  const c = loadRenderer();
  const w = stubWalk(c);
  const a = el(c, 'math $a+b$ end');
  const b = el(c, 'other $c+d$ end');
  c.renderChatMath(a);
  c.renderChatMath(b);
  assert.equal(w.walks, 2);
  assert.notEqual(a.innerHTML, b.innerHTML);
});

test('the streamed draft shape stays off the memo', () => {
  const c = loadRenderer();
  const w = stubWalk(c);
  const d1 = { dataset: {}, innerHTML: '<p>growing $a$</p>' };
  const d2 = { dataset: {}, innerHTML: '<p>growing $a$</p>' };
  c.renderChatMath(d1, 'draft $a$ text');
  c.renderChatMath(d2, 'draft $a$ text');
  assert.equal(w.walks, 2, 'the draft shape re-walked');
  assert.equal(d2.innerHTML, d1.innerHTML);
});

test('a math-free body still skips before the memo', () => {
  const c = loadRenderer();
  const w = stubWalk(c);
  const free = { dataset: { raw: 'plain text' }, innerHTML: '<p>plain</p>' };
  c.renderChatMath(free);
  assert.equal(w.walks, 0);
  assert.equal(free.innerHTML, '<p>plain</p>');
});

test('the flush settle re-keys the walked entry onto the settled parse bytes', () => {
  const c = loadRenderer({ withTimers: true, codeParser: true });
  const w = stubWalk(c);
  const src = 'math $a$ then\n\n```\nconst x = 1\n```\n';
  const first = el(c, src);
  c.renderChatMath(first);
  assert.equal(w.walks, 1);
  assert.match(first.innerHTML, /data-hl="/);
  c.__runTimers(); // the flush settles the parse entry and re-keys the walked entry
  const second = el(c, src);
  c.renderChatMath(second);
  assert.equal(w.walks, 1, 'the settled re-render re-ran the walk');
  assert.doesNotMatch(second.innerHTML, /data-hl/);
  assert.match(second.innerHTML, /katex-walked/);
  // The stub world's settled block is the plain block minus its marker
  // attribute, so the served bytes are the cold walk's bytes with the same
  // attribute dropped.
  assert.equal(second.innerHTML, first.innerHTML.replace(/ data-hl="\d+"/, ''));
});

test('foreign markup falls through to the walk and stays out of the parse memo', () => {
  const c = loadRenderer();
  const w = stubWalk(c);
  const src = 'math $a+b$ end';
  c.renderProseMarkdown(src); // the parse memo holds the plain bytes
  const foreign = { dataset: { raw: src }, innerHTML: '<p>not the parse output</p>' };
  c.renderChatMath(foreign);
  assert.equal(w.walks, 1, 'the foreign element did not walk');
  assert.equal(c.renderProseMarkdown(src), '<p>math $a+b$ end</p>', 'the parse memo was poisoned');
});

test('the LRU cap evicts the least recently walked body', () => {
  const c = loadRenderer();
  const w = stubWalk(c);
  for (let i = 0; i < 64; i++) c.renderChatMath(el(c, 'body $' + i + '$'));
  assert.equal(w.walks, 64);
  c.renderChatMath(el(c, 'body $0$')); // hit; refreshes body 0 past body 1
  assert.equal(w.walks, 64);
  c.renderChatMath(el(c, 'overflow $o$')); // 65th insert evicts body 1
  assert.equal(w.walks, 65);
  c.renderChatMath(el(c, 'body $1$')); // evicted: re-walks
  assert.equal(w.walks, 66);
  c.renderChatMath(el(c, 'body $0$')); // still resident
  assert.equal(w.walks, 66);
});
