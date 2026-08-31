const assert = require('node:assert/strict');
const test = require('node:test');

const { baseSessionContext, buildUsageElements, createChatSidebarContext } = require('./session_context_stub');
const { createElement } = require('./dom_element_stub');

function buildContext(overrides = {}) {
  const fetchCalls = [];
  const {context, elements} = baseSessionContext(overrides);

  context.fetch = overrides.fetch || (async (url) => {
    fetchCalls.push(url);
    return {ok: true, async json() { return {has_more: false, next_before: 0, messages: []}; }};
  });
  context.setInterval = () => 1;
  context.setTimeout = (fn) => { if (overrides.autoTimeout) fn(); return 1; };
  context.clearInterval = () => {};
  context.clearTimeout = () => {};
  context.document.getElementById = (id) => {
    const fromMap = elements.get(id);
    if (fromMap) return fromMap;
    const container = elements.get('messages');
    if (container) {
      for (const child of container.children) {
        if (child.id === id) return child;
      }
    }
    return null;
  };
  context.document.querySelectorAll = () => [];
  context.document.querySelector = () => null;
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
      ['input-model-badge', createElement()],
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

  const loadPromise = context.loadOlderIfNeeded(messages);
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

  await context.loadOlderIfNeeded(messages);

  // Failure must NOT clear has_more — verify by checking that a second call
  // (or clicking the failed sentinel) triggers another fetch.
  assert.equal(fetchCount, 1, 'first fetch should have been attempted');

  const sentinel = findSentinel(elements);
  assert.ok(sentinel, 'sentinel must remain after failure');
  assert.equal(sentinel.getAttribute('data-state'), 'failed');
  assert.match(sentinel.innerHTML, /Failed to load older messages/);
  assert.match(sentinel.innerHTML, /click to retry/);
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

  await context.loadOlderIfNeeded(messages);
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

test('viewport fill stops at 5 consecutive auto-loads', async () => {
  const messages = makeMessages({scrollTop: 0, scrollHeight: 100, clientHeight: 100});
  let fetchCount = 0;
  const {context} = buildContext({
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

  await context.loadOlderIfNeeded(messages);
  // 1 initial fetch + 5 viewport fills = 6 total.
  assert.equal(fetchCount, 6, `expected 6 fetches (1 initial + 5 fills), got ${fetchCount}`);
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

  await context.loadOlderIfNeeded(messages);

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

  await context.loadOlderIfNeeded(messages);

  assert.equal(fetchCount, 0, 'an unusable cursor must not reach the network');
  const sentinel = findSentinel(elements);
  assert.ok(sentinel, 'sentinel must remain');
  assert.equal(sentinel.getAttribute('data-state'), 'failed');
  assert.match(sentinel.innerHTML, /Failed to load older messages/);
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
// renderUsageFromData: unknown context + cost cell rendering
// ---------------------------------------------------------------------------

test('renderUsageFromData shows unknown and hides the bar when context_tokens is null', () => {
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
  // The cost cell still renders.
  assert.equal(elements.get('usage-cost').textContent, '$0.50');
  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), false);
});

test('renderUsageFromData shows unknown when only context_full is null', () => {
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
  // N/A cost still renders in the cell.
  assert.equal(elements.get('usage-cost').textContent, 'N/A');
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
});

test('renderUsageFromData hides the indicator when usage is null', () => {
  const elements = buildUsageElements();
  const {context} = buildContext({elements});

  context.renderUsageFromData(null);

  assert.equal(elements.get('usage-indicator').classList.contains('hidden'), true);
});
