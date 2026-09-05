const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');
const { readStatic } = require('./read_static');
const { hljsStub } = require('./hljs_stub');

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

function loadRenderer() {
  const context = {
    console: { error() {}, warn() {}, log() {} },
    hljs: hljsStub,
    document: { querySelectorAll: () => [] },
    platform: {},
  };
  vm.createContext(context);
  vm.runInContext(FAKE_MARKED_SRC, context, { filename: 'marked-fake.js' });
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
