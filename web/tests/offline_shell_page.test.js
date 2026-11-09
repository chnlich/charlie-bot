// The page side of the offline shell's warm: the worker's reply reaches the page as
// charliebot:offline-shell-warm, and a browser without a service worker reports that instead, so the
// poor-network bench's warm wait closes on every client.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const SOURCE = fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'offline-shell.js'), 'utf8');
const WORKER_PATH = '/offline-shell-worker.js';

// One page: the events it dispatched, the registration's worker, and the load listener the script
// installed. `serviceWorkerReady` resolves or rejects `navigator.serviceWorker.ready`.
function buildPage({serviceWorker = true, readyError = null} = {}) {
  const events = [];
  const windowListeners = new Map();
  const context = {
    console: {error: () => {}},
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
    },
    channels: [],
  };
  if (!serviceWorker) context.navigator = {};
  context.window = context;
  context.addEventListener = (type, listener) => {
    if (!windowListeners.has(type)) windowListeners.set(type, []);
    windowListeners.get(type).push(listener);
  };
  if (serviceWorker) {
    context.navigator = {
      serviceWorker: {
        registered: [],
        register: (path_, options) => {
          context.navigator.serviceWorker.registered.push({path: path_, options});
          return Promise.resolve();
        },
        ready: readyError !== null ? Promise.reject(readyError) : Promise.resolve({
          active: {
            postMessage: (message, transfer) => {
              context.postMessage = {message, transfer};
              const channel = context.channels[context.channels.length - 1];
              if (channel) context.reply = (data) => channel.port1.onmessage({data});
            },
          },
        }),
        getRegistration: async () => null,
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
    fireLoad: () => {
      for (const listener of windowListeners.get('load') || []) listener();
    },
  };
}

test('the warm asks the active worker and reports its reply as an event', async () => {
  const page = buildPage();
  page.fireLoad();
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(page.registered().length, 1);
  assert.equal(page.registered()[0].path, WORKER_PATH);
  assert.equal(page.registered()[0].options.scope, '/?session=');
  assert.equal(page.context.postMessage.message.type, 'warm');
  assert.equal(page.context.postMessage.transfer.length, 1, 'the reply channel travels with the message');
  page.context.reply({stored: true});
  assert.equal(page.events.length, 1);
  assert.equal(page.events[0].type, 'charliebot:offline-shell-warm');
  assert.equal(page.events[0].detail.stored, true);
});

test('a browser without service workers reports that the warm cannot run', () => {
  const page = buildPage({serviceWorker: false});
  page.fireLoad();
  assert.equal(page.events.length, 1);
  assert.equal(page.events[0].detail.supported, false);
});

test('a registration that never became active reports the error', async () => {
  const failure = new Error('register refused');
  const page = buildPage({readyError: failure});
  page.fireLoad();
  await Promise.resolve();
  assert.equal(page.events.length, 1);
  assert.equal(page.events[0].detail.error, String(failure));
});
