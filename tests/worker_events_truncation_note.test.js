// The projection's output_truncated marker (the TOOL_OUTPUT_RENDER_CAP cap)
// renders a truncation note on the tool_result row; an uncapped row renders none.
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

test('capped tool_result rows carry the truncation note; uncapped rows do not', () => {
  const ts = '2026-09-12T00:00:00Z';
  const capped = renderHtml([{type: 'tool_result', tool_name: 'Bash', content: 'x'.repeat(600), output_truncated: true, timestamp: ts}]);
  assert.ok(capped.includes('output truncated'), 'note present on a marked row');
  assert.ok(capped.includes('showMoreToggle') || capped.includes('tr-more-'), 'the capped tail stays expandable');

  const uncapped = renderHtml([{type: 'tool_result', tool_name: 'Bash', content: 'x'.repeat(600), timestamp: ts}]);
  assert.ok(!uncapped.includes('output truncated'), 'no note without the marker');
});
