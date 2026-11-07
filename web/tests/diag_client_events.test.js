const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic, readPackageStatic} = require('./read_static');
const {loadWebsocketContext} = require('./websocket_context_stub');
const {createFakeWebSocketClass} = require('./fake_websocket');

// The wire path every client timing event posts to; an endpoint move on either
// side breaks the pin.
const CLIENT_EVENTS_URL = '/api/diag/client-events';

// The client timing channel: core announces the raw moments as page events and
// the diag package's client-events.js assembles each kind into one POST to
// /api/diag/client-events. These tests pin the assembly — one POST per send
// with both waits, one POST per socket episode with the cause separated, one
// POST per traffic flush with the ids folded out of the paths, and the page
// load's announced cause.

const PAGE_URL = 'http://localhost:8000/?session=sess-1';
const FONTS_URL = 'https://fonts.googleapis.com/css2?family=Fira+Code:wght@400;500&display=swap';
const MARKED_URL = 'https://cdn.jsdelivr.net/npm/marked/marked.min.js';
const TAILWIND_URL = 'http://localhost:8000/static/css/tailwind.css?v=abc';
const BLOCKING_ELEMENTS = [
  {tagName: 'LINK', href: FONTS_URL},
  {tagName: 'LINK', href: TAILWIND_URL},
  {tagName: 'SCRIPT', src: MARKED_URL, async: false, defer: false},
  {tagName: 'SCRIPT', src: 'https://cdn.jsdelivr.net/npm/katex@0.16.21/dist/contrib/auto-render.min.js',
    async: false, defer: true},
];
// The entries the browser reports for the two loaded files: the markup's blocking scripts and stylesheets.
const MARKED_ENTRY = {name: MARKED_URL, startTime: 400.4, responseEnd: 900.6, transferSize: 2500,
  encodedBodySize: 2500, requestStart: 0, responseStart: 0};
const TAILWIND_ENTRY = {name: TAILWIND_URL, startTime: 320, requestStart: 310, responseStart: 320, responseEnd: 330,
  transferSize: 4000, encodedBodySize: 3900};

// A harness whose document keeps one listener list per event type, whose fetch
// records every client-event POST, and whose page-timers registry runs the real
// page-timers.js so the traffic flush is the timer callback the page runs.
function buildClientEventsHarness() {
  const posts = [];            // every client-event POST body
  const postOptions = [];      // every client-event POST's fetch options
  const listeners = new Map();
  const sessionStorageData = new Map();
  const resourceEntries = [];  // entries the traffic observer callback is fed
  const flushFns = [];         // the page-timers intervals, flushTraffic first
  const timers = [];           // the recorded setTimeout callbacks, cutoffs included
  let navigationEntries = [];
  let resourceBufferClears = 0;
  let clock = 1_000_000;       // the harness clock, in ms
  const navigationStart = clock;
  const mutationObservers = [];
  const renderedSelectors = new Set();

  const context = {
    JSON_HEADERS: {'Content-Type': 'application/json'},
    console: {log: () => {}, warn: () => {}, error: () => {}},
    CustomEvent,
    document: {
      hidden: false,
      visibilityState: 'visible',
      addEventListener: (type, listener) => {
        if (!listeners.has(type)) listeners.set(type, []);
        listeners.get(type).push(listener);
      },
      removeEventListener: () => {},
      dispatchEvent: (event) => {
        for (const listener of listeners.get(event.type) || []) listener(event);
        return true;
      },
      querySelector: (selector) => (renderedSelectors.has(selector) ? {} : null),
      querySelectorAll: () => BLOCKING_ELEMENTS,
    },
    MutationObserver: class {
      constructor(callback) {
        this.callback = callback;
        this.connected = true;
        mutationObservers.push(this);
      }
      observe() {}
      disconnect() { this.connected = false; }
    },
    sessionStorage: {
      getItem: (key) => sessionStorageData.has(key) ? sessionStorageData.get(key) : null,
      setItem: (key, value) => sessionStorageData.set(key, String(value)),
      removeItem: (key) => sessionStorageData.delete(key),
    },
    location: {href: 'http://localhost:8000/', origin: 'http://localhost:8000'},
    URL,
    SESSION_ID: 'sess-1',
    SESSION_BOOTSTRAP: {messages: [{id: 7}, {id: 8}]},
    OpenedSessions: {openCase: () => 'B'},
    CSS: {escape: (value) => value},
    performance: {
      now: () => clock - navigationStart,
      getEntriesByType: (type) => (type === 'resource' ? resourceEntries : type === 'navigation' ? navigationEntries : []),
      clearResourceTimings: () => { resourceBufferClears++; },
    },
    fetch: async (url, opts = {}) => {
      if (url === CLIENT_EVENTS_URL) {
        posts.push(JSON.parse(opts.body));
        postOptions.push(opts);
        return {ok: true, status: 200, json: async () => ({ok: true})};
      }
      throw new Error('unexpected fetch: ' + url);
    },
    Date: class extends Date {
      static now() { return clock; }
      constructor(...args) { super(...(args.length ? args : [clock])); }
    },
    // The cutoffs record instead of firing inline: a no-echo send's post is the
    // timer callback under test.
    setTimeout: (fn, ms) => {
      timers.push({fn, ms});
      return timers.length;
    },
    clearTimeout: (id) => { delete timers[id - 1]; },
    setInterval: (fn) => {
      flushFns.push(fn);
      return flushFns.length;
    },
    clearInterval: () => {},
  };
  context.globalThis = context;
  context.window = {
    addEventListener: (type, listener) => {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(listener);
    },
  };
  // The observer delivers when the test feeds it entries; observe records
  // nothing by itself, so the test drives exactly the deliveries it asserts on.
  context.PerformanceObserver = class {
    constructor(callback) { context.__trafficObserver = callback; }
    observe() {}
  };
  vm.createContext(context);
  vm.runInContext(readStatic('page-timers.js'), context, {filename: 'page-timers.js'});
  vm.runInContext(readPackageStatic('features', 'diag', 'client-events.js'), context, {filename: 'client-events.js'});

  return {
    context,
    posts,
    postOptions,
    resourceEntries,
    renderMessage(selector) { renderedSelectors.add(selector); },
    notifyMutations() {
      for (const observer of mutationObservers) if (observer.connected) observer.callback([]);
    },
    sessionStorageData,
    timers,
    flush() { flushFns.forEach((fn) => fn()); },
    observeResources(entries) { context.__trafficObserver({getEntries: () => entries}); },
    dispatch(type, detail) {
      context.document.dispatchEvent(new CustomEvent(type, {detail}));
    },
    fireTimers() {
      const pending = timers.splice(0).filter(Boolean);
      for (const timer of pending) timer.fn();
    },
    setVisibility(value) { context.document.visibilityState = value; },
    setNavigation(entry) { navigationEntries = entry ? [entry] : []; },
    resourceBufferClears() { return resourceBufferClears; },
    advance(ms) { clock += ms; },
    clock() { return clock; },
  };
}

// ---------------------------------------------------------------------------
// Send timing: one POST per send, both waits, retries counted
// ---------------------------------------------------------------------------

test('a send posts once, when the echo lands on its response, with both waits', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-send-click', {request_id: 'r1'});
  h.advance(300);
  h.dispatch('charliebot:client-send-response', {request_id: 'r1', ok: true});
  assert.equal(h.posts.length, 0, 'the send waits for the echo');

  h.advance(320);
  h.dispatch('charliebot:client-send-echo', {request_id: 'r1'});

  assert.equal(h.posts.length, 1);
  const post = h.posts[0];
  assert.equal(post.kind, 'send');
  assert.equal(post.request_id, 'r1');
  assert.equal(post.click_to_response_ms, 300);
  assert.equal(post.click_to_echo_ms, 620);
  assert.equal(post.retries, 0);
  assert.match(post.client_ts, /^\d{4}-\d{2}-\d{2}T/);
});

test('an echo that beats its response still posts once, after the response', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-send-click', {request_id: 'r1'});
  h.advance(100);
  h.dispatch('charliebot:client-send-echo', {request_id: 'r1'});
  assert.equal(h.posts.length, 0);

  h.advance(200);
  h.dispatch('charliebot:client-send-response', {request_id: 'r1', ok: true});

  assert.equal(h.posts.length, 1);
  assert.deepEqual([h.posts[0].click_to_response_ms, h.posts[0].click_to_echo_ms], [300, 100]);
});

test('a refused POST posts with a null echo and the retry count', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-send-click', {request_id: 'r1'});
  h.advance(50);
  h.dispatch('charliebot:client-send-retry', {request_id: 'r1'});
  h.advance(3000);
  h.dispatch('charliebot:client-send-retry', {request_id: 'r1'});
  h.advance(500);
  h.dispatch('charliebot:client-send-response', {request_id: 'r1', ok: false});

  assert.equal(h.posts.length, 1);
  const post = h.posts[0];
  assert.equal(post.click_to_response_ms, 3550);
  assert.equal(post.click_to_echo_ms, null, 'a refused POST gets no echo');
  assert.equal(post.retries, 2);
});

test('a retry after a failed response waits for its echo before posting', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-send-click', {request_id: 'r1'});
  h.advance(200);
  h.dispatch('charliebot:client-send-retry', {request_id: 'r1'});
  h.advance(800);
  h.dispatch('charliebot:client-send-response', {request_id: 'r1', ok: true});
  assert.equal(h.posts.length, 0, 'the retry waits for its echo');

  h.advance(2000);
  h.dispatch('charliebot:client-send-echo', {request_id: 'r1'});

  assert.equal(h.posts.length, 1);
  assert.deepEqual([h.posts[0].click_to_response_ms, h.posts[0].click_to_echo_ms, h.posts[0].retries],
      [1000, 3000, 1]);
});

test('a send whose echo never comes posts at the cutoff with a null echo', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-send-click', {request_id: 'r1'});
  h.advance(300);
  h.dispatch('charliebot:client-send-response', {request_id: 'r1', ok: true});
  assert.equal(h.posts.length, 0, 'the send still waits for its echo');

  h.advance(120000);
  assert.equal(h.timers.length, 1);
  assert.equal(h.timers[0].ms, 120000, 'the cutoff is one send-timing TTL after the POST returned');
  h.fireTimers();

  assert.equal(h.posts.length, 1);
  assert.equal(h.posts[0].click_to_response_ms, 300);
  assert.equal(h.posts[0].click_to_echo_ms, null, 'the echo never arrived');
  assert.equal(h.posts[0].retries, 0);
});

test('an echo before the cutoff cancels the cutoff post', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-send-click', {request_id: 'r1'});
  h.dispatch('charliebot:client-send-response', {request_id: 'r1', ok: true});
  h.dispatch('charliebot:client-send-echo', {request_id: 'r1'});
  assert.equal(h.posts.length, 1);

  h.fireTimers();
  assert.equal(h.posts.length, 1, 'the cutoff timer was cleared with the entry');
});

test('a page hide posts the pending no-echo sends', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-send-click', {request_id: 'r1'});
  h.advance(400);
  h.dispatch('charliebot:client-send-response', {request_id: 'r1', ok: true});

  h.dispatch('pagehide', {});

  assert.equal(h.posts.length, 1, 'the hide posts the send the echo never reached');
  assert.equal(h.posts[0].request_id, 'r1');
  assert.equal(h.posts[0].click_to_response_ms, 400);
  assert.equal(h.posts[0].click_to_echo_ms, null);
  assert.equal(h.postOptions[0].keepalive, true, 'the hide post must outlive the unload');

  h.fireTimers();
  assert.equal(h.posts.length, 1, 'the posted send is gone: the cutoff posts no second event');
});

test('a responded send keeps its cutoff when a later click arrives past the TTL', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-send-click', {request_id: 'r1'});
  h.advance(60000);
  h.dispatch('charliebot:client-send-response', {request_id: 'r1', ok: true});
  h.advance(70000);
  h.dispatch('charliebot:client-send-click', {request_id: 'r2'});

  h.fireTimers();
  assert.equal(h.posts.length, 1, 'the cutoff still reports r1, 130 s after its click');
  assert.equal(h.posts[0].request_id, 'r1');
  assert.equal(h.posts[0].click_to_response_ms, 60000);
  assert.equal(h.posts[0].click_to_echo_ms, null);
});

test('a click whose POST never returns still ages out silently', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-send-click', {request_id: 'r1'});
  h.advance(120000 + 1);
  h.dispatch('charliebot:client-send-click', {request_id: 'r2'});
  h.dispatch('charliebot:client-send-response', {request_id: 'r1', ok: true});
  assert.equal(h.posts.length, 0, 'the aged-out click reports nothing rather than a wrong wait');
  h.fireTimers();
  assert.equal(h.posts.length, 0);
});

// ---------------------------------------------------------------------------
// Socket episodes: one POST per episode, the cause separated
// ---------------------------------------------------------------------------

test('a switch close that reconnects and replays posts one socket event', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-socket-close', {cause: 'switch', code: null});
  h.advance(1000);
  h.dispatch('charliebot:client-socket-open', {attempts: 1});
  h.advance(900);
  h.dispatch('charliebot:client-socket-catchup', {bytes: 80860});

  assert.equal(h.posts.length, 1);
  const post = h.posts[0];
  assert.equal(post.kind, 'socket');
  assert.equal(post.cause, 'switch');
  assert.equal(post.close_code, null);
  assert.equal(post.reconnect_count, 1);
  assert.equal(post.catchup_bytes, 80860);
  assert.match(post.disconnect_at, /^\d{4}-\d{2}-\d{2}T/);
  assert.match(post.reconnect_at, /^\d{4}-\d{2}-\d{2}T/);
  assert.match(post.catchup_at, /^\d{4}-\d{2}-\d{2}T/);
  assert.equal(new Date(post.reconnect_at) - new Date(post.disconnect_at), 1000);
});

test('a network close with no reconnect posts when the next close supersedes it', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-socket-close', {cause: 'network', code: 1006});
  h.dispatch('charliebot:client-socket-close', {cause: 'network', code: 1006});

  assert.equal(h.posts.length, 1, 'the first episode posts when the second close starts');
  const post = h.posts[0];
  assert.equal(post.cause, 'network', 'the readback counts only network episodes as disconnect waits');
  assert.equal(post.close_code, 1006);
  assert.equal(post.reconnect_at, null);
  assert.equal(post.catchup_bytes, null);
});

test('an episode that reconnects but never replays posts with null catch-up fields', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-socket-close', {cause: 'network', code: 1006});
  h.dispatch('charliebot:client-socket-open', {attempts: 3});
  h.dispatch('charliebot:client-socket-close', {cause: 'network', code: 1006});

  assert.equal(h.posts.length, 1);
  assert.deepEqual(
    [h.posts[0].reconnect_at === null, h.posts[0].reconnect_count, h.posts[0].catchup_at, h.posts[0].catchup_bytes],
    [false, 3, null, null]);
});

test('a catchup without a reconnect (the page-load open) posts nothing', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-socket-catchup', {bytes: 500});
  assert.equal(h.posts.length, 0);
});

// ---------------------------------------------------------------------------
// Traffic: one POST per flush, ids folded out of the paths
// ---------------------------------------------------------------------------

function resourceEntry(name, transferSize) {
  return {name, transferSize};
}

test('a traffic flush groups the window by endpoint with the ids folded out', () => {
  const h = buildClientEventsHarness();
  h.observeResources([
    resourceEntry('http://localhost:8000/api/sessions/0f0e1d2c-3b4a-5968-7789-aabbccddeeff/bootstrap', 26592),
    resourceEntry('http://localhost:8000/api/sessions/', 113004),
    resourceEntry('http://localhost:8000/api/sessions/', 113004),
    resourceEntry('http://localhost:8000/api/concurrency/now', 365),
  ]);
  h.advance(60000);
  h.flush();

  assert.equal(h.posts.length, 1);
  const post = h.posts[0];
  assert.equal(post.kind, 'traffic');
  assert.equal(post.window_s, 60);
  assert.equal(post.visibility_state, 'visible');
  assert.deepEqual(post.endpoints, [
    {endpoint: '/api/sessions/', transfer_size: 226008, count: 2},
    {endpoint: '/api/sessions/{id}/bootstrap', transfer_size: 26592, count: 1},
    {endpoint: '/api/concurrency/now', transfer_size: 365, count: 1},
  ]);
});

test('a traffic flush covers a hidden tab’s accumulated minutes in one window', () => {
  const h = buildClientEventsHarness();
  h.observeResources([resourceEntry('http://localhost:8000/api/concurrency/now', 365)]);
  h.advance(3600 * 1000);
  h.flush();

  assert.equal(h.posts.length, 1);
  assert.equal(h.posts[0].window_s, 3600, 'the window carries the seconds, so the readback divides by tab-time');
  assert.deepEqual(h.posts[0].endpoints, [{endpoint: '/api/concurrency/now', transfer_size: 365, count: 1}]);
});

test('an empty window still flushes, so the tab-hour denominator survives an idle minute', () => {
  const h = buildClientEventsHarness();
  h.advance(60000);
  h.flush();
  assert.equal(h.posts.length, 1);
  assert.deepEqual([h.posts[0].endpoints, h.posts[0].window_s], [[], 60]);
});

test('a traffic flush clears the resource buffer the observer has already counted', () => {
  const h = buildClientEventsHarness();
  h.observeResources([resourceEntry('http://localhost:8000/api/concurrency/now', 365)]);
  h.advance(60000);
  h.flush();

  assert.equal(h.resourceBufferClears(), 1,
      'the buffer stays under its limit so a switch can still read its bootstrap entry');
  assert.equal(h.posts[0].endpoints[0].count, 1, 'the observer counted the entry before the buffer dropped it');
});

test('the traffic flush reports the tab’s visibility', () => {
  const h = buildClientEventsHarness();
  h.setVisibility('hidden');
  h.advance(60000);
  h.flush();
  assert.equal(h.posts[0].visibility_state, 'hidden');
});

test('the flush clears its window, so the next flush reports only its own minutes', () => {
  const h = buildClientEventsHarness();
  h.observeResources([resourceEntry('http://localhost:8000/api/sessions/status', 1000)]);
  h.advance(60000);
  h.flush();
  h.advance(60000);
  h.flush();
  assert.equal(h.posts.length, 2);
  assert.deepEqual(h.posts[1].endpoints, []);
});

// ---------------------------------------------------------------------------
// Page load: the announced cause reaches the next page's report
// ---------------------------------------------------------------------------

test('a navigating path announces its cause and the report consumes it once', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:page-load-cause', {cause: 'stale_asset'});
  assert.match(h.sessionStorageData.get('charliebot-page-load-cause'), /stale_asset/,
      'the announcement lands where the next page reads it');

  h.setNavigation({responseEnd: 33000.4, domContentLoadedEventEnd: 34500.6, transferSize: 415115});
  h.dispatch('charliebot:client-socket-catchup', {bytes: 0});
  h.dispatch('load');
  h.fireTimers();  // the load handler schedules its report on a setTimeout

  assert.equal(h.posts.length, 1);
  const post = h.posts[0];
  assert.equal(post.kind, 'page_load');
  assert.deepEqual([post.response_end_ms, post.dom_content_loaded_ms, post.transfer_size],
      [33000, 34501, 415115]);
  assert.equal(post.cause, 'stale_asset');
  assert.equal(h.sessionStorageData.has('charliebot-page-load-cause'), false,
      'the report consumes the marker, so a later report names a fresh open');
});

test('a cause marker past its minute reports as a fresh open', () => {
  const h = buildClientEventsHarness();
  h.sessionStorageData.set('charliebot-page-load-cause',
      JSON.stringify({cause: 'stale_asset', at: h.clock() - 61000}));
  h.setNavigation({responseEnd: 100, domContentLoadedEventEnd: 200, transferSize: 0});
  h.dispatch('charliebot:client-socket-catchup', {bytes: 0});
  h.dispatch('load');
  h.fireTimers();  // the load handler schedules its report on a setTimeout
  assert.equal(h.posts[0].cause, 'open');
});

test('a page load reports the newest message, the catch-up, the open case and the blocking files', () => {
  const h = buildClientEventsHarness();
  h.setNavigation({name: PAGE_URL, responseEnd: 1300, domContentLoadedEventEnd: 1400, transferSize: 12600,
    requestStart: 0, responseStart: 300, encodedBodySize: 12500});
  h.observeResources([MARKED_ENTRY, TAILWIND_ENTRY]);
  h.advance(900);
  h.renderMessage('[data-message-id="8"]');
  h.notifyMutations();
  h.advance(1000);
  h.dispatch('charliebot:page-current', {sessionId: 'session-a'});
  assert.equal(h.posts.length, 0, 'the report waits for the load event');

  h.dispatch('load');
  assert.equal(h.posts.length, 1);
  const post = h.posts[0];
  assert.equal(post.kind, 'page_load');
  assert.equal(post.newest_message_ms, 900);
  assert.equal(post.current_ms, 1900);
  assert.equal(post.open_case, 'B');
  assert.equal(post.content_source, 'network');
  assert.equal(post.offline_shell, false);
  // The deferred script does not block rendering, and the same-origin stylesheet is not cross-origin.
  assert.deepEqual(post.render_blocking, [
    {url: FONTS_URL, start_ms: null, response_end_ms: null, transfer_size: null, encoded_body_size: null},
    {url: MARKED_URL, start_ms: 400, response_end_ms: 901, transfer_size: 2500, encoded_body_size: 2500},
  ]);
  assert.deepEqual(post.same_origin, [
    {url: PAGE_URL, request_start_ms: 0, response_start_ms: 300, response_end_ms: 1300, encoded_body_size: 12500},
    {url: TAILWIND_URL, request_start_ms: 310, response_start_ms: 320, response_end_ms: 330, encoded_body_size: 3900},
  ]);
});

test('a page load whose catch-up never comes reports it as null once the wait passes', () => {
  const h = buildClientEventsHarness();
  h.setNavigation({name: PAGE_URL, responseEnd: 1300, domContentLoadedEventEnd: 1400, transferSize: 12600,
    requestStart: 0, responseStart: 300, encodedBodySize: 12500});
  h.dispatch('load');
  assert.equal(h.posts.length, 0, 'the report waits for the catch-up or for the wait to pass');

  h.fireTimers();
  assert.equal(h.posts.length, 1);
  assert.equal(h.posts[0].current_ms, null);
  assert.equal(h.posts[0].newest_message_ms, null, 'no newest message was rendered');
});

// The open case of one chat page: the browser's record decides it, and the page then records its own open.
function openCaseOf(storage, sessionId) {
  const context = {
    localStorage: {
      getItem: (key) => (storage.has(key) ? storage.get(key) : null),
      setItem: (key, value) => storage.set(key, String(value)),
    },
    SESSION_ID: sessionId,
    console: {warn: () => {}},
  };
  vm.createContext(context);
  vm.runInContext(readStatic('opened-sessions.js'), context, {filename: 'opened-sessions.js'});
  return vm.runInContext('OpenedSessions.openCase()', context);
}

test('the open case is A for a session this browser opened, B for another, C for none, and no session has none', () => {
  const storage = new Map();
  assert.equal(openCaseOf(storage, 'sess-1'), 'C');
  assert.equal(openCaseOf(storage, 'sess-1'), 'A');
  assert.equal(openCaseOf(storage, 'sess-2'), 'B');
  assert.equal(openCaseOf(storage, null), null);
});

test('a page without a Navigation Timing entry reports nothing', () => {
  const h = buildClientEventsHarness();
  h.dispatch('charliebot:client-socket-catchup', {bytes: 0});
  h.dispatch('load');
  assert.equal(h.posts.length, 0);
});

// ---------------------------------------------------------------------------
// The transport's announcements: the switch close against the network close
// ---------------------------------------------------------------------------

// websocket.js in a context whose document records the page events it
// announces, with a fake socket the test drives.
function buildTransportHarness() {
  const announced = [];
  const timers = [];
  const {context, FakeWebSocket} = loadWebsocketContext({
    SESSION_ID: 'session-a',
    WebSocket: createFakeWebSocketClass(),
    wsUrlWithToken: (path) => path,
    setTimeout: (fn, ms) => { timers.push({fn, ms}); return timers.length; },
    clearTimeout: () => {},
    document: {
      getElementById: () => null,
      dispatchEvent: (event) => {
        announced.push({type: event.type, detail: event.detail});
        return true;
      },
    },
    console: {log: () => {}, error: () => {}},
  });
  return {context, FakeWebSocket, announced, timers};
}

test('the transport announces a switch close with cause switch and no code', () => {
  const h = buildTransportHarness();
  h.context.connectWS();
  h.FakeWebSocket.instances[0].emitOpen();
  h.context.disconnectWS();

  const close = h.announced.find((event) => event.type === 'charliebot:client-socket-close');
  assert.deepEqual({...close.detail}, {cause: 'switch', code: null},
      'the page closed its own socket: the readback must never count this as a network wait');
});

test('a page-driven close with no open socket announces nothing', () => {
  const h = buildTransportHarness();
  h.context.connectWS();
  h.context.disconnectWS();

  assert.equal(h.announced.filter((event) => event.type === 'charliebot:client-socket-close').length, 0,
      'no socket was open, so there is no disconnect to report');
});

test('the transport announces a socket-driven close as a network close with the code', () => {
  const h = buildTransportHarness();
  h.context.connectWS();
  const socket = h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1];
  socket.emitOpen();
  socket.emitClose({code: 1006});

  const close = h.announced.filter((event) => event.type === 'charliebot:client-socket-close').pop();
  assert.deepEqual({...close.detail}, {cause: 'network', code: 1006});
  assert.deepEqual(h.timers.map((timer) => timer.ms), [1000, 250], 'the close schedules its reconnect and quick retry');
});

test('the transport announces the stale-asset close once and marks the reload cause', () => {
  const h = buildTransportHarness();
  h.context.connectWS();
  const socket = h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1];
  socket.emitOpen();
  socket.emitClose({code: 4409});

  const close = h.announced.filter((event) => event.type === 'charliebot:client-socket-close').pop();
  assert.deepEqual({...close.detail}, {cause: 'network', code: 4409});
  const cause = h.announced.filter((event) => event.type === 'charliebot:page-load-cause').pop();
  assert.deepEqual({...cause.detail}, {cause: 'stale_asset'});
  assert.equal(h.timers.length, 0, 'the stale-asset close reloads instead of scheduling a reconnect');
});

test('the transport announces the reopen with the reconnect attempt count', () => {
  const h = buildTransportHarness();
  h.context.connectWS();
  let socket = h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1];
  socket.emitClose({code: 1006});           // first attempt dies before opening
  assert.equal(h.announced.filter((event) => event.type === 'charliebot:client-socket-close').length, 0,
      'a reconnect attempt that never opened is part of the disconnect no close was announced for');
  h.timers.pop().fn();                      // the quick retry fires the second attempt
  socket = h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1];
  socket.emitOpen();

  const open = h.announced.filter((event) => event.type === 'charliebot:client-socket-open').pop();
  assert.deepEqual({...open.detail}, {attempts: 2});

  socket.emitMessage({type: 'catchup_complete', event_total: 0});
  const catchup = h.announced.filter((event) => event.type === 'charliebot:client-socket-catchup').pop();
  assert.deepEqual({...catchup.detail}, {bytes: 0, eventTotal: 0}, 'an empty replay reports zero bytes');
});

test('the transport counts the replay bytes up to catchup_complete', () => {
  const h = buildTransportHarness();
  h.context.connectWS();
  const socket = h.FakeWebSocket.instances[h.FakeWebSocket.instances.length - 1];
  socket.emitOpen();

  socket.emitMessage({type: 'ping'});
  socket.emitMessage({type: 'message', message: {role: 'assistant', content: 'héllo 世界', event_index: 1}});
  socket.emitMessage({type: 'catchup_complete', event_total: 2});
  socket.emitMessage({type: 'message', message: {role: 'assistant', content: 'after the replay', event_index: 2}});

  const catchup = h.announced.find((event) => event.type === 'charliebot:client-socket-catchup');
  const replayed = JSON.stringify({type: 'ping'}) + JSON.stringify(
    {type: 'message', message: {role: 'assistant', content: 'héllo 世界', event_index: 1}});
  assert.deepEqual({...catchup.detail}, {bytes: Buffer.byteLength(replayed, 'utf8'), eventTotal: 2},
      'the replay bytes are the frames before catchup_complete, in UTF-8');
});
