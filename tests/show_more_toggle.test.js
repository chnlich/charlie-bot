// ---------------------------------------------------------------------------
// showMoreToggleHtml (chat/shared.js) single-sources the truncated-text
// "Show more" toggle for the chat tool-activity renderer (chat/rendering.js).
// These tests pin the emitted markup through the real renderer: span ids, the
// inline onclick swap, the button classes, and the short/full split.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadToggleHarness } = require('./chat_rendering_context_stub');

function loadContext() {
  // usage.js stands in as the extra module: the harness loads the real
  // renderer, pins Math.random for deterministic toggle ids, and the deleted
  // workers.js card panel no longer takes part.
  return loadToggleHarness('usage.js', {restoreBottomPin: () => {}});
}

function toggleHtml(id, restHtml) {
  return `<span id="${id}-short">… <button onclick="document.getElementById('${id}-short').style.display='none';document.getElementById('${id}-full').style.display='inline'" class="text-blue-400 hover:underline text-xs">Show more</button></span><span id="${id}-full" style="display:none">${restHtml}</span>`;
}

test('showMoreToggleHtml emits the id-anchored swap pair and is exposed bare', () => {
  const ctx = loadContext();
  assert.equal(ctx.Chat.showMoreToggleHtml('k', '<em>rest</em>'), toggleHtml('k', '<em>rest</em>'));
  assert.equal(ctx.showMoreToggleHtml, ctx.Chat.showMoreToggleHtml);
});

test('chat tool-activity summary and output use the toggle past their limits', () => {
  const ctx = loadContext();
  const html = ctx.Chat.renderMessage({
    role: 'assistant',
    content: '',
    tools: [{ name: 'Bash', input: { command: 'q'.repeat(90) }, output: 'z'.repeat(600) }],
  }, 'sess-1');
  assert.ok(html.includes(
    '<span class="text-xs text-slate-400 flex-1 min-w-0">'
    + 'q'.repeat(80) + toggleHtml('ts-i', 'q'.repeat(10)) + '</span>'));
  assert.ok(html.includes(
    '<pre class="mt-1 text-xs text-slate-400 whitespace-pre-wrap break-all">'
    + 'z'.repeat(500) + toggleHtml('to-i', 'z'.repeat(100)) + '</pre>'));
});

test('chat tool-activity within limits renders no toggle', () => {
  const ctx = loadContext();
  const html = ctx.Chat.renderMessage({
    role: 'assistant',
    content: '',
    tools: [{ name: 'Bash', input: { command: 'ls' }, output: 'ok' }],
  }, 'sess-1');
  assert.ok(!html.includes('Show more'));
});
