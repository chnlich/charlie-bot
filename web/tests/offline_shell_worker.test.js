// The offline shell worker, run as the browser runs it: the real source with the config line the server prepends,
// against fake caches, a scripted network and the worker's own global scope.

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const SOURCE = fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'offline-shell-worker.js'), 'utf8');
const ORIGIN = 'https://charlie.test';
const SHELL_URL = ORIGIN + '/?session=';
const SHELL_KEY = ORIGIN + '/shell';
const STORED_AT = 'X-CharlieBot-Stored-At';
const NETWORK_ONLY = 'X-CharlieBot-Network-Only';
const SESSION = 'session-a';
const SHELL_FLAG_URL = ORIGIN + '/stale-shell';
const STALE_NETWORK_CAP_MS = 5000;
const BOOTSTRAP_URL = ORIGIN + '/api/sessions/' + SESSION + '/bootstrap';
const LIST_URL = ORIGIN + '/api/sessions/?compact=1&screen=1&active=' + SESSION;

const WAIT_CAP_MS = 400;
// The copies sit in the data cache named for the copy format; the shell sits in the shell cache named for the release.
const DATA_CACHE = 'last-state-data-1';
const SHELL_CACHE = 'last-state-shell-v1';
async function openStores(h) {
  return {copies: await h.caches.api.open(DATA_CACHE), shells: await h.caches.api.open(SHELL_CACHE)};
}

// A network answer the test releases by hand, so no test waits on a real timer.
function deferred() {
  let release;
  const promise = new Promise((resolve) => { release = resolve; });
  return {promise, release};
}

// Lets the worker's pending steps run up to their next timer, the fake clock's cue to advance.
const flush = () => new Promise((resolve) => setImmediate(resolve));

// A settle's re-reads chain through timers the fake clock queues as earlier ones fire, so a test
// advances the clock one wait cap at a time, flushing between, until its step ran.
async function settleCaps(t, count) {
  for (let cap = 0; cap < count; cap++) {
    t.mock.timers.tick(WAIT_CAP_MS);
    await flush();
  }
}

function makeCaches() {
  const stores = new Map();
  const keyOf = (input) => new URL(typeof input === 'string' ? input : input.url, ORIGIN).href;
  const api = {
    // Keys here miss their first match, the way a just-started worker's match can race the cache's
    // disk index; the index settles once per worker, so the first match per key misses and every
    // later match answers. The settle re-reads are what the race tests exercise.
    firstMatchMisses: new Set(),
    matched: new Set(),
    open: async (name) => {
      if (!stores.has(name)) stores.set(name, new Map());
      const entries = stores.get(name);
      return {
        match: async (input) => {
          const key = keyOf(input);
          if (api.firstMatchMisses.has(key) && !api.matched.has(key)) {
            api.matched.add(key);
            return undefined;
          }
          return entries.has(key) ? entries.get(key).clone() : undefined;
        },
        put: async (input, response) => {
          if (api.quotaFailures > 0 && name === DATA_CACHE) {
            api.quotaFailures -= 1;
            const error = new Error('storage is full');
            error.name = 'QuotaExceededError';
            throw error;
          }
          entries.set(keyOf(input), response);
        },
        delete: async (input) => entries.delete(keyOf(input)),
        keys: async () => [...entries.keys()].map((url) => ({url})),
      };
    },
    keys: async () => [...stores.keys()],
    delete: async (name) => stores.delete(name),
    quotaFailures: 0,
  };
  return {api, stores};
}

// One worker: its listeners, its caches, and the network it reaches. `network(request, init)` answers a fetch.
function harness({version = 'v1', enabled = true, cacheStorage = makeCaches()} = {}) {
  const listeners = {};
  const cachesFake = cacheStorage;
  const state = {claimed: 0, unregistered: 0, preloadEnabled: 0, network: null, calls: []};
  const self = {
    location: {origin: ORIGIN},
    addEventListener(type, handler) {
      (listeners[type] ||= []).push(handler);
    },
    skipWaiting: async () => {},
    clients: {claim: async () => { state.claimed += 1; }},
    registration: {
      unregister: async () => { state.unregistered += 1; },
      navigationPreload: {enable: async () => { state.preloadEnabled += 1; }},
    },
  };
  const fetch = async (input, init) => {
    const request = typeof input === 'string' ? {url: new URL(input, ORIGIN).href, method: 'GET', headers: new Headers()} : input;
    state.calls.push({url: request.url, init});
    if (!state.network) throw new TypeError('network unreachable');
    return state.network(request, init);
  };
  const context = vm.createContext({
    self, fetch, caches: cachesFake.api, URL, Response, Headers, Error, Promise, Date, Set, Map, JSON, Object, Array,
    String, Number, RegExp, console, setTimeout, clearTimeout, AbortController,
  });
  vm.runInContext(`const WORKER_CONFIG = ${JSON.stringify({version, enabled})};\n${SOURCE}`, context);
  return {
    state,
    caches: cachesFake,
    // Fires one event; returns its waitUntil promises and the response its respondWith answered with, if any.
    fire(type, props = {}) {
      const event = {
        ...props,
        waits: [],
        responses: [],
        waitUntil(promise) { this.waits.push(Promise.resolve(promise)); },
        respondWith(promise) { this.responses.push(Promise.resolve(promise)); },
      };
      for (const handler of listeners[type] || []) handler(event);
      return event;
    },
    hasListener: (type) => (listeners[type] || []).length > 0,
  };
}

function request(url, {method = 'GET', mode = 'cors', headers = {}} = {}) {
  return {url, method, mode, headers: new Headers(headers)};
}

// A message channel the test reads the worker's replies from: the worker posts to the port it is given.
function messageChannel() {
  const replies = [];
  return {replies, port2: {postMessage: (message) => replies.push(message)}, port1: {onmessage: null}};
}

// Answers one fetch event with the response (or the rejection), once the background work it kept has ended.
async function answer(h, req) {
  const result = await answerNow(h, req);
  await Promise.all(result.waits);
  return result;
}

// Answers one fetch event with the response, leaving the background work the worker kept to the caller.
async function answerNow(h, req) {
  const event = h.fire('fetch', {request: req});
  if (event.responses.length === 0) return {responded: false, waits: event.waits};
  const response = await event.responses[0];
  return {responded: true, response, waits: event.waits};
}

function firstScreen(body, extra = {}) {
  return new Response(JSON.stringify({body, event_count: 3}), {
    status: 200, headers: {'Content-Type': 'application/json', ETag: '"s1"', ...extra},
  });
}

test('the first load reaches the network, and a warm shell then answers the navigation', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async (req) => (req.url === SHELL_URL
    ? new Response('<html><body>shell</body></html>', {status: 200, headers: {ETag: '"shell"'}})
    : new Response('unexpected', {status: 500}));
  const nav = request(SHELL_URL, {mode: 'navigate'});
  const pending = answer(h, nav);
  await flush();
  await settleCaps(t, 2);
  const first = await pending;
  assert.equal(await first.response.text(), '<html><body>shell</body></html>', 'the first load is the network page');

  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 1, 'the warm answers once');
  assert.equal(channel.replies[0].stored, true, 'the warm reports the stored shell over its channel');
  const second = await answer(h, nav);
  assert.equal(await second.response.text(), '<html><body>shell</body></html>');
  assert.equal(h.state.calls.filter((call) => call.url === SHELL_URL).length, 2,
      'the first load and the warm fetch reached the network; the stored shell answers the second navigation');
});

test('a warm stores server-rendered copies before it reports success', async () => {
  const h = harness();
  h.state.network = async (req) => (req.url === SHELL_URL
    ? new Response('<html><body>shell</body></html>', {status: 200})
    : new Response('unexpected', {status: 500}));
  const bootstrap = {event_count: 4, session: {id: SESSION}};
  const listing = {$encoded: {ids: [SESSION]}, $screen: {partial: false}};
  const channel = messageChannel();
  await Promise.all(h.fire('message', {
    data: {
      type: 'warm',
      copies: [
        {url: BOOTSTRAP_URL, body: bootstrap},
        {url: LIST_URL, body: listing},
      ],
    },
    ports: [channel.port2],
  }).waits);

  const cache = await h.caches.api.open(DATA_CACHE);
  const storedBootstrap = await cache.match(BOOTSTRAP_URL);
  const storedListing = await cache.match(LIST_URL);
  assert.deepEqual(await storedBootstrap.json(), bootstrap);
  assert.ok(storedBootstrap.headers.get(STORED_AT));
  assert.deepEqual(await storedListing.json(), listing);
  assert.equal(channel.replies[0].stored, true);
  assert.deepEqual(h.state.calls.map((call) => call.url), [SHELL_URL], 'seeding uses the page data without a fetch');
});

test('a shell match that races the cache index settles and answers the stored shell', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  const {copies: store, shells} = await openStores(h);
  await shells.put(SHELL_KEY, new Response('<html>stored</html>'));
  h.caches.api.firstMatchMisses.add(SHELL_KEY);
  h.state.network = async () => {
    throw new TypeError('the network must not be reached while the stored shell answers');
  };
  const pending = answer(h, request(SHELL_URL, {mode: 'navigate'}));
  await flush();
  t.mock.timers.tick(WAIT_CAP_MS);
  const nav = await pending;
  assert.equal(await nav.response.text(), '<html>stored</html>', 'the stored shell answers the navigation');
  assert.deepEqual(h.state.calls, [], 'the re-read replaced the network fetch, not the other way round');
});

test('a navigation with no stored shell fetches the network after the settle', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  const network = deferred();
  h.state.network = () => network.promise.then(() => new Response('<html>network</html>'));
  const pending = answer(h, request(SHELL_URL, {mode: 'navigate'}));
  await flush();
  assert.deepEqual(h.state.calls, [], 'the settle re-reads the cache before it reaches the network');
  await settleCaps(t, 2);
  assert.equal(h.state.calls.length, 1, 'the network fetch starts after the settle');
  network.release();
  const nav = await pending;
  assert.equal(await nav.response.text(), '<html>network</html>');
});

test('a warm that finds the shell stored fetches nothing and reports the shell it has', async () => {
  const h = harness();
  h.state.network = async (req) => (req.url === SHELL_URL
    ? new Response('<html><body>shell</body></html>', {status: 200, headers: {ETag: '"shell"'}})
    : new Response('unexpected', {status: 500}));
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
  const again = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [again.port2]}).waits);
  assert.equal(again.replies.length, 1);
  assert.equal(again.replies[0].stored, true);
  assert.equal(h.state.calls.filter((call) => call.url === SHELL_URL).length, 1,
      'the second warm of the same release fetches no shell');
});

test('a warm whose shell fetch fails reports the failure', async () => {
  const h = harness();
  h.state.network = async () => new Response('no', {status: 503});
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 1);
  assert.equal(channel.replies[0].stored, false);
  assert.match(channel.replies[0].error, /503/);
});

test('a warm whose shell fetch answers 401 reports no stored shell without an error', async () => {
  const h = harness();
  h.state.network = async () => new Response('unauthorized', {status: 401});
  const cache = await h.caches.api.open(DATA_CACHE);
  await cache.put(BOOTSTRAP_URL, firstScreen('private copy'));
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 1);
  assert.equal(channel.replies[0].stored, false);
  assert.equal('error' in channel.replies[0], false);
  assert.equal(await cache.match(BOOTSTRAP_URL), undefined, 'a 401 drops the stored first screen');
});

test('a shell file 401 clears copies without storing an incomplete shell', async () => {
  const h = harness();
  const html = '<link href="/static/css/private.css" rel="stylesheet">';
  h.state.network = async (req) => (req.url === SHELL_URL
    ? new Response(html, {status: 200})
    : new Response('unauthorized', {status: 401}));
  const cache = await h.caches.api.open(DATA_CACHE);
  await cache.put(BOOTSTRAP_URL, firstScreen('private copy'));
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 1);
  assert.equal(channel.replies[0].stored, false);
  assert.equal('error' in channel.replies[0], false);
  assert.equal(await cache.match(SHELL_KEY), undefined, 'the shell is stored after its files');
  assert.equal(await cache.match(BOOTSTRAP_URL), undefined, 'the 401 drops the stored first screen');
});

test('the retired form answers no message', async () => {
  const h = harness({enabled: false});
  h.state.network = async () => new Response('unexpected', {status: 200});
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 0, 'a worker the switch retired stores nothing');
  assert.deepEqual(h.state.calls, [], 'a worker the switch retired fetches nothing');
});

test('warming stores the shell and the same-origin files it names, and drops the files it no longer names', async () => {
  const h = harness();
  const html = '<link href="/static/css/a.css?v=1" rel="stylesheet"><script src="/static/js/b.js"></script>' +
      '<script defer src="https://cdn.example/x.js"></script>';
  // The warm reads a file's body from the browser's HTTP cache with only-if-cached. A default-mode request for a file the
  // browser holds comes back as a bodyless 304, which the warm cannot store; a file the cache lacks fails with a TypeError.
  h.state.network = async (req, init) => {
    if (req.url === SHELL_URL) return new Response(html, {status: 200});
    if (!init || init.cache !== 'only-if-cached') return new Response(null, {status: 304});
    if (req.url === ORIGIN + '/static/css/a.css?v=1') return new Response('a', {status: 200, headers: {ETag: '"a"'}});
    if (req.url === ORIGIN + '/static/js/b.js') throw new TypeError('only-if-cached: not in the cache');
    throw new Error('unexpected ' + req.url);
  };
  const files = h.caches.api;
  await (await files.open('last-state-files')).put(ORIGIN + '/static/old.css', new Response('old'));
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 1);
  assert.equal(channel.replies[0].stored, true);

  const stored = await (await files.open('last-state-files')).keys();
  assert.deepEqual(stored.map((entry) => entry.url), [ORIGIN + '/static/css/a.css?v=1'],
      'the file with an ETag is stored; the file without one is not, and the old file is dropped');
});

test('a static file fetched on a cache miss is stored for the next request', async () => {
  const h = harness();
  const fileUrl = ORIGIN + '/static/features/cron/cron.js?v=1';
  let calls = 0;
  h.state.network = async () => {
    calls += 1;
    return new Response('feature()', {status: 200, headers: {ETag: '"asset"'}});
  };

  const first = await answer(h, request(fileUrl));
  assert.equal(await first.response.text(), 'feature()');
  const files = await h.caches.api.open('last-state-files');
  assert.equal(await (await files.match(fileUrl)).text(), 'feature()');

  const second = await answer(h, request(fileUrl));
  assert.equal(await second.response.text(), 'feature()');
  assert.equal(calls, 1, 'the next request uses the stored response');
});

test('a first screen with no stored copy takes the network answer, however long it takes', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  const network = deferred();
  h.state.network = () => network.promise.then(() => firstScreen('network'));
  const pending = answer(h, request(BOOTSTRAP_URL));
  await flush();
  t.mock.timers.tick(10 * WAIT_CAP_MS);
  network.release();
  const result = await pending;
  assert.equal(result.response.headers.get(STORED_AT), null, 'a network answer carries no stored-at header');
  assert.equal((await result.response.json()).body, 'network');
});

test('a stored first screen answers at once, sends no request, and the catch-up refresh replaces it', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));

  h.state.network = async () => firstScreen('two');
  const callsAtHit = h.state.calls.length;
  const copy = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await copy.response.json()).body, 'one');
  assert.ok(copy.response.headers.get(STORED_AT), 'a copy answer carries the time it was stored');
  assert.equal(h.state.calls.length, callsAtHit, 'the hit sends no request: the first screen is not on the link twice');

  // The page reports the catch-up complete, and the refresh replaces the copy for the next open.
  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  assert.equal(h.state.calls.length, callsAtHit + 1, 'the refresh is the one network request');
  const later = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await later.response.json()).body, 'two', 'the refreshed copy serves the next open');
});

test('a copy match that races the cache index answers at the cap, and its fetch is aborted', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));
  h.caches.api.firstMatchMisses.add(BOOTSTRAP_URL);

  const network = deferred();
  h.state.network = (req, init) => new Promise((resolve, reject) => {
    init.signal?.addEventListener('abort', () => reject(new TypeError('This request was aborted')));
    network.promise.then(() => resolve(firstScreen('two')));
  });
  const callsAtHit = h.state.calls.length;
  const pending = answer(h, request(BOOTSTRAP_URL));
  await flush();
  await settleCaps(t, 2);
  const hit = await pending;
  assert.equal((await hit.response.json()).body, 'one', 'the copy answers at the cap\'s re-read');
  assert.equal(h.state.calls.length, callsAtHit + 1, 'the network fetch of the hit made no call beyond its own');
  network.release();
  await flush();
  assert.equal(h.state.calls.length, callsAtHit + 1, 'the fetch was aborted, not left running');

  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  const later = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await later.response.json()).body, 'two', 'the catch-up refresh replaced the copy');
});

test('a copy hit answers from the copy when the network fails, and the refresh runs after the catch-up', async (t) => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));

  h.state.network = async () => { throw new TypeError('offline'); };
  const callsAtHit = h.state.calls.length;
  const hit = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await hit.response.json()).body, 'one', 'the copy answers the first screen while offline');
  assert.equal(h.state.calls.length, callsAtHit, 'the hit sends no request; the refresh waits for the catch-up');

  h.state.network = async () => firstScreen('two');
  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  const refreshed = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await refreshed.response.json()).body, 'two', 'the catch-up refresh replaced the copy');
});

test('a copy hit on one URL leaves the copies of other URLs queued for the same catch-up refresh', async () => {
  const h = harness();
  h.state.network = async () => new Response('<html>shell</html>', {status: 200});
  await Promise.all(h.fire('message', {
    data: {
      type: 'warm',
      copies: [
        {url: BOOTSTRAP_URL, body: {body: 'one'}},
        {url: LIST_URL, body: ['one']},
      ],
    },
  }).waits);

  h.state.network = async () => { throw new TypeError('offline'); };
  await answer(h, request(BOOTSTRAP_URL));
  await answer(h, request(LIST_URL));

  h.state.network = async (req) => (req.url === LIST_URL ? new Response('[]', {status: 200}) : firstScreen('two'));
  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  const {copies: store, shells} = await openStores(h);
  assert.equal(JSON.parse(await (await store.match(BOOTSTRAP_URL)).text()).body, 'two');
  assert.equal(await (await store.match(LIST_URL)).text(), '[]');
});

test('a 401 during the catch-up refresh drops every copy', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));

  h.state.network = async () => { throw new TypeError('offline'); };
  await answer(h, request(BOOTSTRAP_URL));

  h.state.network = async () => new Response('no', {status: 401});
  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  const {copies: store, shells} = await openStores(h);
  assert.equal(await store.match(BOOTSTRAP_URL), undefined, 'the refresh 401 empties the copy store');
});

// A list answer with no copy at hand: its one re-read waits a wait cap on the fake clock, then the network answers.
async function answerListWithCapWait(t, h, req) {
  const pending = answer(h, req);
  await flush();
  await settleCaps(t, 1);
  return pending;
}

// The page's ask after a reply the socket delivered: the stored first screen is re-fetched and re-stored,
// so an offline reopen reads a copy that carries the reply.
test('a refresh-copies ask re-fetches the stored first screen and reports the copy it re-stored', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));

  h.state.network = async () => firstScreen('two');
  const channel = messageChannel();
  await Promise.all(h.fire('message', {
    data: {type: 'refresh-copies', urls: [BOOTSTRAP_URL]},
    ports: [channel.port2],
  }).waits);
  assert.equal(channel.replies.length, 1, 'the ask answers over its channel once');
  assert.equal(channel.replies[0].refreshed, 1, 'the reply names the copy the refresh re-stored');
  const body = await (await (await h.caches.api.open(DATA_CACHE)).match(BOOTSTRAP_URL)).json();
  assert.equal(body.body, 'two', 'the stored copy carries the fresh answer');
});

test('a refresh-copies ask for a URL with no stored copy re-stores nothing', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  const channel = messageChannel();
  await Promise.all(h.fire('message', {
    data: {type: 'refresh-copies', urls: [BOOTSTRAP_URL]},
    ports: [channel.port2],
  }).waits);
  assert.equal(channel.replies[0].refreshed, 0);
  const {copies: store, shells} = await openStores(h);
  assert.equal(await store.match(BOOTSTRAP_URL), undefined, 'a refresh stores nothing the page did not already hold');
});

test('a 401 during a refresh-copies ask drops every copy', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));

  h.state.network = async () => new Response('no', {status: 401});
  const channel = messageChannel();
  await Promise.all(h.fire('message', {
    data: {type: 'refresh-copies', urls: [BOOTSTRAP_URL]},
    ports: [channel.port2],
  }).waits);
  assert.equal(channel.replies[0].refreshed, 0);
  const {copies: store, shells} = await openStores(h);
  assert.equal(await store.match(BOOTSTRAP_URL), undefined, 'the refresh 401 empties the copy store');
});


test('a stored list answers from its copy at once, sends no request, and the catch-up refresh replaces it', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async () => new Response('["one"]', {status: 200, headers: {ETag: '"l1"'}});
  await answerListWithCapWait(t, h, request(LIST_URL));
  h.state.network = async () => new Response('["two"]', {status: 200, headers: {ETag: '"l2"'}});
  const callsAtHit = h.state.calls.length;
  const copy = await answer(h, request(LIST_URL));
  assert.equal(await copy.response.text(), '["one"]');
  assert.ok(copy.response.headers.get(STORED_AT), 'a copy answer carries the time it was stored');
  assert.equal(h.state.calls.length, callsAtHit, 'the hit sends no request: the list is not on the link twice');

  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  assert.equal(h.state.calls.length, callsAtHit + 1, 'the refresh is the one network request');
  const later = await answer(h, request(LIST_URL));
  assert.equal(await later.response.text(), '["two"]', 'the refreshed copy serves the next open');
});

test('a list whose first match misses answers from its copy after one re-read, with no request', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async () => new Response('["one"]', {status: 200, headers: {ETag: '"l1"'}});
  await answerListWithCapWait(t, h, request(LIST_URL));
  h.caches.api.firstMatchMisses.add(LIST_URL);
  const callsAtHit = h.state.calls.length;
  const pending = answer(h, request(LIST_URL));
  await flush();
  await settleCaps(t, 1);
  const hit = await pending;
  assert.equal(await hit.response.text(), '["one"]', 'the re-read finds the copy the index had not yet shown');
  assert.equal(h.state.calls.length, callsAtHit, 'the re-read hit sends no request for the list');
});


test('a catch-up refresh that fails or answers 5xx leaves the stored list in place', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async () => new Response('["one"]', {status: 200, headers: {ETag: '"l1"'}});
  await answerListWithCapWait(t, h, request(LIST_URL));
  await answer(h, request(LIST_URL));

  h.state.network = async () => { throw new TypeError('offline'); };
  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  h.state.network = async () => new Response('busy', {status: 503});
  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  const after = await answer(h, request(LIST_URL));
  assert.equal(await after.response.text(), '["one"]', 'the failed and 5xx refreshes keep the stored list');
});


test('a 401 at the catch-up refresh drops every stored copy, including the list copy', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async () => new Response('["one"]', {status: 200, headers: {ETag: '"l1"'}});
  await answerListWithCapWait(t, h, request(LIST_URL));
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));
  // Both copies answer their next opens, so both wait in the queue for the catch-up refresh.
  await answer(h, request(LIST_URL));
  await answer(h, request(BOOTSTRAP_URL));

  h.state.network = async () => new Response('no', {status: 401});
  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  const {copies: store, shells} = await openStores(h);
  assert.equal(await store.match(LIST_URL), undefined, 'the refresh 401 empties the list copy too');
  assert.equal(await store.match(BOOTSTRAP_URL), undefined, 'and the first-screen copy');
});


test('a revalidating 304 at the catch-up refresh leaves the stored list as it is', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async () => new Response('["one"]', {status: 200, headers: {ETag: '"l1"'}});
  await answerListWithCapWait(t, h, request(LIST_URL));
  await answer(h, request(LIST_URL));

  // The refresh's 304 names no body, so it stores nothing and the copy answers the next open.
  h.state.network = async () => new Response(null, {status: 304, headers: {ETag: '"l1"'}});
  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  const again = await answer(h, request(LIST_URL));
  assert.equal(await again.response.text(), '["one"]', 'the bodyless 304 leaves the stored list');
  assert.notEqual(again.response.headers.get(STORED_AT), null, 'the answer is the stored copy');
});

test('a 304 with no copy is fetched again past the HTTP cache, and the body reaches the page', async () => {
  const h = harness();
  h.state.network = async (req, init) => (init && init.cache === 'no-store'
    ? firstScreen('fresh')
    : new Response(null, {status: 304}));
  const result = await answer(h, request(BOOTSTRAP_URL, {headers: {'If-None-Match': '"s1"'}}));
  assert.equal((await result.response.json()).body, 'fresh', 'the re-fetch brings the body');
  assert.equal(h.state.calls.length, 2, 'the 304 re-fetched once');
  assert.equal(h.state.calls[1].init.cache, 'no-store', 'the re-fetch bypasses the HTTP cache');
});

test('a network-only request reads no copy and waits for the network', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));
  h.state.network = async () => firstScreen('two');
  const result = await answer(h, request(BOOTSTRAP_URL, {headers: {[NETWORK_ONLY]: '1'}}));
  assert.equal(result.response.headers.get(STORED_AT), null);
  assert.equal((await result.response.json()).body, 'two');
});

test('a network-only list refresh consumes the queued copy refresh', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async () => new Response('["one"]', {status: 200, headers: {ETag: '"l1"'}});
  await answerListWithCapWait(t, h, request(LIST_URL));
  await answer(h, request(LIST_URL));

  h.state.network = async () => new Response('["two"]', {status: 200, headers: {ETag: '"l2"'}});
  const callsBeforeRefresh = h.state.calls.length;
  const refreshed = await answer(h, request(LIST_URL, {headers: {[NETWORK_ONLY]: '1'}}));
  assert.equal(await refreshed.response.text(), '["two"]');
  assert.equal(h.state.calls.length, callsBeforeRefresh + 1, 'the page refreshes the list once');

  await Promise.all(h.fire('message', {data: {type: 'catchup-complete'}}).waits);
  assert.equal(h.state.calls.length, callsBeforeRefresh + 1, 'catch-up does not fetch the list a second time');
  const later = await answer(h, request(LIST_URL));
  assert.equal(await later.response.text(), '["two"]', 'the successful page refresh stores the current list');
});

test('the worker answers only the listed GETs; writes, socket paths and the other /api routes pass through', async () => {
  const h = harness();
  h.state.network = async () => new Response('x', {status: 200});
  const passing = [
    request(ORIGIN + '/api/sessions/' + SESSION + '/read', {method: 'POST'}),
    request(ORIGIN + '/api/sessions/' + SESSION + '/events'),
    request(ORIGIN + '/api/sessions/?ids=a,b'),
    request(ORIGIN + '/'),
    request('https://other.example/api/sessions/' + SESSION + '/bootstrap'),
    request(ORIGIN + '/api/sessions/' + SESSION + '/bootstrap', {method: 'POST'}),
  ];
  for (const req of passing) {
    const result = await answer(h, req);
    assert.equal(result.responded, false, 'not answered by the worker: ' + req.method + ' ' + req.url);
  }
});


test('a copy-less list re-reads its copy after one wait cap, then takes the network answer', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  const network = deferred();
  h.state.network = () => network.promise.then(() => new Response('[]', {status: 200, headers: {ETag: '"l"'}}));
  const pending = answer(h, request(LIST_URL));
  await flush();
  assert.equal(h.state.calls.length, 0, 'no request is sent before the re-read');
  t.mock.timers.tick(WAIT_CAP_MS);
  await flush();
  assert.equal(h.state.calls.length, 1, 'the network is asked once the re-read has missed');
  network.release();
  const result = await pending;
  assert.equal(await result.response.text(), '[]');
});

test('a list whose session has no copy takes the network, and offline the newest stored list with the same filters answers', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout', 'Date']});
  const h = harness();
  h.state.network = async () => new Response('["one"]', {status: 200, headers: {ETag: '"l1"'}});
  await answerListWithCapWait(t, h, request(LIST_URL));
  t.mock.timers.tick(1000);

  const otherSession = ORIGIN + '/api/sessions/?compact=1&screen=1&active=session-b';
  h.state.network = async () => new Response('["two"]', {status: 200, headers: {ETag: '"l2"'}});
  const online = await answerListWithCapWait(t, h, request(otherSession));
  assert.equal(await online.response.text(), '["two"]', 'online, a session with no copy takes the network answer');
  t.mock.timers.tick(1000);

  h.state.network = async () => { throw new TypeError('offline'); };
  const offline = await answerListWithCapWait(t, h, request(ORIGIN + '/api/sessions/?compact=1&screen=1&active=session-c'));
  assert.equal(await offline.response.text(), '["two"]', 'offline, the newest stored list with the same filters answers');
  assert.ok(offline.response.headers.get(STORED_AT), 'the answer names its stored time, so the page marks it a copy');

  const otherFilters = answer(h, request(ORIGIN + '/api/sessions/?compact=0&screen=1&active=session-c')).catch((error) => error);
  await flush();
  await settleCaps(t, 1);
  assert.equal((await otherFilters).message, 'offline', 'a list with other filters has no stored copy to answer it');
});

test('a full store evicts the session opened longest ago and then stores the new first screen', async () => {
  const h = harness();
  const {copies: store, shells} = await openStores(h);
  await store.put(ORIGIN + '/api/sessions/old/bootstrap', new Response('old'));
  const oldListUrl = ORIGIN + '/api/sessions/?compact=1&screen=1&active=old';
  await store.put(oldListUrl, new Response('old list'));
  await store.put(ORIGIN + '/index', new Response(JSON.stringify({
    sessions: {old: 1, recent: 2}, files: [],
  }), {headers: {'Content-Type': 'application/json'}}));
  h.caches.api.quotaFailures = 1;
  h.state.network = async () => firstScreen('new');

  const result = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await result.response.json()).body, 'new');
  const keys = (await store.keys()).map((entry) => entry.url);
  assert.ok(!keys.includes(ORIGIN + '/api/sessions/old/bootstrap'), 'the longest-unopened session left the store');
  assert.ok(!keys.includes(oldListUrl), 'its session-specific list copy leaves with its first screen');
  assert.ok(keys.includes(BOOTSTRAP_URL), 'the new first screen is stored');
});

test('a release keeps the stored copies and drops the shell of the release before it', async () => {
  const h = harness({version: 'v2'});
  const previousShell = await h.caches.api.open('last-state-shell-v1');
  await previousShell.put(SHELL_KEY, new Response('<html>previous</html>', {headers: {ETag: '"shell-1"'}}));
  const copies = await h.caches.api.open(DATA_CACHE);
  await copies.put(BOOTSTRAP_URL, firstScreen('stored by the previous release'));
  await copies.put(LIST_URL, new Response('{"sessions":[]}'));
  await h.caches.api.open('last-state-files');
  await h.caches.api.open('other-cache');
  await Promise.all(h.fire('activate').waits);
  assert.deepEqual((await h.caches.api.keys()).sort(), [DATA_CACHE, 'last-state-files', 'other-cache']);
  assert.equal(h.state.unregistered, 0);

  h.state.network = async () => new Response('<html>network</html>', {status: 200});
  const navigation = await answer(h, request(SHELL_URL, {mode: 'navigate'}));
  assert.equal(await navigation.response.text(), '<html>network</html>', 'the previous release\'s shell answers no navigation');

  const first = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await first.response.json()).body, 'stored by the previous release', 'the stored copy answers the open');
  assert.equal(await (await copies.match(LIST_URL)).text(), '{"sessions":[]}');
  assert.deepEqual(h.state.calls.map((call) => call.url), [SHELL_URL], 'the copy answers with no request of its own');
});

test('activation drops a copy cache of another format, which no copy of it answers', async () => {
  const h = harness();
  const older = await h.caches.api.open('last-state-data-0');
  await older.put(BOOTSTRAP_URL, firstScreen('a shape the page cannot read'));
  await h.caches.api.open(DATA_CACHE);
  await Promise.all(h.fire('activate').waits);
  assert.deepEqual(await h.caches.api.keys(), [DATA_CACHE]);
});

test('the retired form empties the worker stores, claims the pages and unregisters itself', async () => {
  const h = harness({enabled: false});
  await h.caches.api.open(DATA_CACHE);
  await h.caches.api.open('last-state-files');
  await h.caches.api.open('other-cache');
  await Promise.all(h.fire('activate').waits);
  assert.deepEqual(await h.caches.api.keys(), ['other-cache']);
  assert.equal(h.state.claimed, 1);
  assert.equal(h.state.unregistered, 1);
  const result = await answer(h, request(BOOTSTRAP_URL));
  assert.equal(result.responded, false, 'the retired form answers nothing');
});

test('the enabled form claims no page: the load it arrives in keeps its own requests', async () => {
  const h = harness();
  await Promise.all(h.fire('activate').waits);
  assert.equal(h.state.claimed, 0);
});

test('activation enables navigation preload so the server can answer it without a body', async () => {
  const h = harness();
  await Promise.all(h.fire('activate').waits);
  assert.equal(h.state.preloadEnabled, 1);
});

test('the store-files ask fetches each on-demand file the cache lacks, stores the tagged ones, and reports the count', async () => {
  const h = harness();
  const tagged = ORIGIN + '/static/features/cron/cron.js?v=1';
  const untagged = ORIGIN + '/static/js/chat/status-panel.js?v=1';
  const held = ORIGIN + '/static/css/worker-summary.css?v=1';
  const fetched = [];
  h.state.network = async (req) => {
    fetched.push(req.url);
    if (req.url === tagged) return new Response('cron()', {status: 200, headers: {ETag: '"cron"'}});
    if (req.url === untagged) return new Response('status()', {status: 200});
    throw new Error('unexpected ' + req.url);
  };
  const files = await h.caches.api.open('last-state-files');
  await files.put(held, new Response('held'));
  const channel = messageChannel();
  await Promise.all(h.fire('message', {
    data: {type: 'store-files', files: [tagged, untagged, held]},
    ports: [channel.port2],
  }).waits);
  assert.deepEqual(fetched, [tagged, untagged], 'the held file is not fetched again');
  assert.deepEqual(JSON.parse(JSON.stringify(channel.replies)), [{stored: 1, revalidated: 0}],
      'only the tagged file is stored, and the reply counts it');
  assert.equal(await (await files.match(tagged)).text(), 'cron()');
  assert.equal(await files.match(untagged), undefined, 'a file without an ETag is not stored');
});

test('a kept file whose bytes a release leaves unchanged is revalidated with its stored ETag: no body crosses the link', async () => {
  const h = harness();
  const earlier = ORIGIN + '/static/js/chat/status-panel.js?v=1';
  const current = '/static/js/chat/status-panel.js?v=2';
  const files = await h.caches.api.open('last-state-files');
  await files.put(earlier, new Response('status()', {status: 200, headers: {ETag: '"same"'}}));
  const sent = [];
  h.state.network = async (req, init) => {
    sent.push({url: req.url, ifNoneMatch: init.headers['If-None-Match']});
    if (init.headers['If-None-Match'] === '"same"') return new Response(null, {status: 304});
    throw new Error('unexpected ' + req.url);
  };
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'store-files', files: [current]}, ports: [channel.port2]}).waits);
  assert.deepEqual(JSON.parse(JSON.stringify(sent)), [{url: ORIGIN + current, ifNoneMatch: '"same"'}],
      'the request is a conditional revalidation of the earlier copy');
  assert.deepEqual(JSON.parse(JSON.stringify(channel.replies)), [{stored: 0, revalidated: 1}]);
  assert.equal(await (await files.match(ORIGIN + current)).text(), 'status()', 'the stored body answers the new version');
  assert.equal(await files.match(earlier), undefined, 'the earlier version goes once its replacement is stored');
});

test('a kept file whose bytes changed is fetched whole and stored, and its earlier copy goes', async () => {
  const h = harness();
  const earlier = ORIGIN + '/static/js/chat/status-panel.js?v=1';
  const current = '/static/js/chat/status-panel.js?v=2';
  const files = await h.caches.api.open('last-state-files');
  await files.put(earlier, new Response('status(1)', {status: 200, headers: {ETag: '"old"'}}));
  h.state.network = async (req, init) => {
    if (init.headers['If-None-Match'] === '"old"') return new Response('status(2)', {status: 200, headers: {ETag: '"new"'}});
    throw new Error('unexpected ' + req.url);
  };
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'store-files', files: [current]}, ports: [channel.port2]}).waits);
  assert.deepEqual(JSON.parse(JSON.stringify(channel.replies)), [{stored: 1, revalidated: 0}]);
  assert.equal(await (await files.match(ORIGIN + current)).text(), 'status(2)');
  assert.equal(await files.match(earlier), undefined, 'the earlier version goes once its replacement is stored');
});

test('a warm keeps the earlier copy of a kept file until its replacement is stored', async () => {
  const h = harness();
  const earlier = ORIGIN + '/static/js/chat/status-panel.js?v=1';
  const html = '<link href="/static/css/a.css?v=2" rel="stylesheet">';
  h.state.network = async (req, init) => {
    if (req.url === SHELL_URL) return new Response(html, {status: 200});
    if (!init || init.cache !== 'only-if-cached') return new Response(null, {status: 304});
    if (req.url === ORIGIN + '/static/css/a.css?v=2') return new Response('a', {status: 200, headers: {ETag: '"a"'}});
    throw new Error('unexpected ' + req.url);
  };
  const files = await h.caches.api.open('last-state-files');
  await files.put(earlier, new Response('status(1)', {status: 200, headers: {ETag: '"old"'}}));
  const channel = messageChannel();
  await Promise.all(h.fire('message', {
    data: {type: 'warm', copies: [], files: ['/static/js/chat/status-panel.js?v=2']},
    ports: [channel.port2],
  }).waits);
  assert.equal(channel.replies[0].stored, true);
  assert.equal(await (await files.match(earlier)).text(), 'status(1)', 'the earlier copy is the revalidation base until store-files runs');
});

test('a warm keeps the on-demand files it names and drops other stored files', async () => {
  const h = harness();
  const onDemand = ORIGIN + '/static/features/cron/cron.js?v=2';
  const html = '<link href="/static/css/a.css?v=2" rel="stylesheet">';
  h.state.network = async (req, init) => {
    if (req.url === SHELL_URL) return new Response(html, {status: 200});
    if (!init || init.cache !== 'only-if-cached') return new Response(null, {status: 304});
    if (req.url === ORIGIN + '/static/css/a.css?v=2') return new Response('a', {status: 200, headers: {ETag: '"a"'}});
    throw new Error('unexpected ' + req.url);
  };
  const files = await h.caches.api.open('last-state-files');
  await files.put(onDemand, new Response('cron()'));
  await files.put(ORIGIN + '/static/old.css?v=1', new Response('old'));
  const channel = messageChannel();
  await Promise.all(h.fire('message', {
    data: {type: 'warm', copies: [], files: ['/static/features/cron/cron.js?v=2']},
    ports: [channel.port2],
  }).waits);
  assert.equal(channel.replies[0].stored, true);
  const stored = (await files.keys()).map((entry) => entry.url).sort();
  assert.deepEqual(stored, [ORIGIN + '/static/css/a.css?v=2', onDemand].sort(),
      'the on-demand file survives the warm; the file of an older version is dropped');
});

// The quiet gap as offline-shell-worker.js names it.
const PAGE_QUIET_MS = 1000;

// A shell fetch the test answers by hand. It rejects when the worker aborts it, as a fetch does.
function heldShellFetches(h) {
  const fetches = [];
  h.state.network = (req, init) => {
    if (req.url !== SHELL_URL) throw new Error('unexpected ' + req.url);
    const attempt = deferred();
    fetches.push({signal: init.signal, finish: attempt.release});
    return new Promise((resolve, reject) => {
      init.signal.addEventListener('abort', () => reject(new TypeError('This request was aborted')));
      attempt.promise.then(() => resolve(new Response('<html>shell</html>', {status: 200, headers: {ETag: '"s"'}})));
    });
  };
  return fetches;
}

test('a page request aborts the shell fetch, and the shell runs again once the page has been quiet', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  const fetches = heldShellFetches(h);
  const channel = messageChannel();
  const warming = Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
  await flush();
  assert.equal(fetches.length, 1, 'the shell fetch starts on an idle page');

  h.fire('message', {data: {type: 'page-request-start', id: 'p1'}});
  assert.equal(fetches[0].signal.aborted, true, 'a page request start aborts the shell fetch in flight');
  await flush();
  h.fire('message', {data: {type: 'page-request-end', id: 'p1'}});
  t.mock.timers.tick(PAGE_QUIET_MS - 1);
  await flush();
  assert.equal(fetches.length, 1, 'the shell waits out the quiet gap');
  t.mock.timers.tick(1);
  await flush();
  assert.equal(fetches.length, 2, 'the shell runs again once the page has been quiet');
  fetches[1].finish();
  await warming;
  assert.equal(channel.replies[0].stored, true);
  assert.equal(channel.replies[0].fetchHolds.count, 1, 'the reply counts the one wait the page request imposed');
});

test('a user request in flight past ten seconds holds the shell install until it ends and the page has been quiet',
    async (t) => {
      t.mock.timers.enable({apis: ['setTimeout']});
      const h = harness();
      const fetches = heldShellFetches(h);
      const channel = messageChannel();
      const warming = Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
      await flush();
      assert.equal(fetches.length, 1, 'the shell fetch starts on an idle page');

      h.fire('message', {data: {type: 'page-request-start', id: 'slow-1'}});
      assert.equal(fetches[0].signal.aborted, true, 'a page request start aborts the shell fetch in flight');
      await flush();
      t.mock.timers.tick(15000);
      await flush();
      assert.equal(fetches.length, 1, 'the install stays held past fifteen seconds while the request is in flight');

      h.fire('message', {data: {type: 'page-request-end', id: 'slow-1'}});
      t.mock.timers.tick(PAGE_QUIET_MS - 1);
      await flush();
      assert.equal(fetches.length, 1, 'the install waits out the quiet gap');
      t.mock.timers.tick(1);
      await flush();
      assert.equal(fetches.length, 2, 'the install runs again once the page has been quiet');
      fetches[1].finish();
      await warming;
      assert.equal(channel.replies[0].stored, true);
      assert.equal(channel.replies[0].fetchHolds.count, 1, 'the one wait is counted');
    });

// A reload past the stored shell carries the reload parameter; the worker leaves that navigation to the network, so
// the stored shell never answers it, while a plain navigation still takes the stored shell.
// A page that found the server on a newer release marks this worker's shell stale: online navigations then go to the
// network, so a tab's next navigation cannot take the stale shell again. Offline, the stored shell still answers.
test('after a stale-shell mark a navigation goes to the network; offline the stored shell answers', async () => {
  const h = harness();
  h.state.network = async () => new Response('<html>shell</html>', {status: 200, headers: {ETag: '"s"'}});
  const warm = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [warm.port2]}).waits);
  assert.equal(warm.replies[0].stored, true, 'a shell is stored');
  assert.equal(warm.replies[0].version, 'v1', 'the warm reply names the release the worker serves');

  const mark = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'stale-shell'}, ports: [mark.port2]}).waits);
  assert.equal(mark.replies.length, 1, 'the mark is acknowledged');

  h.state.network = async () => new Response('<html>release</html>', {status: 200});
  const online = await answer(h, request(SHELL_URL, {mode: 'navigate'}));
  assert.equal(await online.response.text(), '<html>release</html>', 'the navigation takes the network page');

  h.state.network = async () => { throw new TypeError('network unreachable'); };
  const offline = await answer(h, request(SHELL_URL, {mode: 'navigate'}));
  assert.equal(await offline.response.text(), '<html>shell</html>', 'offline, the stored shell answers');
});

// A blackout holds the stale navigation's network attempt without a fast error: past the deadline the attempt is
// aborted and the stored shell answers, so the offline view still opens.
test('a stale navigation whose network attempt hangs past the deadline is answered from the stored shell', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async () => new Response('<html>shell</html>', {status: 200, headers: {ETag: '"s"'}});
  const warm = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [warm.port2]}).waits);
  const mark = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'stale-shell'}, ports: [mark.port2]}).waits);

  let aborted = false;
  h.state.network = (req, init) => new Promise((resolve, reject) => {
    init.signal.addEventListener('abort', () => {
      aborted = true;
      const error = new Error('the attempt was aborted');
      error.name = 'AbortError';
      reject(error);
    });
  });
  const pending = answer(h, request(SHELL_URL, {mode: 'navigate'}));
  await flush();
  assert.equal(aborted, false, 'the attempt waits for the network until the deadline');
  t.mock.timers.tick(STALE_NETWORK_CAP_MS);
  await flush();
  const served = await pending;
  assert.equal(aborted, true, 'the deadline aborts the attempt');
  assert.equal(await served.response.text(), '<html>shell</html>', 'past the deadline the stored shell answers');
});

// The release's activation drops the stale flag an older release's worker set, so the flag never outlives that worker.
test('activating a release drops the stale flag an older release set', async () => {
  const cacheStorage = makeCaches();
  const older = harness({version: 'v1', cacheStorage});
  await Promise.all(older.fire('message', {data: {type: 'stale-shell'}, ports: [messageChannel().port2]}).waits);
  const stateKeys = () => [...(cacheStorage.stores.get('offline-shell-state')?.keys() ?? [])];
  assert.deepEqual(stateKeys(), [SHELL_FLAG_URL], 'the older release set its flag');

  const newer = harness({version: 'v2', cacheStorage});
  await Promise.all(newer.fire('activate', {}).waits);
  assert.deepEqual(stateKeys(), [], 'the newer release\'s activation drops the flag');
});

test('a navigation that carries the reload parameter goes to the network, though a shell is stored', async () => {
  const h = harness();
  h.state.network = async () => new Response('<html>shell</html>', {status: 200, headers: {ETag: '"s"'}});
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm', copies: []}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies[0].stored, true, 'a shell is stored');

  const plain = await answerNow(h, request(SHELL_URL, {mode: 'navigate'}));
  assert.equal(plain.responded, true, 'a plain navigation is answered from the stored shell');
  const reload = await answerNow(h, request(ORIGIN + '/?session=' + SESSION + '&reloading=1', {mode: 'navigate'}));
  assert.equal(reload.responded, false, 'a reload past the stored shell is left to the network');
});
