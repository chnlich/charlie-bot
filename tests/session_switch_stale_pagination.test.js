const assert = require('node:assert/strict');
const test = require('node:test');

const { baseSessionContext, createChatSidebarContext } = require('./session_context_stub');
const { createElement } = require('./dom_element_stub');

const TELEMETRY_URL = '/api/diag/switch-events';
const PENDING = Symbol('pending');

const A_PAGE_900 = '/api/sessions/session-a/events?before=900&limit=40';
const B_PAGE_500 = '/api/sessions/session-b/events?before=500&limit=40';

function makeSidebarRow(sessionId, name) {
  const nameEl = createElement({textContent: name});
  return createElement({
    id: 'session-' + sessionId,
    querySelector: (sel) => (sel === '.session-name' ? nameEl : null),
  });
}

function bootstrapPayload(sessionId, oldestOrdinal) {
  return {
    session: {id: sessionId, name: 'Session ' + sessionId, backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [{role: 'assistant', content: 'hello from ' + sessionId, event_index: 5}],
    pending_draft: null,
    event_count: 6,
    oldest_message_ordinal: oldestOrdinal,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    switchable_backends: [],
    has_more: true,
    threads: [],
    triggers: [],
  };
}

const BOOTSTRAP = {
  'session-a': bootstrapPayload('session-a', 900),
  'session-b': bootstrapPayload('session-b', 500),
};

// The stale flight's payload: a successful page carrying A's own next cursor
// (800) and content marker (A-OLDER-PAGE) that must never reach B's view.
function aPage(hasMore) {
  return {
    has_more: hasMore,
    next_before: 800,
    messages: [{id: 'a-old-1', role: 'assistant', content: 'A-OLDER-PAGE', event_index: 899}],
  };
}

// B's own page: strictly decreasing cursor so the no-progress guard stays
// quiet; has_more bottoms out below 100 so a never-awaited re-entry cannot
// flood the harness with chained auto-fills.
function bPage(before) {
  return {
    has_more: before - 50 > 100,
    next_before: before - 50,
    messages: [{id: 'b-old-' + before, role: 'assistant', content: 'B-OLDER-PAGE', event_index: before - 1}],
  };
}

// eventsHandler(sid, before) answers a pagination fetch: PENDING parks the
// request with its resolve/reject handles recorded in pendingEvents; any
// other value is answered immediately as the page payload.
function buildHarness(eventsHandler) {
  const messages = createElement({id: 'messages'});
  messages.clientHeight = 500;
  messages.scrollHeight = 100;
  messages.scrollTop = 0;
  const rows = [
    makeSidebarRow('session-a', 'Alpha'),
    makeSidebarRow('session-b', 'Beta'),
  ];
  const elements = new Map([
    ['messages', messages],
    ['header-session-name', createElement({id: 'header-session-name'})],
    ['backend-badge', createElement()],
    ['input-model-badge', createElement()],
    ['msg-input', createElement()],
    ...rows.map((row) => [row.id, row]),
  ]);
  const h = {
    messages,
    elements,
    fetchCalls: [],
    pendingEvents: [],
    errors: [],
    preBootstrap: null,
  };

  const {context} = baseSessionContext({elements});
  context.eventCursor = 0;
  context.console.error = (...args) => h.errors.push(args);
  context.document.getElementById = (id) => {
    const fromMap = elements.get(id);
    if (fromMap) return fromMap;
    for (const child of messages.children) {
      if (child.id === id) return child;
    }
    return null;
  };
  context.document.querySelectorAll = (sel) => (sel === '[id^="session-"]' ? rows : []);
  context.document.querySelector = () => null;
  context.fetch = async (url, opts = {}) => {
    if (url === TELEMETRY_URL) {
      return {ok: true, status: 200, json: async () => ({ok: true})};
    }
    const boot = url.match(/\/api\/sessions\/([^/]+)\/bootstrap/);
    if (boot) {
      // Interleaving (a) hook: the test may land the stale flight inside the
      // bootstrap branch, before the response feeds the render microtask.
      if (h.preBootstrap) {
        const pre = h.preBootstrap;
        h.preBootstrap = null;
        pre();
      }
      return {ok: true, status: 200, json: async () => BOOTSTRAP[boot[1]]};
    }
    const page = url.match(/\/api\/sessions\/([^/]+)\/events\?before=(\d+)/);
    if (page) {
      h.fetchCalls.push(url);
      const out = eventsHandler(page[1], Number(page[2]));
      if (out === PENDING) {
        const entry = {sessionId: page[1], before: Number(page[2])};
        entry.done = new Promise((resolve, reject) => {
          entry.resolve = resolve;
          entry.reject = reject;
        });
        h.pendingEvents.push(entry);
        return entry.done.then((payload) => ({ok: true, status: 200, json: async () => payload}));
      }
      return {ok: true, status: 200, json: async () => out};
    }
    return {ok: true, status: 200, json: async () => ({})};
  };
  context.setInterval = () => 1;
  context.setTimeout = () => 1;
  context.clearInterval = () => {};
  context.clearTimeout = () => {};

  createChatSidebarContext(context);
  h.context = context;
  return h;
}

const alwaysPending = () => PENDING;

// B's first pagination request parks (the test controls when it lands); every
// later B request answers immediately so bounded auto-continue chains settle.
function bFirstPendingThenChain() {
  let bCalls = 0;
  return (sid, before) => {
    if (sid === 'session-a') return PENDING;
    bCalls += 1;
    if (bCalls === 1) return PENDING;
    return bPage(before);
  };
}

// Unswitched normal path: one page with has_more, then the final page.
function aImmediateChain() {
  let calls = 0;
  return (sid, before) => {
    calls += 1;
    return {
      has_more: calls === 1,
      next_before: before - 100,
      messages: [{id: 'a-old-' + calls, role: 'assistant', content: 'A-OLDER-PAGE-' + calls, event_index: before - 1}],
    };
  };
}

function eventsUrls(h, sessionId) {
  return h.fetchCalls.filter((u) => u.includes('/api/sessions/' + sessionId + '/events'));
}

function beforeValues(urls) {
  return urls.map((u) => Number(u.match(/before=(\d+)/)[1]));
}

function findSentinel(h) {
  return h.messages.children.find((c) => c.id === 'load-more-sentinel');
}

function collectText(el) {
  let text = String(el.innerHTML || '') + ' ' + String(el.textContent || '');
  for (const child of el.children) text += ' ' + collectText(child);
  return text;
}

function startHangingAFlight(h) {
  h.context.renderSessionView(BOOTSTRAP['session-a']);
  h.messages.scrollTop = 0;
  h.context.loadOlderIfNeeded(h.messages);
  assert.deepEqual(h.fetchCalls, [A_PAGE_900], 'A pagination must be in flight before the switch');
}

async function completeSwitch(h, sessionId) {
  await h.context.switchSession(sessionId);
}

// Two macrotask turns flush every microtask hop of a landing (fetch await,
// json await, finally). Never `await` a stale flight itself: legacy code
// would hold it open with an auto-continue request the harness parks forever.
async function flush() {
  await new Promise((r) => setImmediate(r));
  await new Promise((r) => setImmediate(r));
}

// ---------------------------------------------------------------------------
// Assertion 1: the interleaving invariant — at every release point, the stale
// landing must leave B exactly as if the flight never existed.
// ---------------------------------------------------------------------------

test('interleaving (a): stale page landing between placeholder and render is dropped wholesale', async () => {
  const h = buildHarness(alwaysPending);
  startHangingAFlight(h);
  // Resolve A inside the bootstrap branch: A's landing microtask lands after
  // the placeholder paint and before B's render microtask.
  h.preBootstrap = () => h.pendingEvents[0].resolve(aPage(true));

  await completeSwitch(h, 'session-b');
  await flush();

  assert.deepEqual(h.fetchCalls, [A_PAGE_900], 'no auto-continue chain and no fresh A request may leak');
  const sentinel = findSentinel(h);
  assert.ok(sentinel, "B's sentinel must survive the stale landing");
  assert.equal(sentinel.getAttribute('data-state'), 'idle', "B's has_more must not be overwritten");
  assert.doesNotMatch(collectText(h.messages), /A-OLDER-PAGE/, 'no A message node in the container');

  h.messages.scrollTop = 0;
  h.context.loadOlderIfNeeded(h.messages);
  assert.deepEqual(eventsUrls(h, 'session-b'), [B_PAGE_500], 'the next request must carry B own cursor');
  assert.equal(eventsUrls(h, 'session-a').length, 1, 'still no fresh A pagination URL');
});

test('interleaving (b): stale page landing after B rendered, no B flight, is dropped wholesale', async () => {
  const h = buildHarness(alwaysPending);
  startHangingAFlight(h);
  await completeSwitch(h, 'session-b');
  // Render pinned scrollTop at scrollHeight(100) > 80, so the switch tail's auto-fetch
  // exited early: B is rendered with no flight of its own.

  h.pendingEvents[0].resolve(aPage(false));  // has_more:false poisons B if applied
  await flush();

  assert.deepEqual(h.fetchCalls, [A_PAGE_900], 'the dropped page must not spawn any request');
  const sentinel = findSentinel(h);
  assert.ok(sentinel, 'a stale has_more:false page must not strip B of its sentinel');
  assert.equal(sentinel.getAttribute('data-state'), 'idle', "B's has_more must not be overwritten");
  assert.doesNotMatch(collectText(h.messages), /A-OLDER-PAGE/, 'no A message node in the container');

  h.messages.scrollTop = 0;
  h.context.loadOlderIfNeeded(h.messages);
  assert.deepEqual(eventsUrls(h, 'session-b'), [B_PAGE_500], 'the next request must carry B own cursor');
  assert.equal(eventsUrls(h, 'session-a').length, 1, 'still no fresh A pagination URL');
});

test('interleaving (c): stale page landing while B own pagination is in flight is dropped', async () => {
  const h = buildHarness(bFirstPendingThenChain());
  startHangingAFlight(h);
  await completeSwitch(h, 'session-b');

  h.messages.scrollTop = 0;
  const bFlight = h.context.loadOlderIfNeeded(h.messages);
  assert.deepEqual(eventsUrls(h, 'session-b'), [B_PAGE_500], 'B own flight is in the air with cursor 500');

  h.pendingEvents[0].resolve(aPage(true));
  await flush();

  h.context.loadOlderIfNeeded(h.messages);
  assert.deepEqual(eventsUrls(h, 'session-b'), [B_PAGE_500], 'the drop must not release B gate ownership');
  assert.equal(eventsUrls(h, 'session-a').length, 1, 'no fresh A pagination URL');

  h.pendingEvents[1].resolve(bPage(500));
  await bFlight;

  assert.deepEqual(beforeValues(eventsUrls(h, 'session-b')), [500, 450, 400, 350, 300, 250],
      'the whole fill chain derives from B own cursor; A next cursor (800) never enters it');
  assert.doesNotMatch(collectText(h.messages), /A-OLDER-PAGE/, 'no A message node in the container');
  const sentinel = findSentinel(h);
  assert.ok(sentinel);
  assert.equal(sentinel.getAttribute('data-state'), 'idle', "B's has_more stays authoritative");
});

// ---------------------------------------------------------------------------
// Assertion 2: a rejected stale flight never paints the failure sentinel.
// ---------------------------------------------------------------------------

test('rejected stale flight never paints a failed sentinel onto the new session', async () => {
  const h = buildHarness(alwaysPending);
  startHangingAFlight(h);
  await completeSwitch(h, 'session-b');

  h.pendingEvents[0].reject(new Error('500'));
  await flush();

  const failedNodes = h.messages.children.filter((c) => c.getAttribute('data-state') === 'failed');
  assert.equal(failedNodes.length, 0, 'no data-state=failed sentinel node may land in B view');
  const sentinel = findSentinel(h);
  assert.ok(sentinel);
  assert.equal(sentinel.getAttribute('data-state'), 'idle');
  assert.equal(h.errors.length, 0, 'the stale failure branch (console.error included) is skipped wholesale');
  assert.deepEqual(h.fetchCalls, [A_PAGE_900]);
});

// ---------------------------------------------------------------------------
// Assertion 3: ownership mechanics.
// ---------------------------------------------------------------------------

test('ownership release: after the drop, B paginates again with its own cursor', async () => {
  const h = buildHarness(alwaysPending);
  startHangingAFlight(h);
  await completeSwitch(h, 'session-b');

  h.pendingEvents[0].resolve(aPage(true));
  await flush();
  assert.deepEqual(eventsUrls(h, 'session-b'), [], 'no B request yet');

  h.messages.scrollTop = 0;
  h.context.loadOlderIfNeeded(h.messages);  // direct re-entry via the exported API
  assert.deepEqual(eventsUrls(h, 'session-b'), [B_PAGE_500],
      'the gate is free for B and the request carries B own cursor');
});

test('ownership probe: gate holds while B is in flight, releases on B own completion', async () => {
  const h = buildHarness(bFirstPendingThenChain());
  startHangingAFlight(h);
  await completeSwitch(h, 'session-b');

  h.messages.scrollTop = 0;
  const bFlight = h.context.loadOlderIfNeeded(h.messages);
  assert.deepEqual(eventsUrls(h, 'session-b'), [B_PAGE_500]);

  h.pendingEvents[0].resolve(aPage(true));
  await flush();

  // Probe 1: an injected re-entry while B is in flight must stay gated — a
  // finally that unconditionally cleared the hold would let this fetch out.
  h.context.loadOlderIfNeeded(h.messages);
  assert.deepEqual(eventsUrls(h, 'session-b'), [B_PAGE_500], 're-entry stays gated during B flight');

  // B's own completion releases the hold (credential-equal finally).
  h.pendingEvents[1].resolve(bPage(500));
  await bFlight;
  assert.equal(eventsUrls(h, 'session-b').length, 6, 'the bounded viewport-fill chain settled');

  // Probe 2: a re-entry now issues a fresh request continuing B's cursor.
  h.messages.scrollTop = 0;
  h.context.loadOlderIfNeeded(h.messages);
  const urls = eventsUrls(h, 'session-b');
  assert.equal(urls.length, 7, 'the post-release re-entry issues a fresh request');
  assert.deepEqual(beforeValues(urls), [500, 450, 400, 350, 300, 250, 200],
      'the fresh request continues from B own cursor chain');
});

// ---------------------------------------------------------------------------
// Assertion 4: A→B→A — the session leg matches, the generation leg drops.
// ---------------------------------------------------------------------------

test('A-B-A switch-back: the first A flight is dropped on the generation leg alone', async () => {
  const h = buildHarness(alwaysPending);
  startHangingAFlight(h);
  await completeSwitch(h, 'session-b');  // generation 1
  await completeSwitch(h, 'session-a');  // generation 2: session id matches, generation does not

  h.pendingEvents[0].resolve(aPage(true));
  await flush();

  assert.deepEqual(eventsUrls(h, 'session-a'), [A_PAGE_900], 'no auto-continue chain from the stale landing');
  assert.doesNotMatch(collectText(h.messages), /A-OLDER-PAGE/, 'no stale-page message node in the container');
  assert.match(h.messages.innerHTML, /hello from session-a/, 'the re-rendered A view is intact');
  const sentinel = findSentinel(h);
  assert.ok(sentinel);
  assert.equal(sentinel.getAttribute('data-state'), 'idle');

  h.messages.scrollTop = 0;
  h.context.loadOlderIfNeeded(h.messages);
  assert.deepEqual(eventsUrls(h, 'session-a'), [A_PAGE_900, A_PAGE_900],
      'fresh pagination restarts from the bootstrap cursor (900), not the stale page cursor (800)');
});

// ---------------------------------------------------------------------------
// Assertion 5: normal path regression — no switch, one page lands and applies.
// ---------------------------------------------------------------------------

test('unswitched pagination still lands and applies its page and cursor', async () => {
  const h = buildHarness(aImmediateChain());
  h.context.renderSessionView(BOOTSTRAP['session-a']);
  h.messages.scrollTop = 0;

  await h.context.loadOlderIfNeeded(h.messages);

  assert.deepEqual(h.fetchCalls, [A_PAGE_900, '/api/sessions/session-a/events?before=800&limit=40'],
      'the first page landed, advanced the cursor, and viewport-fill continued from it');
  assert.ok(!findSentinel(h), 'the final has_more:false page removed the sentinel');

  h.messages.scrollTop = 0;
  h.context.loadOlderIfNeeded(h.messages);
  assert.equal(h.fetchCalls.length, 2, 'has_more:false was applied — no further request');
});
