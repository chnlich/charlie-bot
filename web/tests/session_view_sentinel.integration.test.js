const assert = require('node:assert/strict');
const test = require('node:test');

const {baseSessionContext, buildUsageElements, createChatSidebarContext, installSessionDocumentLookups,
  stubPageTimers} = require('./session_context_stub');
const {createElement} = require('./dom_element_stub');

function buildContext(overrides = {}) {
  const fetchCalls = [];
  const {context, elements} = baseSessionContext(overrides);

  context.fetch = overrides.fetch || (async (url) => {
    fetchCalls.push(url);
    return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
  });
  stubPageTimers(context);
  context.setTimeout = (fn) => { if (overrides.autoTimeout) fn(); return 1; };
  installSessionDocumentLookups(context, elements, elements.get('messages'), []);
  context.renderUserMessageBubble = (content) => `<div>${content || ''}</div>`;
  context.alert = () => {};
  context.confirm = () => true;

  createChatSidebarContext(context);
  return {context, fetchCalls, elements};
}

function makeMessages(overrides = {}) {
  const messages = createElement({id: 'messages'});
  messages.scrollTop = overrides.scrollTop ?? 0;
  messages.clientHeight = overrides.clientHeight ?? 500;
  messages.scrollHeight = overrides.scrollHeight ?? 1000;
  return messages;
}

function renderSessionViewWith(context, elements, hasMore, opts = {}) {
  const messages = elements.get('messages');
  context.renderSessionView({
    session: {id: 'session-a', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: opts.existingMessages || [{role: 'assistant', content: 'hello', event_index: 5}],
    pending_draft: null,
    event_count: opts.eventCount || 6,
    oldest_message_ordinal: opts.oldestOrdinal ?? 4,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: hasMore,
  });
  // renderSessionView scrolls to bottom; reset for pagination tests.
  messages.scrollTop = 0;
}

function findSentinel(elements) {
  return elements.get('messages').children.find(c => c.id === 'load-more-sentinel');
}

// ---------------------------------------------------------------------------
// Acceptance test 6: Sentinel states, failure recovery, scroll suppress,
//                     bounded viewport fill
// ---------------------------------------------------------------------------

test('sentinel renders idle text on first paint when has_more is true', () => {
  const messages = makeMessages();
  const {context, elements} = buildContext({
    elements: new Map([
      ['messages', messages],
      ['header-session-name', createElement()],
      ['backend-badge', createElement()],
    ]),
  });

  renderSessionViewWith(context, elements, true);

  const sentinel = findSentinel(elements);
  assert.ok(sentinel, 'sentinel must be prepended when has_more is true');
  assert.equal(sentinel.getAttribute('data-state'), 'idle');
  assert.equal(sentinel.innerHTML, 'Scroll up for older messages');
});

test('sentinel shows loading text while a request is in flight', async () => {
  const messages = makeMessages({scrollTop: 0});
  let resolveFetch;
  const fetchPromise = new Promise((resolve) => { resolveFetch = resolve; });
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => fetchPromise.then(() => ({
      ok: true,
      async json() { return {has_more: false, next_before: 0, messages: []}; },
    })),
  });

  renderSessionViewWith(context, elements, true);

  const loadPromise = context.Sidebar.loadOlderIfNeeded(messages);
  const sentinel = findSentinel(elements);
  assert.equal(sentinel.getAttribute('data-state'), 'loading');
  assert.match(sentinel.innerHTML, /Loading older messages/);

  resolveFetch();
  await loadPromise;
});

test('failed fetch keeps has_more and leaves a clickable failed sentinel', async () => {
  const messages = makeMessages({scrollTop: 0});
  let fetchCount = 0;
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => { fetchCount++; return {ok: false, status: 500}; },
  });

  renderSessionViewWith(context, elements, true);

  await context.Sidebar.loadOlderIfNeeded(messages);

  // Failure must NOT clear has_more — verify by checking that a second call
  // (or clicking the failed sentinel) triggers another fetch.
  assert.equal(fetchCount, 1, 'first fetch should have been attempted');

  const sentinel = findSentinel(elements);
  assert.ok(sentinel, 'sentinel must remain after failure');
  assert.equal(sentinel.getAttribute('data-state'), 'failed');
  assert.match(sentinel.innerHTML, /<svg/, 'the failed sentinel carries the retry icon');
  assert.ok(sentinel.getAttribute('aria-label'), 'the icon has an accessible name');
  assert.equal(typeof sentinel.onclick, 'function', 'failed sentinel must be clickable');

  // Clicking the failed sentinel retries — proving has_more was NOT cleared.
  messages.scrollTop = 0;
  sentinel.onclick();
  // The onclick call is async; wait a tick for the fetch to register.
  await new Promise((r) => setTimeout(r, 10));
  assert.equal(fetchCount, 2, 'clicking failed sentinel must retry (has_more was not cleared)');
});

test('programmatic scroll restore does not trigger another fetch', async () => {
  const messages = makeMessages({scrollTop: 0});
  let fetchCount = 0;
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => {
      fetchCount++;
      return {
        ok: true,
        async json() {
          return {
            has_more: false,
            next_before: 0,
            messages: [{id: 'msg-old', role: 'user', content: 'old', event_index: 1}],
          };
        },
      };
    },
  });

  renderSessionViewWith(context, elements, true);

  // Register the scroll handler (initScrollPagination) so we can test the
  // suppress flag.
  context.initScrollPagination();

  await context.Sidebar.loadOlderIfNeeded(messages);
  assert.equal(fetchCount, 1, 'exactly one fetch for the page');

  // Simulate the scroll event dispatched by the programmatic scrollTop
  // assignment inside loadOlderIfNeeded. The suppressScrollLoad flag should
  // absorb it — no additional fetch.
  const scrollHandlers = messages._listeners?.scroll || [];
  assert.ok(scrollHandlers.length > 0, 'scroll handler must be registered');
  for (const handler of scrollHandlers) {
    handler();
  }

  assert.equal(fetchCount, 1, 'programmatic scroll must not trigger another fetch');
});

test('viewport fill keeps loading while the list is short, until the history ends', async () => {
  const messages = makeMessages({scrollTop: 0, scrollHeight: 100, clientHeight: 100});
  let fetchCount = 0;
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async (url) => {
      fetchCount++;
      const match = url.match(/before=(\d+)/);
      const before = parseInt(match[1], 10);
      const nextBefore = Math.max(0, before - 1);
      return {
        ok: true,
        async json() {
          return {
            has_more: nextBefore > 0,
            next_before: nextBefore,
            messages: [{id: `msg-${before}`, role: 'user', content: `m${before}`, event_index: before}],
          };
        },
      };
    },
    autoTimeout: true,
  });

  renderSessionViewWith(context, new Map([['messages', messages]]), true, {
    oldestOrdinal: 100, eventCount: 101,
  });
  // re-grab elements from context is not needed; messages is the same object.
  // renderSessionViewWith resets scrollTop to 0.

  await context.Sidebar.loadOlderIfNeeded(messages);
  // The container stays shorter than its viewport, so no count bounds the chain:
  // one fetch per cursor step from 100 to the history's first event.
  assert.equal(fetchCount, 100, `expected a fetch for each of the 100 cursor steps, got ${fetchCount}`);
  assert.ok(!findSentinel(elements), 'the last page ended the history and removed the sentinel');
});

test('first paint loads nothing into a hidden list', async () => {
  // A hidden (display: none) container reports no height at all, so it also reads as shorter than its viewport.
  const messages = makeMessages({scrollTop: 0, scrollHeight: 0, clientHeight: 0});
  const fetched = [];
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async (url) => {
      fetched.push(url);
      return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
    },
  });

  renderStaleBootstrap(context, messages, {oldest_message_ordinal: 4});

  await new Promise((r) => setTimeout(r, 10));

  assert.equal(fetched.length, 0, 'a hidden list has no viewport to fill');
  assert.equal(findSentinel(elements).getAttribute('data-state'), 'idle');
});

test('viewport fill stops when the list is hidden in the middle of the chain', async () => {
  const messages = makeMessages({scrollTop: 0, scrollHeight: 100, clientHeight: 100});
  const fetched = [];
  const {context} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async (url) => {
      fetched.push(url);
      const before = parseInt(url.match(/before=(\d+)/)[1], 10);
      // The third page's answer finds the list hidden: display: none reports no height.
      if (fetched.length === 3) {
        messages.clientHeight = 0;
        messages.scrollHeight = 0;
      }
      return {
        ok: true,
        async json() {
          return {
            has_more: before - 1 > 0,
            next_before: before - 1,
            messages: [{id: `msg-${before}`, role: 'user', content: `m${before}`, event_index: before}],
          };
        },
      };
    },
    autoTimeout: true,
  });

  renderSessionViewWith(context, new Map([['messages', messages]]), true, {
    oldestOrdinal: 100, eventCount: 101,
  });

  await context.Sidebar.loadOlderIfNeeded(messages);

  assert.equal(fetched.length, 3, 'the chain asked for no page once the list was hidden');
});

test('successful page with no more messages removes the sentinel', async () => {
  const messages = makeMessages({scrollTop: 0});
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => ({
      ok: true,
      async json() { return {has_more: false, next_before: 0, messages: []}; },
    }),
  });

  renderSessionViewWith(context, elements, true);

  await context.Sidebar.loadOlderIfNeeded(messages);

  const sentinel = findSentinel(elements);
  assert.ok(!sentinel || sentinel.removed, 'sentinel must be removed when has_more becomes false');
});

function renderStaleBootstrap(context, messages, extra = {}) {
  context.renderSessionView({
    session: {id: 'session-a', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [{role: 'assistant', content: 'hello', event_index: 5}],
    pending_draft: null,
    event_count: 6,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: true,
    ...extra,
  });
}

test('missing pagination cursor fails loudly instead of silently doing nothing', async () => {
  const messages = makeMessages({scrollTop: 0});
  let fetchCount = 0;
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => {
      fetchCount++;
      return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
    },
  });
  const errors = [];
  context.console.error = (...args) => { errors.push(args); };

  // A backend older than the cursor rename answers with oldest_event_index, so
  // oldest_message_ordinal is absent and the cursor is unusable.
  renderStaleBootstrap(context, messages, {oldest_event_index: 4});
  messages.scrollTop = 0;

  await context.Sidebar.loadOlderIfNeeded(messages);

  assert.equal(fetchCount, 0, 'an unusable cursor must not reach the network');
  const sentinel = findSentinel(elements);
  assert.ok(sentinel, 'sentinel must remain');
  assert.equal(sentinel.getAttribute('data-state'), 'failed');
  assert.match(sentinel.innerHTML, /<svg/, 'the failed sentinel carries the retry icon');
  assert.ok(errors.length >= 1, 'the unusable cursor must be logged');
});

test('first paint fills the viewport when the tail page is shorter than the container', async () => {
  // scrollHeight <= clientHeight: the container cannot scroll, so no scroll event
  // will ever fire and an idle sentinel would wait forever.
  const messages = makeMessages({scrollTop: 0, scrollHeight: 40, clientHeight: 500});
  const fetched = [];
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async (url) => {
      fetched.push(url);
      return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
    },
  });

  renderStaleBootstrap(context, messages, {oldest_message_ordinal: 4});

  await new Promise((r) => setTimeout(r, 10));

  assert.equal(fetched.length, 1, 'first paint must fetch one older page');
  assert.match(fetched[0], /before=4/);
  const sentinel = findSentinel(elements);
  assert.ok(!sentinel || sentinel.removed, 'sentinel goes away once has_more is false');
});

test('first paint does not fetch when the tail page already fills the container', async () => {
  const messages = makeMessages({scrollTop: 0, scrollHeight: 1000, clientHeight: 500});
  const fetched = [];
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async (url) => {
      fetched.push(url);
      return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
    },
  });

  renderStaleBootstrap(context, messages, {oldest_message_ordinal: 4});

  await new Promise((r) => setTimeout(r, 10));

  assert.equal(fetched.length, 0, 'a scrollable container must not auto-fetch on first paint');
  const sentinel = findSentinel(elements);
  assert.equal(sentinel.getAttribute('data-state'), 'idle');
});

// ---------------------------------------------------------------------------
// renderUsageFromData: context and cost cell rendering
// ---------------------------------------------------------------------------

test('renderUsageFromData shows the cost alone when the context reading is missing', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: null,
    context_full: null,
    context_compact_at: null,
    total_cost_usd: 0.5,
    model: '',
  });

  assert.equal(elements.get('usage-text').textContent, 'unknown');
  assert.match(elements.get('usage-bar').className, /hidden/);
  assert.equal(elements.get('usage-bar').style.width, '0%');
  // The cost cell still renders, and the empty meter and the unknown reading stay hidden beside it.
  assert.equal(elements.get('usage-cost').textContent, '$0.50');
  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), false);
  assert.equal(elements.get('usage-meter').classList.contains('hidden'), true);
  assert.equal(elements.get('usage-text').classList.contains('hidden'), true);
  assert.equal(elements.get('usage-cost').classList.contains('hidden'), false);
});

test('renderUsageFromData keeps the indicator hidden when the record has no readings', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: null,
    context_full: null,
    context_compact_at: null,
    total_cost_usd: null,
    model: '',
  });

  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), true);
  assert.equal(elements.get('usage-meter').classList.contains('hidden'), true);
  assert.equal(elements.get('usage-text').classList.contains('hidden'), true);
  assert.equal(elements.get('usage-cost').classList.contains('hidden'), true);
});

test('renderUsageFromData hides an incomplete context reading with no cost', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 50000,
    context_full: null,
    context_compact_at: null,
    total_cost_usd: null,
    model: '',
  });

  assert.equal(elements.get('usage-text').textContent, 'unknown');
  assert.match(elements.get('usage-bar').className, /hidden/);
  // An incomplete reading with no cost leaves the header empty.
  assert.equal(elements.get('usage-cost').textContent, 'N/A');
  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), true);
});

test('renderUsageFromData shows a cost error label with its text on hover, and keeps the context reading', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});
  const costError = 'native session native-x total_cost_usd fell from 2 to 1.5; a cumulative reading cannot decrease';

  context.renderUsageFromData({
    context_tokens: 500,
    context_full: 1000,
    context_compact_at: null,
    total_cost_usd: null,
    total_cost_error: costError,
    model: 'claude-test',
  });

  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), false);
  assert.equal(elements.get('usage-text').classList.contains('hidden'), false);
  // The cost cell is shown with a label, not a dollar sum, and its hover title is the error text.
  assert.equal(elements.get('usage-cost').classList.contains('hidden'), false);
  assert.match(elements.get('usage-cost').textContent, /\S/);
  assert.doesNotMatch(elements.get('usage-cost').textContent, /\$/);
  assert.equal(elements.get('usage-cost').title, costError);
});

test('renderUsageFromData draws the bar when both context fields are present', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData({
    context_tokens: 100000,
    context_full: 200000,
    context_compact_at: 167000,
    total_cost_usd: 1.25,
    model: 'claude-opus-4-6',
  });

  assert.equal(elements.get('usage-text').textContent, '100k / 200k');
  assert.doesNotMatch(elements.get('usage-bar').className, /hidden/);
  assert.equal(elements.get('usage-bar').style.width, '50.0%');
  assert.equal(elements.get('usage-cost').textContent, '$1.25');
  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), false);
  assert.equal(elements.get('usage-meter').classList.contains('hidden'), false);
  assert.equal(elements.get('usage-text').classList.contains('hidden'), false);
});

test('renderUsageFromData hides the indicator when usage is null', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData(null);

  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), true);
});

// ---------------------------------------------------------------------------
// The older-page retry: a failed page keeps its cursor and its place, and retries on the page-request backoff. The
// harness's setTimeout is a stub, so these cases hold the timers a failure schedules and fire them by hand. The page's
// own 10-second request timeout is held too, and the cases skip it by its length.
// ---------------------------------------------------------------------------

const PAGE_REQUEST_TIMEOUT_MS = 10000;

function heldTimers(context) {
  const timers = new Map();
  let nextId = 1;
  context.setTimeout = (fn, ms) => {
    const id = nextId++;
    timers.set(id, {fn, ms});
    return id;
  };
  context.clearTimeout = (id) => { timers.delete(id); };
  return timers;
}

function retryTimers(timers) {
  return [...timers.values()].filter((timer) => timer.ms !== PAGE_REQUEST_TIMEOUT_MS);
}

// Fires the held retry timer, which leaves the timer list as a real timer does.
function fireRetry(timers) {
  const [id, timer] = [...timers.entries()].find(([, held]) => held.ms !== PAGE_REQUEST_TIMEOUT_MS);
  timers.delete(id);
  timer.fn();
}

function flushed() {
  return new Promise((resolve) => setImmediate(resolve));
}

// A fetch that answers `script` in order: a number is a failure status, an object is an empty older page.
function scriptedPages(script) {
  const urls = [];
  let call = 0;
  const fetch = async (url) => {
    urls.push(url);
    const step = script[Math.min(call++, script.length - 1)];
    if (typeof step === 'number') return {ok: false, status: step};
    return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
  };
  return {fetch, urls};
}

function olderPageRig(script) {
  const messages = makeMessages({scrollTop: 0});
  const pages = scriptedPages(script);
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: pages.fetch,
  });
  renderSessionViewWith(context, elements, true);
  const timers = heldTimers(context);
  return {context, elements, messages, timers, urls: pages.urls};
}

test('a failed older page retries its cursor on the page-request backoff and resets after a success', async () => {
  const {context, messages, timers, urls} = olderPageRig([500, 502, 200]);

  await context.Sidebar.loadOlderIfNeeded(messages);
  assert.deepEqual(retryTimers(timers).map((timer) => timer.ms), [1000]);

  fireRetry(timers);
  await flushed();
  assert.deepEqual(retryTimers(timers).map((timer) => timer.ms), [2000], 'the second failure doubles the delay');

  fireRetry(timers);
  await flushed();
  assert.equal(retryTimers(timers).length, 0, 'the success schedules no retry');
  assert.equal(new Set(urls).size, 1, 'every retry asks for the same cursor');
  assert.equal(urls.length, 3);
});

test('a failed older page waits for the browser to come back online before it retries', async () => {
  const {context, elements, messages, timers, urls} = olderPageRig([500, 200]);
  const listeners = new Map();
  context.navigator = {onLine: false};
  context.window = {
    addEventListener: (type, fn) => listeners.set(type, fn),
    removeEventListener: (type) => listeners.delete(type),
  };

  await context.Sidebar.loadOlderIfNeeded(messages);
  assert.equal(retryTimers(timers).length, 0, 'offline, no timer runs');
  assert.ok(listeners.has('online'), 'the failure waits on the online event');
  assert.equal(findSentinel(elements).getAttribute('data-state'), 'failed');

  listeners.get('online')();
  await flushed();
  assert.equal(urls.length, 2, 'coming online retries at once');
  assert.equal(listeners.has('online'), false, 'the listener leaves with its retry');
});

test('a 4xx older page is the server refusing, so only the reader retries it', async () => {
  const {context, elements, messages, timers, urls} = olderPageRig([404]);

  await context.Sidebar.loadOlderIfNeeded(messages);
  assert.equal(retryTimers(timers).length, 0, 'a refusal schedules no retry');
  assert.equal(findSentinel(elements).getAttribute('data-state'), 'failed');

  findSentinel(elements).onclick();
  await flushed();
  assert.equal(urls.length, 2, 'the click retries');
});

test('a click on the failed sentinel retries at once and clears the pending timer', async () => {
  const {context, elements, messages, timers, urls} = olderPageRig([500, 200]);

  await context.Sidebar.loadOlderIfNeeded(messages);
  assert.equal(retryTimers(timers).length, 1);

  findSentinel(elements).onclick();
  assert.equal(retryTimers(timers).length, 0, 'the click takes over the pending retry');
  // A second request while the click's flight is open starts no duplicate flight.
  context.Sidebar.loadOlderIfNeeded(messages);
  await flushed();
  assert.equal(urls.length, 2, 'one request in flight at a time');
});

test('a switch cancels a pending older-page retry and starts the backoff over', async () => {
  const {context, messages, timers} = olderPageRig([500]);

  await context.Sidebar.loadOlderIfNeeded(messages);
  fireRetry(timers);
  await flushed();
  assert.deepEqual(retryTimers(timers).map((timer) => timer.ms), [2000]);

  context.Sidebar.cancelOlderPageRetry();
  assert.equal(retryTimers(timers).length, 0, 'the switch clears the timer');
  // The next failure after a switch starts from the base delay again.
  await context.Sidebar.loadOlderIfNeeded(messages);
  assert.deepEqual(retryTimers(timers).map((timer) => timer.ms), [1000]);
});

test('a second older-page load while one is in flight starts no second request', async () => {
  const messages = makeMessages({scrollTop: 0});
  const urls = [];
  let settle = null;
  const {context} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async (url) => {
      urls.push(url);
      await new Promise((resolve) => { settle = resolve; });
      return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
    },
  });
  renderSessionViewWith(context, new Map([['messages', messages]]), true);

  const first = context.Sidebar.loadOlderIfNeeded(messages);
  const second = context.Sidebar.loadOlderIfNeeded(messages);
  assert.equal(urls.length, 1, 'the second load finds the first page in flight and asks for nothing');
  settle();
  await Promise.all([first, second]);
  assert.equal(urls.length, 1, 'the page that landed was the only request');
});

test('a landed page drops the failed sentinel\'s retry name and tooltip', async () => {
  // A hidden list (clientHeight 0) takes no fill check, so the landed page settles the sentinel at idle.
  const messages = makeMessages({scrollTop: 0, clientHeight: 0});
  let calls = 0;
  const {context, elements} = buildContext({
    elements: new Map([['messages', messages]]),
    fetch: async () => {
      calls++;
      if (calls === 1) return {ok: false, status: 500};
      return {ok: true, async json() { return {has_more: true, next_before: 1, messages: []}; }};
    },
  });
  renderSessionViewWith(context, elements, true);
  const timers = heldTimers(context);

  await context.Sidebar.loadOlderIfNeeded(messages);
  assert.equal(findSentinel(elements).getAttribute('aria-label'), 'Retry loading older messages');

  fireRetry(timers);
  await flushed();

  const idle = findSentinel(elements);
  assert.equal(idle.getAttribute('data-state'), 'idle');
  assert.ok(!idle.getAttribute('aria-label'), 'the text state carries no retry name');
  assert.ok(!idle.getAttribute('title'), 'the text state carries no retry tooltip');
});
