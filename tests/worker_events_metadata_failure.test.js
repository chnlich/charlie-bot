// Worker-events fetch (workers.js): a failed thread-detail (metadata) fetch must
// not abort the events paint. Both failure forms -- a non-OK response (a 500 with
// a plain-text body) and a non-JSON body behind ok:true -- skip the attach-bar
// update and still render the events; the success form keeps the bar update.
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadToggleHarness } = require('./chat_rendering_context_stub');
const { FakeElement } = require('./fake_dom');

const E1 = {type: 'assistant', content: 'first reply', timestamp: '2026-09-02T10:00:00Z'};

// json() throws on both forms: a 500 ships a plain-text body, and the guard must
// keep non-OK metadata away from .json() entirely.
const METADATA_FAILURES = [
  {
    name: 'non-OK detail response (500 plain-text body)',
    metadata: {ok: false, status: 500, json: async () => { throw new TypeError('Unexpected token I in JSON at position 0'); }},
  },
  {
    name: 'OK detail response with a non-JSON body',
    metadata: {ok: true, json: async () => { throw new TypeError('Unexpected token I in JSON at position 0'); }},
  },
];

function loadContext(metadataResponse, eventsPayload) {
  const ctx = loadToggleHarness('workers.js', {
    fetch: async (url) => (url.includes('/events?after=')
        ? {ok: true, json: async () => eventsPayload}
        : metadataResponse),
  });
  const parent = new FakeElement('div');
  const attachBar = parent.appendChild(new FakeElement('div'));
  const container = parent.appendChild(new FakeElement('div'));
  ctx._elements.set('thread-events-t1', container);
  ctx._elements.set('thread-attach-t1', attachBar);
  attachBar.innerHTML = 'previous attach bar';
  return {ctx, container, attachBar};
}

function fullRenderHtml(events) {
  const {ctx} = loadContext({ok: true, json: async () => ({})}, {events: [], total: 0});
  ctx.renderThreadEvents('t1', events);
  return ctx._elements.get('thread-events-t1').innerHTML;
}

for (const {name, metadata} of METADATA_FAILURES) {
  test(`a failed detail fetch does not abort the events paint: ${name}`, async () => {
    const {ctx, container, attachBar} = loadContext(metadata, {events: [E1], total: 1, reset: false});
    const barBefore = attachBar.innerHTML;

    await ctx.fetchAndRenderEvents('t1', 'sess-9');

    assert.equal(container.innerHTML, fullRenderHtml([E1]));
    assert.equal(attachBar.innerHTML, barBefore, 'the attach bar keeps its previous state');
  });
}

test('a successful detail fetch still updates the attach bar', async () => {
  const {ctx, container, attachBar} = loadContext(
      {ok: true, json: async () => ({attach_command: 'tmux attach -t sess-9'})},
      {events: [E1], total: 1, reset: false});

  await ctx.fetchAndRenderEvents('t1', 'sess-9');

  assert.equal(container.innerHTML, fullRenderHtml([E1]));
  assert.ok(attachBar.innerHTML.includes('tmux attach -t sess-9'), 'the attach bar carries the command');
});
