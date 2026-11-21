// The page side of the offline shell's warm: the registration and the worker's warm message wait for the
// page's catch-up to complete (charliebot:page-current), whose report reaches the page as
// charliebot:offline-shell-warm; a browser without a service worker reports that instead, so the
// poor-network bench's warm wait closes on every client. The same event tells the worker the catch-up is
// done, which releases the copies' refresh.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const SOURCE = fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'offline-shell.js'), 'utf8');
const WORKER_PATH = '/offline-shell-worker.js';
const PAGE_CURRENT = 'charliebot:page-current';

// One page: the events it dispatched, the registration's worker, the messages the active worker got, and
// the document listeners the script installed. `registration` is the registration the page gets (an installing or
// waiting worker in it is an update in progress); `noRegistration` leaves the page with none.
function buildPage({
  serviceWorker = true,
  registration = null,
  noRegistration = false,
  sessionId = null,
  sessionBootstrap = null,
  initialSessions = null,
  initialLoadErrors = [],
  pageFetch = undefined,
} = {}) {
  const events = [];
  const warnings = [];
  const documentListeners = new Map();
  const windowListeners = new Map();
  const context = {
    console: {error: () => {}, warn: (...args) => warnings.push(args)},
    location: {
      href: 'https://charlie.test/?session=',
      origin: 'https://charlie.test',
      reload: () => { context.reloads += 1; },
      replace: (url) => { context.replaced.push(url); },
    },
    history: {
      state: null,
      replaceState: (state, title, url) => {
        context.location.href = new URL(url, context.location.href).href;
      },
    },
    replaced: [],
    reloads: 0,
    performance: {timeOrigin: 1000, now: () => 0},
    URL,
    // The idle callbacks wait in a list the test runs by hand; the page's versioned URLs are the utils.js form.
    idleCallbacks: [],
    requestIdleCallback: (fn) => { context.idleCallbacks.push(fn); },
    pageAssetUrl: (assetPath) => assetPath + '?v=test',
    SESSION_ID: sessionId,
    SESSION_BOOTSTRAP: sessionBootstrap,
    INITIAL_SESSIONS: initialSessions,
    INITIAL_LOAD_ERRORS: initialLoadErrors,
    CustomEvent: class {
      constructor(type, init) {
        this.type = type;
        this.detail = init.detail;
      }
    },
    MessageChannel: class {
      constructor() {
        this.port1 = {onmessage: null};
        this.port2 = {};
        context.channels.push(this);
      }
    },
    document: {
      dispatchEvent: (event) => events.push(event),
      addEventListener: (type, listener, options) => {
        if (!documentListeners.has(type)) documentListeners.set(type, []);
        const wrapped = options?.once
          ? function once(event) {
            documentListeners.get(type).splice(documentListeners.get(type).indexOf(once), 1);
            listener(event);
          }
          : listener;
        documentListeners.get(type).push(wrapped);
      },
    },
    channels: [],
    // The reply function of each message type's channel: the warm and the on-demand ask each open their own.
    replyTo: {},
    workerMessages: [],
    // The stored-copy refresh's debounce: the test runs the pending timer by hand.
    timers: [],
    setTimeout: (fn, ms) => {
      context.timers.push({fn, ms});
      return context.timers.length;
    },
    clearTimeout: (id) => { if (context.timers[id - 1]) context.timers[id - 1].cleared = true; },
  };
  if (!serviceWorker) context.navigator = {};
  context.window = context;
  context.fetch = pageFetch;
  context.addEventListener = (type, listener) => {
    if (!windowListeners.has(type)) windowListeners.set(type, []);
    windowListeners.get(type).push(listener);
  };
  if (serviceWorker) {
    const active = {
      postMessage: (message, transfer) => {
        context.workerMessages.push(message);
        context.postMessage = {message, transfer};
        const channel = context.channels[context.channels.length - 1];
        if (channel) {
          context.reply = (data) => channel.port1.onmessage({data});
          context.replyTo[message.type] = context.reply;
        }
      },
    };
    const registrationFor = noRegistration ? undefined : (registration ?? {active});
    context.navigator = {
      serviceWorker: {
        registered: [],
        register: (path_, options) => {
          context.navigator.serviceWorker.registered.push({path: path_, options});
          return Promise.resolve(registrationFor);
        },
        ready: Promise.resolve(registrationFor),
        getRegistration: async () => registrationFor,
      },
    };
  }
  vm.createContext(context);
  vm.runInContext(SOURCE, context);
  context.registerOfflineShell();
  return {
    events,
    warnings,
    context,
    registered: () => context.navigator?.serviceWorker.registered ?? [],
    firePageCurrent: () => {
      // A copy of the list: a once-only listener removes itself from the list as it runs.
      for (const listener of [...(documentListeners.get(PAGE_CURRENT) || [])]) listener();
    },
    firePageHide: () => {
      for (const listener of windowListeners.get('pagehide') || []) listener();
    },
    fireWindowLoad: () => {
      for (const listener of windowListeners.get('load') || []) listener();
    },
  };
}

test('the warm waits for the page-current event, then asks the active worker and reports its reply', async () => {
  const page = buildPage();
  page.firePageCurrent();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(page.registered().length, 1);
  assert.equal(page.registered()[0].path, WORKER_PATH);
  assert.equal(page.registered()[0].options.scope, '/?session=');
  assert.equal(page.context.postMessage.message.type, 'warm');
  assert.deepEqual(JSON.parse(JSON.stringify(page.context.postMessage.message.copies)), []);
  assert.equal(page.context.postMessage.transfer.length, 1, 'the reply channel travels with the message');
  page.context.reply({stored: true});
  assert.equal(page.events.length, 1);
  assert.equal(page.events[0].type, 'charliebot:offline-shell-warm');
  assert.equal(page.events[0].detail.stored, true);
});

test('the warm seeds the server-rendered first screen and sidebar rows', async () => {
  const bootstrap = {event_count: 4, session: {id: 'session-a'}};
  const initialSessions = {$encoded: {ids: []}, $screen: {partial: false}};
  const page = buildPage({
    sessionId: 'session-a',
    sessionBootstrap: bootstrap,
    initialSessions,
  });
  // sidebar/filters.js registers this at load; the page test plays that registration.
  page.context.registerOfflineShellListingUrl((activeSessionId) => '/api/sessions/?compact=1&screen=1' +
    (activeSessionId ? '&active=' + encodeURIComponent(activeSessionId) : ''));
  page.firePageCurrent();
  await new Promise((resolve) => setImmediate(resolve));

  assert.deepEqual(JSON.parse(JSON.stringify(page.context.postMessage.message.copies)), [
    {
      url: 'https://charlie.test/api/sessions/session-a/bootstrap',
      body: bootstrap,
    },
    {
      url: 'https://charlie.test/api/sessions/?compact=1&screen=1&active=session-a',
      body: initialSessions,
    },
  ]);
});

test('an initial-list error does not seed an empty sidebar copy', async () => {
  const page = buildPage({
    sessionId: 'session-a',
    sessionBootstrap: {event_count: 4, session: {id: 'session-a'}},
    initialSessions: [],
    initialLoadErrors: ['listing failed'],
  });
  page.firePageCurrent();
  await new Promise((resolve) => setImmediate(resolve));

  const copies = JSON.parse(JSON.stringify(page.context.postMessage.message.copies));
  assert.equal(copies.length, 1);
  assert.equal(copies[0].url, 'https://charlie.test/api/sessions/session-a/bootstrap');
});

test('the registration fetches no worker script before the page is current', () => {
  const page = buildPage();
  assert.equal(page.registered().length, 0, 'the open still owns the link');
  assert.equal(page.events.length, 0);
});

test('a browser without service workers reports that the warm cannot run', () => {
  const page = buildPage({serviceWorker: false});
  assert.equal(page.events.length, 1);
  assert.equal(page.events[0].detail.supported, false);
});

test('a page with no registration reports the warm error', async () => {
  const page = buildPage({noRegistration: true});
  page.firePageCurrent();
  await settled();
  assert.equal(page.events.length, 1);
  assert.equal(typeof page.events[0].detail.error, 'string');
});

// A release's first page: the worker the registration still holds is the release before it, and the new worker
// installs. The warm goes to the worker that takes over, not to the one it replaces.
test('a warm waits for the update the registration is installing, and goes to the worker that takes over', async () => {
  const sent = {replaced: [], next: []};
  const replaced = {postMessage: (message) => sent.replaced.push(message)};
  const next = {postMessage: (message) => sent.next.push(message)};
  let onStateChange = null;
  const installing = {state: 'installing', addEventListener: (type, listener) => { onStateChange = listener; }};
  const registration = {active: replaced, installing};
  const page = buildPage({registration});
  page.firePageCurrent();
  await settled();
  assert.deepEqual(sent.replaced.filter((message) => message.type === 'warm'), [],
      'the worker the update replaces is not asked for the warm');

  registration.active = next;
  registration.installing = null;
  installing.state = 'activated';
  onStateChange();
  await settled();
  assert.deepEqual(sent.next.filter((message) => message.type === 'warm').map((message) => message.type), ['warm'],
      'the warm goes to the worker that took over');
});

// The cap bounds a stuck update: past PAGE_UPDATE_CAP_MS the warm reports an error, and the warm never waits longer.
test('an update that never takes over fails the warm at the cap', async () => {
  const PAGE_UPDATE_CAP_MS = 20000;
  const stuck = () => ({
    active: {postMessage: () => {}},
    installing: {state: 'installing', addEventListener: () => {}},
  });

  const warmPage = buildPage({registration: stuck()});
  warmPage.firePageCurrent();
  await settled();
  const warmCap = warmPage.context.timers.filter((timer) => timer.ms === PAGE_UPDATE_CAP_MS && !timer.cleared);
  assert.equal(warmCap.length, 1, 'the warm waits under one cap timer');
  warmCap[0].fn();
  await settled();
  assert.equal(warmPage.events.length, 1);
  assert.ok(warmPage.events[0].detail.error, 'the warm reports the stuck update as an error');
  assert.match(warmPage.events[0].detail.error, /did not take over within 20000 ms/);
});

// A 4409 close reloads past the stored shell: one navigation to the same address with the parameter the worker
// sends to the network (offline-shell-worker.js).
test('reloading past the stored shell navigates once, to the address with the parameter', () => {
  const page = buildPage();
  page.context.location.href = 'https://charlie.test/?session=session-a';
  page.context.reloadPastStoredShell();
  assert.deepEqual(page.context.replaced, ['https://charlie.test/?session=session-a&reloading=1']);
  assert.equal(page.context.reloads, 0, 'the reload is a navigation, not a reload of the stored shell');
});

// The parameter leaves the address bar once the page has loaded, so the next reload answers from the stored shell.
test('the reload parameter leaves the address bar once the page has loaded', () => {
  const page = buildPage();
  page.context.location.href = 'https://charlie.test/?session=session-a&reloading=1';
  page.fireWindowLoad();
  assert.equal(page.context.location.href, 'https://charlie.test/?session=session-a');
});

// Chromium can attach a worker's statechange after the worker has reached its state: then no event reaches the
// listener. The page reads the state once the listener is attached, so a worker that already activated takes over
// at once, with no wait for the cap. The warm's report names the state the page saw.
test('an update that activated before the page attached its listener takes over at once', async () => {
  const PAGE_UPDATE_CAP_MS = 20000;
  const sent = [];
  const active = {postMessage: (message, transfer) => sent.push({message, transfer})};
  const installing = {state: 'activated', addEventListener: () => {}};
  const page = buildPage({registration: {active, installing}});
  page.firePageCurrent();
  await settled();
  const warms = sent.filter((entry) => entry.message.type === 'warm');
  assert.equal(warms.length, 1, 'the warm goes out without a state change');
  assert.equal(page.context.timers.filter((timer) => timer.ms === PAGE_UPDATE_CAP_MS && !timer.cleared).length, 0,
      'the cap timer is cleared');

  const channel = page.context.channels.find((candidate) => candidate.port2 === warms[0].transfer[0]);
  channel.port1.onmessage({data: {stored: true}});
  assert.deepEqual(JSON.parse(JSON.stringify(page.events[0].detail.update)), {activated: 0});
});

test('the page-current event tells the active worker the catch-up is complete', async () => {
  const page = buildPage();
  page.firePageCurrent();
  await new Promise((resolve) => setImmediate(resolve));
  // The first open's own report precedes the warm request; its refresh queue is empty, so the worker
  // refreshes nothing. The messages are compared as JSON: the vm context's objects carry another prototype.
  assert.deepEqual(JSON.parse(JSON.stringify(page.context.workerMessages)),
      [{type: 'catchup-complete'}, {type: 'warm', copies: [], files: onDemandPaths().map((listed) => listed + '?v=test')}]);
  page.context.workerMessages.length = 0;
  page.firePageCurrent();
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(JSON.parse(JSON.stringify(page.context.workerMessages)), [{type: 'catchup-complete'}],
      'the once-only warm does not repeat; the catch-up report does');
});

test('a reply the socket delivered asks the worker to refresh the session\'s stored copy once the burst settles', async () => {
  const page = buildPage();
  // The committed-reply handler asks once per frame of the reply's burst; the debounce holds the asks
  // until the gap, then one message covers them.
  page.context.refreshStoredSessionCopies('session-a');
  page.context.refreshStoredSessionCopies('session-a');
  assert.equal(page.context.workerMessages.length, 0, 'the ask waits for the quiet gap');
  assert.equal(page.context.timers.filter((timer) => !timer.cleared).length, 1, 'the burst shares one timer');

  const pending = page.context.timers[page.context.timers.length - 1];
  pending.fn();
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(JSON.parse(JSON.stringify(page.context.workerMessages)), [{
    type: 'refresh-copies',
    urls: ['https://charlie.test/api/sessions/session-a/bootstrap'],
  }], 'the ask names the session\'s stored first screen');
  assert.equal(pending.cleared, undefined, 'the settled ask leaves its timer run');

  page.context.reply({refreshed: 1});
  await new Promise((resolve) => setImmediate(resolve));
  const reported = page.events.filter((event) => event.type === 'charliebot:offline-shell-copies-refreshed');
  assert.equal(reported.length, 1, 'the refresh reports its end once');
  assert.equal(reported[0].detail.refreshed, 1, 'the report carries the count the worker re-stored');
});

test('a reply burst that spans two sessions refreshes both stored copies in one ask', async () => {
  const page = buildPage();
  page.context.refreshStoredSessionCopies('session-a');
  page.context.refreshStoredSessionCopies('session-b');
  page.context.timers[page.context.timers.length - 1].fn();
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(JSON.parse(JSON.stringify(page.context.workerMessages)), [{
    type: 'refresh-copies',
    urls: [
      'https://charlie.test/api/sessions/session-a/bootstrap',
      'https://charlie.test/api/sessions/session-b/bootstrap',
    ],
  }]);
});

test('the on-demand files are asked of the active worker once the page is current and idle, under the page\'s URLs', async () => {
  const page = buildPage();
  page.firePageCurrent();
  assert.equal(page.context.idleCallbacks.length, 1, 'the request waits for an idle callback, not the page-current event');
  page.context.idleCallbacks.splice(0).forEach((fn) => fn());
  await new Promise((resolve) => setImmediate(resolve));
  const asked = page.context.workerMessages.filter((message) => message.type === 'store-files');
  assert.equal(asked.length, 1, 'one ask per page');
  assert.equal(asked[0].files.length, onDemandPaths().length);
  assert.ok(asked[0].files.every((url) => url.endsWith('?v=test')), 'each file is named by the URL the page requests');

  // The worker's reply reaches the page as the event the bench and the screenshots wait for.
  page.context.replyTo['store-files']({stored: 3});
  const reported = page.events.filter((event) => event.type === 'charliebot:offline-shell-files-stored');
  assert.deepEqual(reported.map((event) => event.detail), [{stored: 3}]);
});

test('the warm names the on-demand files, so a later warm keeps them', async () => {
  const page = buildPage();
  page.firePageCurrent();
  await new Promise((resolve) => setImmediate(resolve));
  const warm = page.context.workerMessages.find((message) => message.type === 'warm');
  assert.deepEqual(JSON.parse(JSON.stringify(warm.files)), onDemandPaths().map((listed) => listed + '?v=test'));
});

test('every same-origin file a page loader names is either kept or excluded with a reason', () => {
  const kept = onDemandPaths();
  const excluded = onDemandExcluded();
  assert.ok(kept.length > 0 && excluded.size > 0, 'the lists the test reads are not empty');
  const named = new Set();
  for (const file of pageSourceFiles()) {
    const text = fs.readFileSync(file, 'utf8');
    if (!/\bload(PageScripts|PageStyles)\(/.test(text)) continue;
    for (const match of text.matchAll(/['"](\/static\/[^'"]+)['"]/g)) named.add(match[1]);
  }
  assert.deepEqual([...named].filter((file) => !kept.includes(file) && !excluded.has(file)), [],
      'a loader names a same-origin file that neither list names: keep it or exclude it with its reason');
  assert.deepEqual(kept.filter((file) => !named.has(file)), [], 'a kept file that no loader names is stale');
  assert.deepEqual([...excluded.keys()].filter((file) => !named.has(file)), [],
      'an excluded file that no loader names is stale');
  for (const [file, reason] of excluded) assert.ok(reason.length > 0, file + ' has no exclusion reason');
});

// The paths in offline-shell.js's ON_DEMAND_FILES, unversioned.
function onDemandPaths() {
  const block = SOURCE.match(/const ON_DEMAND_FILES = Object\.freeze\(\[([\s\S]*?)\]\);/);
  return [...block[1].matchAll(/'([^']+)'/g)].map((match) => match[1]);
}

// The excluded paths and their reasons, from ON_DEMAND_EXCLUDED.
function onDemandExcluded() {
  const block = SOURCE.match(/const ON_DEMAND_EXCLUDED = Object\.freeze\(\{([\s\S]*?)\}\);/);
  return new Map([...block[1].matchAll(/'([^']+)': '([^']+)'/g)].map((match) => [match[1], match[2]]));
}

// The page's own source files: the templates and feature trees a session page includes, and the static scripts. Vendor
// libraries and the tests stay out.
function pageSourceFiles() {
  const files = [];
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, {withFileTypes: true})) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        if (entry.name !== 'vendor' && entry.name !== 'tests') walk(full);
      } else if (/\.(js|html)$/.test(entry.name)) {
        files.push(full);
      }
    }
  };
  for (const root of ['templates', 'features', path.join('static', 'js')]) walk(path.join(__dirname, '..', root));
  return files;
}

// Lets the reports queued on the registration's ready promise reach the active worker.
const settled = () => new Promise((resolve) => setImmediate(resolve));

test('a user-waiting request reports its start and its end, unchanged; other requests report nothing', async () => {
  const seen = [];
  const pageFetch = (input, init) => {
    seen.push({input, init});
    return Promise.resolve({ok: true});
  };
  const page = buildPage({pageFetch});
  const bootstrapInit = {headers: {'X-Probe': 'kept'}};
  await page.context.fetch('/api/sessions/s1/bootstrap', bootstrapInit);
  await page.context.fetch('/api/sessions/status');
  await page.context.fetch('/api/chat/s1/message', {method: 'POST', body: 'hi'});
  await settled();

  assert.deepEqual(seen.map((call) => call.input), [
    '/api/sessions/s1/bootstrap', '/api/sessions/status', '/api/chat/s1/message',
  ], 'every request goes out');
  assert.equal(seen[0].init, bootstrapInit, 'the request is the page\'s own, with its own init');
  const [start, end, sendStart, sendEnd] = page.context.workerMessages;
  assert.deepEqual([start.type, end.type, sendStart.type, sendEnd.type],
      ['page-request-start', 'page-request-end', 'page-request-start', 'page-request-end']);
  assert.equal(start.id, end.id, 'the end names the request its start named');
  assert.notEqual(sendStart.id, start.id, 'each request has its own id');
  assert.equal(page.context.workerMessages.length, 4, 'the status poll reports nothing');
});

test('a page unload ends the requests that have not finished', async () => {
  const pageFetch = () => new Promise(() => {});
  const page = buildPage({pageFetch});
  page.context.fetch('/api/sessions/s1/events?before=3');
  await settled();
  page.firePageHide();
  await settled();
  const types = page.context.workerMessages.map((message) => message.type);
  assert.deepEqual(types, ['page-request-start', 'page-request-end'], 'the unload ends the open request');
});
