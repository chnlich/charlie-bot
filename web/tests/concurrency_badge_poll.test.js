// ---------------------------------------------------------------------------
// The concurrency badge's own poll: a hidden tab makes no request until it is
// visible, and at most one request is in flight at a time.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const BADGE_JS = fs.readFileSync(
    path.join(__dirname, '..', 'static', 'js', 'concurrency-badge.js'), 'utf8');

const flush = () => new Promise((resolve) => setImmediate(resolve));

// The badge script loaded into a page with the given visibility. Requests answer only when the test says so.
function loadBadge({hidden}) {
  const listeners = new Map();
  const timers = [];
  const page = {requests: []};
  const document = {
    hidden,
    visibilityState: hidden ? 'hidden' : 'visible',
    readyState: 'complete',
    addEventListener(type, fn) {
      if (!listeners.has(type)) listeners.set(type, []);
      listeners.get(type).push(fn);
    },
    dispatch(type) {
      (listeners.get(type) || []).forEach((fn) => fn());
    },
    getElementById: () => null,
    querySelector: () => null,
    querySelectorAll: () => [],
    createElement: () => ({style: {}, classList: {add() {}, remove() {}, toggle() {}}, setAttribute() {}}),
  };
  const context = {
    document,
    console: {error() {}, warn() {}, log() {}},
    window: {addEventListener() {}, setInterval: () => 0, clearInterval() {}, setTimeout: () => 0},
    setTimeout: (fn) => {
      timers.push(fn);
      return timers.length;
    },
    clearTimeout() {},
    fetch: (url) => new Promise((resolve, reject) => {
      page.requests.push({url, respond: (body) => resolve({ok: true, json: async () => body}), fail: reject});
    }),
    Date,
    Math,
    Number,
    String,
    Array,
    Object,
    JSON,
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(BADGE_JS, context, {filename: 'concurrency-badge.js'});
  page.context = context;
  page.document = document;
  page.show = () => {
    document.hidden = false;
    document.visibilityState = 'visible';
    document.dispatch('visibilitychange');
  };
  page.hide = () => {
    document.hidden = true;
    document.visibilityState = 'hidden';
    document.dispatch('visibilitychange');
  };
  page.runTimers = () => timers.splice(0).forEach((fn) => fn());
  return page;
}

test('a hidden tab makes no badge request until it is visible, then makes one', async () => {
  const page = loadBadge({hidden: true});
  page.context.ConcurrencyBadge.refreshSoon();
  page.runTimers();
  await flush();
  assert.deepEqual(page.requests, [], 'a hidden tab makes no request');

  page.show();
  assert.equal(page.requests.length, 1, 'the badge refreshes once the tab is visible');
});

test('the badge keeps one request in flight, and a refresh during it queues exactly one more', async () => {
  const page = loadBadge({hidden: false});
  assert.equal(page.requests.length, 1, 'the load-time refresh is the first request');

  page.context.ConcurrencyBadge.refresh();
  page.context.ConcurrencyBadge.refresh();
  page.context.ConcurrencyBadge.refreshSoon();
  page.runTimers();
  assert.equal(page.requests.length, 1, 'no second request starts while the first is in flight');

  page.requests[0].respond({});
  await flush();
  assert.equal(page.requests.length, 2, 'the calls made in flight run as one queued request');

  page.requests[1].respond({});
  await flush();
  assert.equal(page.requests.length, 2, 'nothing else queued, so no third request');
});
