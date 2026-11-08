// ---------------------------------------------------------------------------
// Wide-char 2ch boxing (wrapWideChars in markdown-renderer.js) in the memo flush, the tool output pre and the raw
// backend block, through the chat rendering modules.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { FakeElement } = require('./fake_dom');
const { buildRendererContext } = require('./renderer_vm_context');
const { loadChatRenderingModules } = require('./chat_rendering_context_stub');

// Fake marked with the surface markdown-renderer.js touches at load, plus the
// parse the (real, loaded) renderProseMarkdown takes when a chat-surface test
// renders a message body.
const FAKE_MARKED_SRC = `
globalThis.marked = {
  Renderer: function() { return {}; },
  use() {},
  parse: (s) => '<p>' + String(s || '') + '</p>',
};`;

// markdown-renderer.js plus the chat surfaces (namespace, shared, rendering)
// in one context: wrapWideChars is the REAL one here, so the tool-output and
// raw-backend mounts are exercised end to end.
function loadChatSurfaceContext() {
  const elements = new Map();
  const context = {
    console: { error() {}, warn() {}, log() {} },
    document: {
      getElementById(id) {
        if (!elements.has(id)) elements.set(id, new FakeElement());
        return elements.get(id);
      },
      createElement(tag) { return new FakeElement(tag); },
      querySelector() { return null; },
      querySelectorAll: () => [],
    },
    platform: {},
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'sess-1',
    fetch: () => Promise.resolve({ ok: true }),
    _elements: elements,
  };
  vm.createContext(context);
  vm.runInContext('Math.random = () => 0.5', context);
  vm.runInContext(FAKE_MARKED_SRC, context, { filename: 'marked-fake.js' });
  vm.runInContext(readStatic('math-scanner.js'), context, { filename: 'math-scanner.js' });
  vm.runInContext(readStatic('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  loadChatRenderingModules(context);
  return context;
}

// DOM-free textContent: the spans are layout-only, so stripping tags and
// decoding the escapers' entities recovers the code text byte for byte.
function textContent(html) {
  return String(html).replace(/<[^>]*>/g, '')
    .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'").replace(/&amp;/g, '&');
}

test('the flush writes the memo settled bytes (wrapped) to the live DOM', async () => {
  const markedSrc = `
let codeRenderer = null;
globalThis.marked = {
  Renderer: function() { return {}; },
  use(opts) { if (opts.renderer && opts.renderer.code) codeRenderer = opts.renderer.code; },
  parse: (s) => '<pre>' + codeRenderer({ text: s, lang: '', raw: '' }) + '</pre>',
};`;
  const context = buildRendererContext({ withTimers: true });
  vm.createContext(context);
  vm.runInContext(markedSrc, context, { filename: 'marked-code-fake.js' });
  vm.runInContext(readStatic('math-scanner.js'), context, { filename: 'math-scanner.js' });
  vm.runInContext(readStatic('markdown-renderer.js'), context, { filename: 'markdown-renderer.js' });
  const els = [];
  context.document = {
    querySelectorAll(sel) {
      const m = /data-hl="(\d+)"/.exec(sel);
      if (!m) return [];
      const el = { innerHTML: '', removed: false, removeAttribute() { this.removed = true; } };
      els.push(el);
      return [el];
    },
  };
  const text = 'rail \u2500\u2502 stays, \u4e2d\u6587 boxes';
  context.renderProseMarkdown(text);
  assert.equal(els.length, 0); // deferred: nothing written before the flush
  context.__runTimers();
  assert.equal(els.length, 1);
  const settled = context.renderProseMarkdown(text);
  const direct = context.marked.parse(context.fixNestedFences(text));
  assert.equal(settled, direct, 'memo settled bytes stay identical to the direct render');
  const settledInner = /<code class="hljs">([\s\S]*)<\/code>/.exec(settled)[1];
  assert.equal(els[0].innerHTML, settledInner, 'DOM write is byte-identical to the memo entry');
  assert.match(els[0].innerHTML, /<span class="wc2ch">\u4e2d<\/span>/);
  assert.match(els[0].innerHTML, /\u2500\u2502/); // ambiguous rails stay bare
  assert.equal(els[0].removed, true);
});

// --- mounts ③④⑤: tool output, raw backend output, worker dashboard pre ---

test('tool output pre and raw backend block wrap through rendering.js', () => {
  const context = loadChatSurfaceContext();
  const out = 'step \u4e2d\u6587 done\nrail \u2500\u2502 end';
  const html = context.renderMessage({
    role: 'assistant', content: '', tools: [{ name: 'Bash', input: {}, output: out }],
  }, 'sess-1');
  assert.match(html, /<span class="wc2ch">\u4e2d<\/span>/);
  assert.match(html, /whitespace-pre-wrap break-all/);
  assert.equal((html.match(/<span class="wc2ch">/g) || []).length, 2);
  const preOut = /<pre class="mt-1[^>]*>([\s\S]*?)<\/pre>/.exec(html)[1];
  assert.equal(textContent(preOut), out);

  const raw = '<tool_call> payload \u4e2d';
  const rawHtml = context.renderMessage({ role: 'assistant', content: raw }, 'sess-1');
  assert.match(rawHtml, /<span class="wc2ch">\u4e2d<\/span>/);
  assert.match(rawHtml, /literal text/);
});
