// The projection trims each tool_result output to the inline-render bound and
// marks the row; the note names the raw events log, and no tail ships to hide
// behind a toggle.
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadToggleHarness } = require('./chat_rendering_context_stub');
const { FakeElement } = require('./fake_dom');

function loadContext() {
  const ctx = loadToggleHarness('workers.js', {
    fetch: async () => ({ok: true, json: async () => ({})}),
  });
  const parent = new FakeElement('div');
  const container = parent.appendChild(new FakeElement('div'));
  ctx._elements.set('thread-events-t1', container);
  return ctx;
}

function renderHtml(events) {
  const ctx = loadContext();
  ctx.renderThreadEvents('t1', events);
  return ctx._elements.get('thread-events-t1').innerHTML;
}

test('marked tool_result rows carry the raw-log note; unmarked rows do not', () => {
  const ts = '2026-09-12T00:00:00Z';
  const marked = renderHtml([{type: 'tool_result', tool_name: 'Bash', content: 'x'.repeat(600), output_truncated: true, timestamp: ts}]);
  assert.ok(marked.includes('output truncated'), 'note present on a marked row');
  assert.ok(!marked.includes('showMoreToggle') && !marked.includes('tr-more-'), 'no hidden tail span: the full text stays in the raw events log');

  const unmarked = renderHtml([{type: 'tool_result', tool_name: 'Bash', content: 'x'.repeat(600), timestamp: ts}]);
  assert.ok(!unmarked.includes('output truncated'), 'no note without the marker');
});
