// Incremental worker-events fetch (workers.js): the 5 s expanded-worker poll
// passes its rendered raw count as ?after=N and appends the returned tail via
// insertAdjacentHTML instead of re-fetching and re-painting the full history.
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadToggleHarness } = require('./chat_rendering_context_stub');
const { FakeElement } = require('./fake_dom');

const E1 = {type: 'assistant', content: 'first reply', timestamp: '2026-09-02T10:00:00Z'};
const E2 = {type: 'tool_result', tool_name: 'Bash', content: 'ok', timestamp: '2026-09-02T10:00:01Z'};
const E3 = {type: 'complete', status: 'completed', message: 'done', timestamp: '2026-09-02T10:00:02Z'};
const E9 = {type: 'assistant', content: 'replacement history', timestamp: '2026-09-02T10:00:09Z'};

// One context per flow: eventsResponses are shifted off per /events call;
// non-events fetches (metadata) answer an empty object.
function loadContext(eventsResponses) {
  const fetches = [];
  const ctx = loadToggleHarness('workers.js', {
    fetch: async (url) => {
      fetches.push(url);
      const body = url.includes('/events?after=') ? eventsResponses.shift() : {};
      return {ok: true, json: async () => body};
    },
  });
  const parent = new FakeElement('div');
  const container = parent.appendChild(new FakeElement('div'));
  ctx._elements.set('thread-events-t1', container);
  return {ctx, container, fetches};
}

function fullRenderHtml(events) {
  const {ctx} = loadContext([]);
  ctx.renderThreadEvents('t1', events);
  return ctx._elements.get('thread-events-t1').innerHTML;
}

test('incremental appends, then a reset envelope, reproduce full renders', async () => {
  const {ctx, container, fetches} = loadContext([
    {events: [E1, E2], total: 2, reset: false},
    {events: [E3], total: 3, reset: false},
    {events: [E9], total: 1, reset: true},
  ]);
  let appends = 0;
  container.insertAdjacentHTML = (pos, html) => {
    appends++;
    FakeElement.prototype.insertAdjacentHTML.call(container, pos, html);
  };

  await ctx.fetchAndRenderEvents('t1', 'sess-9');
  assert.ok(fetches[0].endsWith('/events?after=0'));
  await ctx.fetchAndRenderEvents('t1', 'sess-9');
  assert.ok(fetches[2].endsWith('/events?after=2'), `poll fetched ${fetches[2]}`);
  assert.equal(appends, 1, 'the tail paints through one insertAdjacentHTML, not a repaint');
  assert.equal(container.innerHTML, fullRenderHtml([E1, E2, E3]));

  await ctx.fetchAndRenderEvents('t1', 'sess-9');
  assert.equal(container.innerHTML, fullRenderHtml([E9]), 'a reset envelope replaces the prefix');
});

test('placeholder, renderable tail, then empty tail stay consistent', async () => {
  const {ctx, container, fetches} = loadContext([
    {events: [{type: 'ping'}], total: 1, reset: false},
    {events: [E1], total: 2, reset: false},
    {events: [], total: 2, reset: false},
  ]);
  await ctx.fetchAndRenderEvents('t1', 'sess-9');
  assert.equal(container.innerHTML, '<p class="text-xs text-slate-500">No events</p>');
  await ctx.fetchAndRenderEvents('t1', 'sess-9');
  assert.ok(fetches[2].endsWith('/events?after=1'), `poll fetched ${fetches[2]}`);
  assert.equal(container.innerHTML, fullRenderHtml([E1]), 'the tail replaces the placeholder');
  const settled = container.innerHTML;
  await ctx.fetchAndRenderEvents('t1', 'sess-9');
  assert.equal(container.innerHTML, settled, 'an empty tail leaves the painted list untouched');
});
