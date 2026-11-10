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
const BOOTSTRAP_URL = ORIGIN + '/api/sessions/' + SESSION + '/bootstrap';
const LIST_URL = ORIGIN + '/api/sessions/?compact=1';

const WAIT_CAP_MS = 250;

// A network answer the test releases by hand, so no test waits on a real timer.
function deferred() {
  let release;
  const promise = new Promise((resolve) => { release = resolve; });
  return {promise, release};
}

// Lets the worker's pending steps run up to their next timer, the fake clock's cue to advance.
const flush = () => new Promise((resolve) => setImmediate(resolve));

function makeCaches() {
  const stores = new Map();
  const keyOf = (input) => new URL(typeof input === 'string' ? input : input.url, ORIGIN).href;
  const api = {
    open: async (name) => {
      if (!stores.has(name)) stores.set(name, new Map());
      const entries = stores.get(name);
      return {
        match: async (input) => (entries.has(keyOf(input)) ? entries.get(keyOf(input)).clone() : undefined),
        put: async (input, response) => {
          if (api.quotaFailures > 0 && name.startsWith('last-state-v')) {
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
function harness({version = 'v1', enabled = true} = {}) {
  const listeners = {};
  const cachesFake = makeCaches();
  const state = {claimed: 0, unregistered: 0, network: null, calls: []};
  const self = {
    location: {origin: ORIGIN},
    addEventListener(type, handler) {
      (listeners[type] ||= []).push(handler);
    },
    skipWaiting: async () => {},
    clients: {claim: async () => { state.claimed += 1; }},
    registration: {unregister: async () => { state.unregistered += 1; }},
  };
  const fetch = async (input, init) => {
    const request = typeof input === 'string' ? {url: new URL(input, ORIGIN).href, method: 'GET', headers: new Headers()} : input;
    state.calls.push({url: request.url, init});
    if (!state.network) throw new TypeError('network unreachable');
    return state.network(request, init);
  };
  const context = vm.createContext({
    self, fetch, caches: cachesFake.api, URL, Response, Headers, Error, Promise, Date, Set, Map, JSON, Object, Array,
    String, Number, RegExp, console, setTimeout, clearTimeout,
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

test('the first load reaches the network, and a warm shell then answers the navigation', async () => {
  const h = harness();
  h.state.network = async (req) => (req.url === SHELL_URL
    ? new Response('<html><body>shell</body></html>', {status: 200, headers: {ETag: '"shell"'}})
    : new Response('unexpected', {status: 500}));
  const nav = request(SHELL_URL, {mode: 'navigate'});
  const first = await answer(h, nav);
  assert.equal(await first.response.text(), '<html><body>shell</body></html>', 'the first load is the network page');

  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm'}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 1, 'the warm answers once');
  assert.equal(channel.replies[0].stored, true, 'the warm reports the stored shell over its channel');
  const second = await answer(h, nav);
  assert.equal(await second.response.text(), '<html><body>shell</body></html>');
  assert.equal(h.state.calls.filter((call) => call.url === SHELL_URL).length, 2,
      'the first load and the warm fetch reached the network; the stored shell answers the second navigation');
});

test('a warm that finds the shell stored fetches nothing and reports the shell it has', async () => {
  const h = harness();
  h.state.network = async (req) => (req.url === SHELL_URL
    ? new Response('<html><body>shell</body></html>', {status: 200, headers: {ETag: '"shell"'}})
    : new Response('unexpected', {status: 500}));
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm'}, ports: [channel.port2]}).waits);
  const again = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm'}, ports: [again.port2]}).waits);
  assert.equal(again.replies.length, 1);
  assert.equal(again.replies[0].stored, true);
  assert.equal(h.state.calls.filter((call) => call.url === SHELL_URL).length, 1,
      'the second warm of the same release fetches no shell');
});

test('a warm whose shell fetch fails reports the failure', async () => {
  const h = harness();
  h.state.network = async () => new Response('no', {status: 503});
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm'}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 1);
  assert.equal(channel.replies[0].stored, false);
  assert.match(channel.replies[0].error, /503/);
});

test('the retired form answers no message', async () => {
  const h = harness({enabled: false});
  h.state.network = async () => new Response('unexpected', {status: 200});
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm'}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 0, 'a worker the switch retired stores nothing');
  assert.deepEqual(h.state.calls, [], 'a worker the switch retired fetches nothing');
});

test('warming stores the shell and the same-origin files it names, and drops the files it no longer names', async () => {
  const h = harness();
  const html = '<link href="/static/css/a.css?v=1" rel="stylesheet"><script src="/static/js/b.js"></script>' +
      '<script defer src="https://cdn.example/x.js"></script>';
  h.state.network = async (req) => {
    if (req.url === SHELL_URL) return new Response(html, {status: 200});
    if (req.url === ORIGIN + '/static/css/a.css?v=1') return new Response('a', {status: 200, headers: {ETag: '"a"'}});
    if (req.url === ORIGIN + '/static/js/b.js') return new Response('b', {status: 200});
    throw new Error('unexpected ' + req.url);
  };
  const files = h.caches.api;
  await (await files.open('last-state-files')).put(ORIGIN + '/static/old.css', new Response('old'));
  const channel = messageChannel();
  await Promise.all(h.fire('message', {data: {type: 'warm'}, ports: [channel.port2]}).waits);
  assert.equal(channel.replies.length, 1);
  assert.equal(channel.replies[0].stored, true);

  const stored = await (await files.open('last-state-files')).keys();
  assert.deepEqual(stored.map((entry) => entry.url), [ORIGIN + '/static/css/a.css?v=1'],
      'the file with an ETag is stored; the file without one is not, and the old file is dropped');
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

test('a slow network gives the stored copy at the wait cap, and its later answer replaces the copy', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));

  const network = deferred();
  h.state.network = () => network.promise.then(() => firstScreen('two'));
  let answered = false;
  const slow = answerNow(h, request(BOOTSTRAP_URL)).then((result) => {
    answered = true;
    return result;
  });
  await flush();
  t.mock.timers.tick(WAIT_CAP_MS - 1);
  await flush();
  assert.equal(answered, false, 'no answer before the wait cap');
  t.mock.timers.tick(1);
  const copy = await slow;
  assert.equal((await copy.response.json()).body, 'one');
  assert.ok(copy.response.headers.get(STORED_AT), 'a copy answer carries the time it was stored');

  network.release();
  await Promise.all(copy.waits);
  const later = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await later.response.json()).body, 'two', 'the copy the network answered later serves the next open');
});

test('a fast network answer reaches the page as it is', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));
  h.state.network = async () => firstScreen('two');
  const result = await answer(h, request(BOOTSTRAP_URL));
  assert.equal(result.response.headers.get(STORED_AT), null);
  assert.equal((await result.response.json()).body, 'two');
});

test('a network error or a 5xx answers from the stored copy', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));

  h.state.network = async () => { throw new TypeError('offline'); };
  const offline = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await offline.response.json()).body, 'one');

  h.state.network = async () => new Response('busy', {status: 503});
  const busy = await answer(h, request(LIST_URL));
  assert.equal(busy.response.headers.get(STORED_AT), null, 'the list has no copy yet: the 5xx reaches the page');
  assert.equal(busy.response.status, 503);
});

test('a 401 drops every stored copy and reaches the page, and an offline page with no copy fails the request', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));

  h.state.network = async () => new Response('no', {status: 401});
  const denied = await answer(h, request(BOOTSTRAP_URL));
  assert.equal(denied.response.status, 401);

  h.state.network = async () => { throw new TypeError('offline'); };
  await assert.rejects(answer(h, request(BOOTSTRAP_URL)), TypeError, 'the copy is gone, so the page sees the failure');
});

test('a revalidating 304 gives the stored copy: it names no body the page could read', async () => {
  const h = harness();
  h.state.network = async () => firstScreen('one');
  await answer(h, request(BOOTSTRAP_URL));

  // The browser's HTTP cache revalidates against the worker's own subrequest, and the 304 lands inside
  // the wait cap. The copy carries the state that etag names.
  h.state.network = async () => new Response(null, {status: 304, headers: {ETag: '"s1"'}});
  const revalidated = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await revalidated.response.json()).body, 'one', 'the copy answers the bodyless 304');
  assert.notEqual(revalidated.response.headers.get(STORED_AT), null, 'the answer is the stored copy');
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

test('a copy-less list answers from the network without the cap', async (t) => {
  t.mock.timers.enable({apis: ['setTimeout']});
  const h = harness();
  const network = deferred();
  h.state.network = () => network.promise.then(() => new Response('[]', {status: 200, headers: {ETag: '"l"'}}));
  const pending = answer(h, request(LIST_URL));
  await flush();
  t.mock.timers.tick(10 * WAIT_CAP_MS);
  network.release();
  const result = await pending;
  assert.equal(await result.response.text(), '[]');
});

test('a full store evicts the session opened longest ago and then stores the new first screen', async () => {
  const h = harness();
  const store = await h.caches.api.open('last-state-v1');
  await store.put(ORIGIN + '/api/sessions/old/bootstrap', new Response('old'));
  await store.put(ORIGIN + '/index', new Response(JSON.stringify({
    sessions: {old: 1, recent: 2}, files: [],
  }), {headers: {'Content-Type': 'application/json'}}));
  h.caches.api.quotaFailures = 1;
  h.state.network = async () => firstScreen('new');

  const result = await answer(h, request(BOOTSTRAP_URL));
  assert.equal((await result.response.json()).body, 'new');
  const keys = (await store.keys()).map((entry) => entry.url);
  assert.ok(!keys.includes(ORIGIN + '/api/sessions/old/bootstrap'), 'the longest-unopened session left the store');
  assert.ok(keys.includes(BOOTSTRAP_URL), 'the new first screen is stored');
});

test('a drop-shell message removes the stored shell and replies on its port', async () => {
  const h = harness();
  const store = await h.caches.api.open('last-state-v1');
  await store.put(SHELL_KEY, new Response('shell'));
  let replied = false;
  const port = {postMessage: () => { replied = true; }};
  await Promise.all(h.fire('message', {data: {type: 'drop-shell'}, ports: [port]}).waits);
  assert.equal(replied, true);
  assert.equal(await store.match(SHELL_KEY), undefined);
});

test('a release drops the stores of older releases and keeps its own and the files store', async () => {
  const h = harness({version: 'v2'});
  await h.caches.api.open('last-state-v1');
  await h.caches.api.open('last-state-v2');
  await h.caches.api.open('last-state-files');
  await h.caches.api.open('other-cache');
  await Promise.all(h.fire('activate').waits);
  assert.deepEqual((await h.caches.api.keys()).sort(),
      ['last-state-files', 'last-state-v2', 'other-cache']);
  assert.equal(h.state.unregistered, 0);
});

test('the retired form empties the worker stores, claims the pages and unregisters itself', async () => {
  const h = harness({enabled: false});
  await h.caches.api.open('last-state-v1');
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
