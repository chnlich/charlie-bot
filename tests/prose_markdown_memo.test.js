const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');
const { readStatic } = require('./read_static');
const { hljsStub } = require('./hljs_stub');
const { buildRendererContext } = require('./renderer_vm_context');

// Fake marked counts parse calls; the Renderer/use surface is what
// markdown-renderer.js touches at load.
const FAKE_MARKED_SRC = `
let parseCalls = 0;
globalThis.marked = {
  Renderer: function() { return {}; },
  use() {},
  parse: (s) => { parseCalls++; return '<p>' + s + '</p>'; },
  parseCallCount: () => parseCalls,
};`;

// Fake marked that routes every parse through the registered code renderer,
// the surface the highlight deferral touches.
const FAKE_MARKED_CODE_SRC = `
let parseCalls = 0;
let codeRenderer = null;
globalThis.marked = {
  Renderer: function() { return {}; },
  use(opts) { if (opts.renderer && opts.renderer.code) codeRenderer = opts.renderer.code; },
  parse: (s) => { parseCalls++; return '<pre>' + codeRenderer({ text: s, lang: '', raw: '' }) + '</pre>'; },
  parseCallCount: () => parseCalls,
};`;

function loadRenderer() {
  const context = buildRendererContext();
  vm.createContext(context);
  vm.runInContext(FAKE_MARKED_SRC, context, { filename: 'marked-fake.js' });
  vm.runInContext(readStatic('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  return context;
}

function loadCodeRenderer() {
  const context = buildRendererContext({ withTimers: true });
  vm.createContext(context);
  vm.runInContext(FAKE_MARKED_CODE_SRC, context, { filename: 'marked-code-fake.js' });
  vm.runInContext(readStatic('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  return context;
}

test('repeat bodies serve from the memo without re-parsing', () => {
  const c = loadRenderer();
  const first = c.renderProseMarkdown('hello **world**');
  const second = c.renderProseMarkdown('hello **world**');
  assert.equal(second, first);
  assert.equal(c.marked.parseCallCount(), 1);
});

test('distinct bodies parse once each and never collide', () => {
  const c = loadRenderer();
  const a = c.renderProseMarkdown('aaa');
  const b = c.renderProseMarkdown('bbb');
  assert.notEqual(a, b);
  assert.equal(c.marked.parseCallCount(), 2);
  assert.equal(c.renderProseMarkdown('aaa'), a);
  assert.equal(c.marked.parseCallCount(), 2);
});

test('the memo composes fixNestedFences into the parse it serves', () => {
  const c = loadRenderer();
  const nested = '````css\nx\n````\n';
  assert.equal(c.renderProseMarkdown(nested), '<p>' + c.fixNestedFences(nested) + '</p>');
  assert.equal(c.marked.parseCallCount(), 1);
});

test('the LRU cap evicts the least recently rendered body', () => {
  const c = loadRenderer();
  for (let i = 0; i < 64; i++) c.renderProseMarkdown('body ' + i);
  assert.equal(c.marked.parseCallCount(), 64);
  c.renderProseMarkdown('body 0'); // hit; refreshes body 0 past body 1 in recency
  assert.equal(c.marked.parseCallCount(), 64);
  c.renderProseMarkdown('overflow'); // 65th insert evicts body 1, the LRU entry
  assert.equal(c.marked.parseCallCount(), 65);
  c.renderProseMarkdown('body 1'); // evicted: re-parses
  assert.equal(c.marked.parseCallCount(), 66);
  c.renderProseMarkdown('body 0'); // still resident: no re-parse
  assert.equal(c.marked.parseCallCount(), 66);
});

test('a code body parses deferred and never runs hljs before the flush', () => {
  const c = loadCodeRenderer();
  let autoCalls = 0;
  c.hljs = { ...hljsStub, highlightAuto: (s) => { autoCalls++; return { value: String(s) }; } };
  const first = c.renderProseMarkdown('body one');
  assert.match(first, /data-hl="/);
  assert.equal(autoCalls, 0);
  assert.equal(c.__timerCount(), 1);
});

test('a body without code blocks schedules no flush', () => {
  const c = loadRenderer();
  c.renderProseMarkdown('plain body');
  assert.equal(c.__timerCount === undefined ? 0 : c.__timerCount(), 0);
});

test('the flush settles the memo entry to the direct render bytes', () => {
  const c = loadCodeRenderer();
  const text = 'body two';
  c.renderProseMarkdown(text);
  c.__runTimers();
  const settled = c.renderProseMarkdown(text);
  const direct = c.marked.parse(c.fixNestedFences(text));
  assert.equal(settled, direct);
  assert.doesNotMatch(settled, /data-hl/);
  assert.equal(c.marked.parseCallCount(), 2); // 1 memo parse + 1 direct; the repeat served the settled entry
});

test('the flush swaps the highlighted bytes into the marker nodes', () => {
  const c = loadCodeRenderer();
  const nodes = [];
  c.document = {
    querySelectorAll(sel) {
      const m = /data-hl="(\d+)"/.exec(sel);
      if (!m) return [];
      const el = { id: m[1], innerHTML: '', removed: false, removeAttribute() { this.removed = true; } };
      nodes.push(el);
      return [el];
    },
  };
  c.renderProseMarkdown('body three');
  c.__runTimers();
  assert.equal(nodes.length, 1);
  assert.equal(nodes[0].innerHTML, 'body three'); // the stub highlight returns the input unchanged
  assert.equal(nodes[0].removed, true);
});

test('a repeat render before the flush re-emits markers the pending flush covers', () => {
  const c = loadCodeRenderer();
  const text = 'body four';
  const first = c.renderProseMarkdown(text);
  const second = c.renderProseMarkdown(text); // memo hit on the not-yet-settled entry
  assert.equal(second, first);
  assert.match(second, /data-hl="/);
  c.__runTimers();
  const settled = c.renderProseMarkdown(text);
  assert.equal(settled, c.marked.parse(c.fixNestedFences(text)));
  assert.doesNotMatch(settled, /data-hl/);
  assert.equal(c.marked.parseCallCount(), 2); // 1 memo parse + 1 direct; the repeat served the settled entry
});

test('a settled body with replacement patterns stays byte-identical to the direct render', () => {
  const c = loadCodeRenderer();
  const text = 'echo $$ and $1 and $& and $` tail';
  c.renderProseMarkdown(text);
  c.__runTimers();
  const settled = c.renderProseMarkdown(text);
  const direct = c.marked.parse(c.fixNestedFences(text));
  assert.equal(settled, direct); // a replacement-string replace would corrupt $$/$&/$`
});

test('the flush sweeps a detached postProcess root the prerender registered', () => {
  const c = loadCodeRenderer();
  c.document = { querySelectorAll: () => [] };
  const el = { innerHTML: '', removed: false, removeAttribute() { this.removed = true; } };
  const root = {
    querySelectorAll(sel) {
      return /data-hl="\d+"/.test(sel) ? [el] : [];
    },
  };
  c.renderProseMarkdown('body five');
  c.scheduleCodeHighlightFlush(root); // registers the root; the first flush is still pending
  c.__runTimers();
  assert.equal(el.innerHTML, 'body five');
  assert.equal(el.removed, true);
  assert.equal(c.__timerCount(), 0); // found on the first pass: no retries
});

test('markers no sweep ever finds give up after the bounded retries', () => {
  const c = loadCodeRenderer();
  c.document = { querySelectorAll: () => [] };
  c.renderProseMarkdown('body six');
  for (let i = 0; i < 10000 && c.__timerCount() > 0; i++) c.__runTimers();
  assert.equal(c.__timerCount(), 0); // the retry loop terminated
  const settled = c.renderProseMarkdown('body six');
  assert.equal(settled, c.marked.parse(c.fixNestedFences('body six')));
  assert.doesNotMatch(settled, /data-hl/); // the memo settled on the first pass regardless
});
