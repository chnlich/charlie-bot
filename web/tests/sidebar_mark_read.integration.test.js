const assert = require('node:assert/strict');
const test = require('node:test');

const { bootstrapPayload, buildSwitchFlowHarness, makeSidebarRow } = require('./session_context_stub');

// A network answer: no stored-copy header (view-requests.js isStoredResponse reads it).
const NO_STORED_COPY = {get: () => null};

// ---------------------------------------------------------------------------
// Switch flow: the POST lands only on the winning generation's landed render,
// through the shared switch-flow harness in session_context_stub.js.
// ---------------------------------------------------------------------------

const BOOTSTRAP = {
  'session-b': bootstrapPayload('session-b', 0, false),
  'session-c': bootstrapPayload('session-c', 0, false),
};

function buildHarness() {
  return buildSwitchFlowHarness({
    rows: [
      makeSidebarRow('session-a', 'Alpha'),
      makeSidebarRow('session-b', 'Beta'),
      makeSidebarRow('session-c', 'Gamma'),
    ],
    scrollHeight: 100,
    fields: {readPosts: [], pendingBootstraps: {}},
    fetch: (h, url, opts = {}) => {
      if (url.endsWith('/read') && opts.method === 'POST') {
        h.readPosts.push(url);
        return Promise.resolve({ok: true, headers: NO_STORED_COPY, json: async () => ({})});
      }
      const boot = url.match(/\/api\/sessions\/([^/]+)\/bootstrap/);
      if (boot) {
        const gate = h.pendingBootstraps[boot[1]];
        if (gate) return gate.promise;
        return Promise.resolve({ok: true, headers: NO_STORED_COPY, status: 200, json: async () => BOOTSTRAP[boot[1]]});
      }
      return Promise.resolve({ok: true, headers: NO_STORED_COPY, status: 200, json: async () => ({})});
    },
  });
}

// Park one session's bootstrap fetch so the test owns when that switch's
// response lands (the generation check runs after it).
function parkBootstrap(h, sessionId) {
  const gate = {};
  gate.promise = new Promise((resolve) => { gate.resolve = resolve; });
  gate.keep = () => gate.resolve({
    ok: true, headers: NO_STORED_COPY, status: 200, json: async () => BOOTSTRAP[sessionId]});
  return gate;
}

test('a completed switch posts /read for the switched-to session once, after its catch-up', async () => {
  const h = buildHarness();

  await h.context.switchSession('session-b');
  assert.deepEqual(h.readPosts, [], 'the read waits for the socket catch-up');
  h.context.onSocketCatchupComplete(0);

  assert.deepEqual(h.readPosts, ['/api/sessions/session-b/read']);
  assert.equal(h.context.sessionUnread['session-b'], false, 'the local unread clear stays');
  assert.equal(h.context.SESSION_ID, 'session-b');
});

test('a superseded switch generation never posts /read', async () => {
  const h = buildHarness();
  h.pendingBootstraps['session-b'] = parkBootstrap(h, 'session-b');

  const superseded = h.context.switchSession('session-b'); // parks on B's bootstrap
  await h.context.switchSession('session-c');              // wins: renders
  h.context.onSocketCatchupComplete(0);                    // C catches up and posts
  h.pendingBootstraps['session-b'].keep();                 // B lands after; generation check drops it
  await superseded;

  assert.deepEqual(h.readPosts, ['/api/sessions/session-c/read'],
      'the superseded generation must return before the POST, not after it');
});

test('a render error posts no /read', async () => {
  const h = buildHarness();
  h.context.renderSessionView = () => { throw new Error('boom'); };

  await assert.rejects(h.context.switchSession('session-b'), /boom/);

  assert.deepEqual(h.readPosts, []);
});
