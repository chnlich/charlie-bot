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
// the document listeners the script installed. `readyError` rejects `navigator.serviceWorker.ready`.
function buildPage({
  serviceWorker = true,
  readyError = null,
  sessionId = null,
  sessionBootstrap = null,
  initialSessions = null,
  initialLoadErrors = [],
} = {}) {
  const events = [];
  const documentListeners = new Map();
  const windowListeners = new Map();
  const context = {
    console: {error: () => {}},
    location: {href: 'https://charlie.test/?session='},
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
        if (channel) context.reply = (data) => channel.port1.onmessage({data});
      },
    };
    context.navigator = {
      serviceWorker: {
        registered: [],
        register: (path_, options) => {
          context.navigator.serviceWorker.registered.push({path: path_, options});
          return Promise.resolve();
        },
        ready: readyError !== null ? Promise.reject(readyError) : Promise.resolve({active}),
        getRegistration: async () => ({active}),
      },
    };
  }
  vm.createContext(context);
  vm.runInContext(SOURCE, context);
  context.registerOfflineShell();
  return {
    events,
    context,
    registered: () => context.navigator?.serviceWorker.registered ?? [],
    firePageCurrent: () => {
      // A copy of the list: a once-only listener removes itself from the list as it runs.
      for (const listener of [...(documentListeners.get(PAGE_CURRENT) || [])]) listener();
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

test('a registration that never became active reports the error', async () => {
  const failure = new Error('register refused');
  const page = buildPage({readyError: failure});
  page.firePageCurrent();
  await Promise.resolve();
  assert.equal(page.events.length, 1);
  assert.equal(page.events[0].detail.error, String(failure));
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
  page.context.reply({stored: 3});
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

test('every same-origin file a page loader names is on the on-demand list', () => {
  const listed = new Set(onDemandPaths());
  const named = new Set();
  for (const file of pageSourceFiles()) {
    const text = fs.readFileSync(file, 'utf8');
    if (!/\bload(PageScripts|PageStyles)\(/.test(text)) continue;
    for (const match of text.matchAll(/['"](\/static\/[^'"]+)['"]/g)) named.add(match[1]);
  }
  assert.ok(named.size > 0, 'the scan found the loader files it reads');
  assert.deepEqual([...named].filter((file) => !listed.has(file)), [],
      'a loader names a same-origin file that the on-demand list does not store offline');
});

// The paths in offline-shell.js's ON_DEMAND_FILES, unversioned.
function onDemandPaths() {
  const block = SOURCE.match(/const ON_DEMAND_FILES = Object\.freeze\(\[([\s\S]*?)\]\);/);
  return [...block[1].matchAll(/'([^']+)'/g)].map((match) => match[1]);
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
