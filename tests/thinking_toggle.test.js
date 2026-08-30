// ---------------------------------------------------------------------------
// thinkingToggleHtml (chat/shared.js) single-sources the collapsed "Thinking…"
// block for the chat assistant bubble (chat/rendering.js) and the live
// streaming draft (usage.js). These tests pin the emitted markup through both
// real renderers: the inline onclick swap, the button/hidden-div classes, the
// escaped thinking text, and each site's id choice.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { loadChatRenderingModules } = require('./chat_rendering_context_stub');

const { FakeElement } = require('./fake_dom');

function loadContext() {
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
    renderChatMath: () => {},
    showScrollToBottom: () => {},
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'sess-1',
    fetch: () => Promise.resolve({ ok: true }),
    _elements: elements,
  };
  vm.createContext(context);
  // Deterministic toggle ids: 0.5.toString(36).slice(2) === 'i'.
  vm.runInContext('Math.random = () => 0.5', context);
  loadChatRenderingModules(context);
  vm.runInContext(readStatic('usage.js'), context, { filename: 'usage.js' });
  return context;
}

function toggleHtml(id, escapedThinking) {
  return `<button onclick="const el=document.getElementById('${id}');el.style.display=el.style.display==='none'?'block':'none'" class="text-xs text-slate-500 hover:text-slate-400 italic mb-1">Thinking…</button><div id="${id}" style="display:none" class="text-xs text-slate-500 whitespace-pre-wrap mb-2">${escapedThinking}</div>`;
}

test('thinkingToggleHtml emits the swap pair with escaped text and is exposed bare', () => {
  const ctx = loadContext();
  assert.equal(ctx.Chat.thinkingToggleHtml('k', 'plan <a> & "b"'), toggleHtml('k', 'plan &lt;a&gt; &amp; "b"'));
  assert.equal(ctx.thinkingToggleHtml, ctx.Chat.thinkingToggleHtml);
});

test('chat assistant bubble renders the toggle with a per-message id', () => {
  const ctx = loadContext();
  const html = ctx.Chat.renderMessage({ role: 'assistant', content: '', thinking: 'mull <x>', id: 'm-7' }, 'sess-1');
  assert.ok(html.includes(toggleHtml('think-m-7', 'mull &lt;x&gt;')));
});

test('chat assistant bubble without a message id falls back to the random id', () => {
  const ctx = loadContext();
  const html = ctx.Chat.renderMessage({ role: 'assistant', content: '', thinking: 'mull' }, 'sess-1');
  assert.ok(html.includes(toggleHtml('think-i', 'mull')));
});

test('chat assistant bubble without thinking renders no toggle', () => {
  const ctx = loadContext();
  const html = ctx.Chat.renderMessage({ role: 'assistant', content: 'plain' }, 'sess-1');
  assert.ok(!html.includes('Thinking…'));
});

function showStreamingHtml(ctx, draft) {
  const streaming = new FakeElement('div');
  const content = new FakeElement('div');
  const messages = new FakeElement('div');
  ctx._elements.set('streaming-msg', streaming);
  ctx._elements.set('streaming-content', content);
  ctx._elements.set('messages', messages);
  ctx.showStreaming(draft);
  return content.innerHTML;
}

test('streaming draft renders the toggle with the fixed singleton id', () => {
  const ctx = loadContext();
  const html = showStreamingHtml(ctx, { content: 'hi', thinking: 'mull & more' });
  assert.ok(html.includes(toggleHtml('streaming-thinking', 'mull &amp; more')));
});

test('streaming draft without thinking renders no toggle', () => {
  const ctx = loadContext();
  const html = showStreamingHtml(ctx, { content: 'hi' });
  assert.ok(!html.includes('Thinking…'));
});
