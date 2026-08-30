// ---------------------------------------------------------------------------
// showMoreToggleHtml (chat/shared.js) single-sources the truncated-text
// "Show more" toggle for the chat tool-activity renderer (chat/rendering.js)
// and the worker thread event list (workers.js). These tests pin the emitted
// markup through both real renderers: span ids, the inline onclick swap, the
// button classes, and the short/full split at each site's limit.
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
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'sess-1',
    fetch: () => Promise.resolve({ ok: true }),
    _elements: elements,
  };
  vm.createContext(context);
  // Deterministic toggle ids: 0.5.toString(36).slice(2) === 'i'.
  vm.runInContext('Math.random = () => 0.5', context);
  loadChatRenderingModules(context);
  vm.runInContext(readStatic('workers.js'), context, { filename: 'workers.js' });
  return context;
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

function renderWorkerEvents(ctx, events) {
  const parent = new FakeElement('div');
  const container = new FakeElement('div');
  parent.appendChild(container);
  ctx._elements.set('thread-events-t-1', container);
  ctx.renderThreadEvents('t-1', events);
  return container.innerHTML;
}

test('worker event list toggles assistant, tool_use, and tool_result overflow', () => {
  const ctx = loadContext();
  const html = renderWorkerEvents(ctx, [
    { type: 'assistant', content: 'a'.repeat(350) },
    { type: 'tool_use', tool_name: 'Bash', input: { command: 'x'.repeat(90) } },
    { type: 'tool_result', content: 'y'.repeat(600) },
  ]);
  assert.ok(html.includes(
    '<div class="text-sm text-slate-300">' + 'a'.repeat(300) + toggleHtml('evt-more-i', 'a'.repeat(50)) + '</div>'));
  assert.ok(html.includes(
    'flex-1">' + 'x'.repeat(80) + toggleHtml('tu-i', 'x'.repeat(10)) + '</span>'));
  assert.ok(html.includes(
    '<pre class="text-xs text-slate-500 whitespace-pre-wrap break-all">'
    + 'y'.repeat(500) + toggleHtml('tr-more-i', 'y'.repeat(100)) + '</pre>'));
});

test('worker event list within limits renders no toggle', () => {
  const ctx = loadContext();
  const html = renderWorkerEvents(ctx, [
    { type: 'assistant', content: 'short answer' },
    { type: 'tool_use', tool_name: 'Bash', input: { command: 'ls' } },
    { type: 'tool_result', content: 'ok' },
  ]);
  assert.ok(!html.includes('Show more'));
});
