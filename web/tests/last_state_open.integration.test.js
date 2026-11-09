// ---------------------------------------------------------------------------
// The open path's last-state contract (sidebar/last-state.js), driven through
// the real session-view.js, filters.js and last-state.js modules. The page's
// document routes dispatchEvent to its listeners, so the charliebot:* seams
// between the modules run as they do in the browser.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const {
  bootstrapPayload,
  buildSwitchFlowHarness,
  buildUsageElements,
  makeSessionMeta,
  makeSidebarRow,
} = require('./session_context_stub');
const { createElement } = require('./dom_element_stub');

const STORED_AT_HEADER = 'X-CharlieBot-Stored-At';
const NETWORK_ONLY_HEADER = 'X-CharlieBot-Network-Only';
const STORED_AT = '2026-11-06T00:00:00Z';
const STARRED_URL = '/api/sessions/starred';

const flush = () => new Promise((resolve) => setImmediate(resolve));

// The response headers the page reads: a stored copy carries the stored-at time, a network answer none.
function responseHeaders({stored}) {
  return {get: (name) => (stored && name === STORED_AT_HEADER ? STORED_AT : null)};
}

// The page the open runs in. Every GET parks until the test answers it, so the test owns the order in which the
// first screen, the list refresh and the socket's catch-up land. The Starred view is the list the refresh reads: it
// has no workspace badges to answer beside it.
function buildOpenHarness() {
  const page = {requests: [], readPosts: [], log: []};
  const h = buildSwitchFlowHarness({
    rows: [makeSidebarRow('session-a', 'Alpha'), makeSidebarRow('session-b', 'Beta')],
    scrollHeight: 100,
    fields: {},
    fetch: (_h, url, opts = {}) => {
      if (url.endsWith('/read') && opts.method === 'POST') {
        page.readPosts.push(url);
        return Promise.resolve({
          ok: true, status: 200, headers: responseHeaders({stored: false}), json: async () => ({})});
      }
      page.log.push('fetch ' + url);
      return new Promise((resolve, reject) => {
        page.requests.push({
          url,
          headers: opts.headers || {},
          respond(body, {stored = false} = {}) {
            resolve({ok: true, status: 200, headers: responseHeaders({stored}), json: async () => body});
          },
          fail(error) { reject(error); },
        });
      });
    },
    contextTweaks: (context) => {
      context.TextEncoder = TextEncoder;
      const handlers = new Map();
      context.document.addEventListener = (type, handler) => {
        if (!handlers.has(type)) handlers.set(type, []);
        handlers.get(type).push(handler);
      };
      context.document.dispatchEvent = (event) => {
        (handlers.get(event.type) || []).forEach((handler) => handler(event));
        return true;
      };
    },
  });
  h.elements.set('session-list', createElement());
  h.context.currentFilter = 'starred';
  const paint = h.context.renderSessionView;
  h.context.renderSessionView = (data) => {
    page.log.push('paint ' + data.session.id);
    return paint(data);
  };
  return {h, page};
}

function requestsTo(page, suffix) {
  return page.requests.filter((request) => request.url.endsWith(suffix));
}

function lastRequestTo(page, suffix) {
  const found = requestsTo(page, suffix);
  assert.ok(found.length > 0, 'no request to ' + suffix);
  return found[found.length - 1];
}

// The chat column's mark: shown, and the freshness state it carries.
function markOf(h) {
  const mark = h.elements.get('freshness-mark');
  return {shown: !mark.classList.contains('hidden'), state: mark.getAttribute('data-freshness') ?? null};
}

// Opens a session whose first screen answers from a stored copy, and leaves the sidebar list refresh and the
// socket's catch-up pending.
async function openFromCopy(h, page, sessionId, body) {
  const opened = h.context.switchSession(sessionId);
  await flush();
  lastRequestTo(page, '/bootstrap').respond(body, {stored: true});
  await opened;
}

function dispatchPage(h, type, detail) {
  h.context.document.dispatchEvent(new h.context.CustomEvent(type, {detail}));
}

test('a stored first screen paints its messages before the sidebar list refresh starts', async () => {
  const {h, page} = buildOpenHarness();

  await openFromCopy(h, page, 'session-b', bootstrapPayload('session-b', 0, false));

  const paintedAt = page.log.indexOf('paint session-b');
  const refreshedAt = page.log.indexOf('fetch ' + STARRED_URL);
  assert.ok(paintedAt >= 0 && refreshedAt >= 0, page.log.join(', '));
  assert.ok(paintedAt < refreshedAt, 'the messages paint before the list refresh is requested');
});

test('the first screen\'s render announces its newest message for the page\'s own timing', async () => {
  const {h, page} = buildOpenHarness();
  const announced = [];
  h.context.document.addEventListener('charliebot:first-screen-rendered', (event) => announced.push(event.detail));
  const body = bootstrapPayload('session-b', 0, false);
  body.messages = [{id: 9, role: 'assistant', content: 'newest', event_index: 5}];

  await openFromCopy(h, page, 'session-b', body);

  assert.equal(announced.length, 1);
  assert.equal(announced[0].sessionId, 'session-b');
  assert.equal(announced[0].newestMessageId, 9);
});

test('a stored copy shows the mark until catch-up and the list refresh land, then posts the read', async () => {
  const {h, page} = buildOpenHarness();
  await openFromCopy(h, page, 'session-b', bootstrapPayload('session-b', 0, false));

  assert.deepEqual(markOf(h), {shown: true, state: 'not-current'});
  assert.deepEqual(page.readPosts, [], 'no read before the catch-up');

  h.context.onSocketCatchupComplete(6);
  await flush();
  assert.deepEqual(markOf(h), {shown: true, state: 'not-current'}, 'the list refresh still runs');
  assert.deepEqual(page.readPosts, [], 'no read before the list refresh');

  const refresh = lastRequestTo(page, STARRED_URL);
  assert.equal(refresh.headers[NETWORK_ONLY_HEADER], '1', 'the refresh must reach the network');
  refresh.respond([makeSessionMeta('session-b')]);
  await flush();

  assert.deepEqual(markOf(h), {shown: false, state: null});
  assert.deepEqual(page.readPosts, ['/api/sessions/session-b/read']);
});

test('a stored position past the session event total discards the copy and fetches the first screen', async () => {
  const {h, page} = buildOpenHarness();
  await openFromCopy(h, page, 'session-b', {...bootstrapPayload('session-b', 0, false), event_count: 9});
  lastRequestTo(page, STARRED_URL).respond([makeSessionMeta('session-b')]);
  await flush();

  h.context.onSocketCatchupComplete(6);
  await flush();

  const firstScreens = requestsTo(page, '/bootstrap');
  assert.equal(firstScreens.length, 2, 'the copy is replaced by one network first-screen request');
  assert.equal(firstScreens[1].headers[NETWORK_ONLY_HEADER], '1');
  assert.deepEqual(page.readPosts, [], 'no read while the network first screen waits for its catch-up');

  firstScreens[1].respond(bootstrapPayload('session-b', 0, false));
  await flush();
  assert.equal(page.log.filter((entry) => entry === 'paint session-b').length, 2, 'the network first screen paints');
  assert.deepEqual(markOf(h), {shown: false, state: null});

  h.context.onSocketCatchupComplete(6);
  await flush();
  assert.deepEqual(page.readPosts, ['/api/sessions/session-b/read'], 'the read posts once, after the second catch-up');
});

test('a socket replay larger than the stored first screen fetches the first screen', async () => {
  const {h, page} = buildOpenHarness();
  await openFromCopy(h, page, 'session-b', bootstrapPayload('session-b', 0, false));

  dispatchPage(h, 'charliebot:client-socket-replay', {bytes: 1});
  assert.equal(requestsTo(page, '/bootstrap').length, 1, 'a replay within the copy keeps it');

  dispatchPage(h, 'charliebot:client-socket-replay', {bytes: 1000000});
  const firstScreens = requestsTo(page, '/bootstrap');
  assert.equal(firstScreens.length, 2, 'a replay past the copy requests the first screen again');
  assert.equal(firstScreens[1].headers[NETWORK_ONLY_HEADER], '1');
});

test('a boot with no embedded first screen requests it at once, then reads after catch-up', async () => {
  const {h, page} = buildOpenHarness();

  const opened = h.context.openSession('session-b', null);
  assert.equal(page.requests.length, 1, 'the first-screen request leaves before any await');
  assert.ok(page.requests[0].url.endsWith('/api/sessions/session-b/bootstrap'), page.requests[0].url);

  page.requests[0].respond(bootstrapPayload('session-b', 0, false));
  await opened;

  assert.equal(requestsTo(page, STARRED_URL).length, 0, 'a network first screen starts no list refresh');
  assert.deepEqual(markOf(h), {shown: false, state: null});
  assert.deepEqual(page.readPosts, [], 'no read before the catch-up');

  h.context.onSocketCatchupComplete(6);
  assert.deepEqual(page.readPosts, ['/api/sessions/session-b/read']);
});

test('an offline open with no stored copy names the missing copy in the placeholder', async () => {
  const {h, page} = buildOpenHarness();
  h.context.navigator = {userAgent: 'TestAgent/1.0', onLine: false};

  h.context.switchSession('session-b');
  await flush();
  page.requests[0].fail(new TypeError('Failed to fetch'));
  await flush();

  const placeholder = h.messages.children.find((child) => child.id === 'switch-placeholder');
  assert.ok(placeholder, 'the switch placeholder stays on screen while the open retries');
  assert.equal(placeholder.textContent, h.context.NO_LOCAL_COPY_LABEL);
});

test('an offline usage panel shows the offline label and loads no usage', async () => {
  const {h} = buildOpenHarness();
  for (const [id, el] of buildUsageElements()) h.elements.set(id, el);
  h.context.navigator = {userAgent: 'TestAgent/1.0', onLine: false};

  h.context.renderUsageFromData({});

  assert.equal(h.elements.get('usage-text')?.innerHTML, h.context.OFFLINE_PANEL_MARK);
});
