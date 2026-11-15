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
      for (const listener of documentListeners.get(PAGE_CURRENT) || []) listener();
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
  page.firePageCurrent();
  await new Promise((resolve) => setImmediate(resolve));

  assert.deepEqual(JSON.parse(JSON.stringify(page.context.postMessage.message.copies)), [
    {
      url: 'https://charlie.test/api/sessions/session-a/bootstrap',
      body: bootstrap,
    },
    {
      url: 'https://charlie.test/api/sessions/?compact=1',
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
      [{type: 'catchup-complete'}, {type: 'warm', copies: []}]);
  page.context.workerMessages.length = 0;
  page.firePageCurrent();
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(JSON.parse(JSON.stringify(page.context.workerMessages)), [{type: 'catchup-complete'}],
      'the once-only warm does not repeat; the catch-up report does');
});
