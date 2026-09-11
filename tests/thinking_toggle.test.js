// ---------------------------------------------------------------------------
// thinkingToggleHtml (chat/shared.js) single-sources the collapsed "Thinking…"
// block for the chat assistant bubble (chat/rendering.js), the live streaming
// draft (usage.js), and the workers thread events (workers.js). These tests
// pin the emitted markup through all three real renderers: the inline onclick
// swap, the button/hidden-div classes, the escaped thinking text, and each
// site's id choice.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadToggleHarness } = require('./chat_rendering_context_stub');

const { FakeElement } = require('./fake_dom');

function loadContext() {
  return loadToggleHarness('usage.js', {showScrollToBottom: () => {}});
}

function toggleHtml(id, escapedThinking) {
  return `<button onclick="const el=document.getElementById('${id}');el.style.display=el.style.display==='none'?'block':'none'" class="text-xs text-slate-500 hover:text-slate-400 italic mb-1">Thinking…</button><div id="${id}" style="display:none" class="text-xs text-slate-500 whitespace-pre-wrap mb-2">${escapedThinking}</div>`;
}

test('thinkingToggleHtml emits the swap pair with escaped text and is exposed bare', () => {
  const ctx = loadContext();
  assert.equal(ctx.Chat.thinkingToggleHtml('k', 'plan <a> & "b"'), toggleHtml('k', 'plan &lt;a&gt; &amp; "b"'));
  assert.equal(ctx.thinkingToggleHtml, ctx.Chat.thinkingToggleHtml);
});

test('thinkingButtonHtml is exposed bare and emits only the button', () => {
  const ctx = loadContext();
  assert.equal(
      ctx.Chat.thinkingButtonHtml('k', 'cls-a cls-b'),
      `<button onclick="const el=document.getElementById('k');el.style.display=el.style.display==='none'?'block':'none'" class="cls-a cls-b">Thinking…</button>`);
  assert.equal(ctx.thinkingButtonHtml, ctx.Chat.thinkingButtonHtml);
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

test('workers thread-event thinking renders the shared button with the workers palette', () => {
  const ctx = loadToggleHarness('workers.js', { fetch: async () => ({ ok: true, json: async () => ({}) }) });
  const container = new FakeElement('div').appendChild(new FakeElement('div'));
  ctx._elements.set('thread-events-t1', container);
  ctx.renderThreadEvents('t1', [{ type: 'thinking', content: 'mull <x>', timestamp: '2026-09-02T10:00:00Z' }]);
  const html = container.innerHTML;
  assert.ok(html.includes(ctx.thinkingButtonHtml('think-i', 'text-xs text-slate-600 hover:text-slate-500 italic')), html);
  assert.ok(
      html.includes(
          `<div id="think-i" style="display:none" class="mt-1 text-xs text-slate-600 whitespace-pre-wrap">mull &lt;x&gt;</div>`),
      html);
});
