const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { baseSessionContext, createChatSidebarContext } = require('./session_context_stub');
const { createElement } = require('./dom_element_stub');

const TELEMETRY_URL = '/api/diag/switch-events';

function makeSidebarRow(sessionId, name) {
  const nameEl = createElement({textContent: name});
  return createElement({
    id: 'session-' + sessionId,
    querySelector: (sel) => (sel === '.session-name' ? nameEl : null),
  });
}

function bootstrapPayload(sessionId) {
  return {
    session: {id: sessionId, name: 'Session ' + sessionId, backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [{role: 'assistant', content: 'hello from ' + sessionId, event_index: 5}],
    pending_draft: null,
    event_count: 6,
    oldest_message_ordinal: 0,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    switchable_backends: [],
    has_more: false,
    threads: [],
    triggers: [],
  };
}

// vm harness for switchSession: bootstrap fetches resolve through manual
// promises (pendingSwitches[i].resolve()), telemetry posts are recorded and
// answered immediately, and every other endpoint gets a benign empty JSON.
function buildSwitchHarness() {
  const bootstrapFetches = [];
  const pendingSwitches = [];
  const telemetryPosts = [];
  const sequence = [];
  const failBootstrap = new Set();
  const knobs = {failTelemetry: false};
  const payloads = {
    'session-a': bootstrapPayload('session-a'),
    'session-b': bootstrapPayload('session-b'),
    'session-c': bootstrapPayload('session-c'),
  };
  const messages = createElement({id: 'messages'});
  messages.clientHeight = 500;
  messages.scrollHeight = 1000;
  const headerName = createElement({id: 'header-session-name'});
  const rows = [
    makeSidebarRow('session-a', 'Alpha'),
    makeSidebarRow('session-b', 'Beta'),
    makeSidebarRow('session-c', 'Gamma'),
  ];
  const elements = new Map([
    ['messages', messages],
    ['header-session-name', headerName],
    ['backend-badge', createElement()],
    ['input-model-badge', createElement()],
    ['msg-input', createElement()],
    ...rows.map((row) => [row.id, row]),
  ]);

  const {context} = baseSessionContext({elements});
  context.eventCursor = 0;
  context.document.getElementById = (id) => {
    const fromMap = elements.get(id);
    if (fromMap) return fromMap;
    for (const child of messages.children) {
      if (child.id === id) return child;
    }
    return null;
  };
  context.document.querySelectorAll = (sel) => (sel === '[id^="session-"]' ? rows : []);
  context.document.querySelector = () => null;
  context.fetch = async (url, opts = {}) => {
    if (url === TELEMETRY_URL) {
      const body = JSON.parse(opts.body);
      telemetryPosts.push(body);
      sequence.push('telemetry:' + body.phase);
      if (knobs.failTelemetry) throw new Error('telemetry down');
      return {ok: true, status: 200, json: async () => ({ok: true})};
    }
    const match = url.match(/\/api\/sessions\/([^/]+)\/bootstrap/);
    bootstrapFetches.push(url);
    sequence.push('fetch:' + (match ? match[1] : url));
    if (!match || failBootstrap.has(match[1])) {
      return {ok: false, status: 500, json: async () => ({})};
    }
    const entry = {sessionId: match[1]};
    entry.done = new Promise((resolve) => { entry.resolve = resolve; });
    pendingSwitches.push(entry);
    return entry.done.then(() => ({
      ok: true,
      status: 200,
      json: async () => payloads[entry.sessionId],
    }));
  };
  let locationHref = '';
  context.location = {
    get href() { return locationHref; },
    set href(value) { sequence.push('reload'); locationHref = String(value); },
    protocol: 'http:',
    host: 'localhost:8000',
    search: '',
  };
  context.setInterval = () => 1;
  context.setTimeout = () => 1;
  context.clearInterval = () => {};
  context.clearTimeout = () => {};

  createChatSidebarContext(context);
  return {
    context,
    messages,
    elements,
    headerName,
    bootstrapFetches,
    pendingSwitches,
    telemetryPosts,
    sequence,
    failBootstrap,
    knobs,
    payloadFor: (sid) => payloads[sid],
    resolveLastSwitch: () => pendingSwitches[pendingSwitches.length - 1].resolve(),
  };
}

// ---------------------------------------------------------------------------
// Acceptance: click-time placeholder, header name, sidebar highlight
// ---------------------------------------------------------------------------

test('click paints the placeholder synchronously over the old session DOM', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));

  const click = h.context.switchSession('session-b');  // no await: click-time DOM

  assert.deepEqual(h.telemetryPosts.map((t) => t.phase), ['started']);
  const row = h.messages.children[0];
  assert.equal(row.id, 'switch-placeholder', 'placeholder must be the first messages child at click time');
  assert.equal(h.messages.children.length, 1, 'old session DOM must be gone at click time');
  assert.equal(row.children[0].className.includes('animate-pulse-dot'), true,
      'placeholder must reuse the thinking pulse-dot style');
  assert.match(row.children[1].textContent, /正在打开/);
  assert.match(row.children[1].textContent, /Beta/);
  assert.equal(h.headerName.textContent, 'Beta', 'header name must update at click time');
  assert.equal(h.elements.get('session-session-b').classList.contains('bg-blue-600/20'), true,
      'sidebar highlight must move at click time');
  assert.equal(h.elements.get('session-session-a').classList.contains('bg-blue-600/20'), false);

  h.resolveLastSwitch();
  await click;
  assert.match(h.messages.innerHTML, /hello from session-b/, 'render must replace the placeholder');
  assert.equal(h.messages.children.length, 0, 'the placeholder element must not survive the render');
});

test('started telemetry carries exactly the eight schema fields', async () => {
  const h = buildSwitchHarness();

  const click = h.context.switchSession('session-b');
  h.resolveLastSwitch();
  await click;

  assert.equal(h.telemetryPosts.length, 2);
  const started = h.telemetryPosts[0];
  assert.deepEqual(Object.keys(started).sort(), [
    'client_ts', 'elapsed_ms', 'error', 'from_session', 'generation', 'phase', 'to_session', 'winner_generation',
  ]);
  assert.equal(started.phase, 'started');
  assert.equal(started.from_session, 'session-a');
  assert.equal(started.to_session, 'session-b');
  assert.equal(started.generation, 1);
  assert.equal(started.winner_generation, null);
  assert.equal(started.elapsed_ms, 0);
  assert.equal(started.error, null);
  assert.match(started.client_ts, /^\d{4}-\d{2}-\d{2}T/);
  assert.equal(h.telemetryPosts[1].phase, 'completed');
  assert.equal(h.telemetryPosts[1].elapsed_ms >= 0, true);
});

// ---------------------------------------------------------------------------
// Acceptance: superseded generation + 60s bootstrap cache
// ---------------------------------------------------------------------------

test('superseded generation never renders, logs superseded, and feeds the cache', async () => {
  const h = buildSwitchHarness();
  const p1 = h.context.switchSession('session-b');
  const p2 = h.context.switchSession('session-c');
  h.pendingSwitches[0].resolve();  // b's bootstrap lands after c took over
  await p1;
  h.pendingSwitches[1].resolve();
  await p2;

  assert.deepEqual(h.telemetryPosts.map((t) => t.phase + ':' + t.to_session), [
    'started:session-b', 'started:session-c', 'superseded:session-b', 'completed:session-c',
  ]);
  const sup = h.telemetryPosts[2];
  assert.equal(sup.generation, 1);
  assert.equal(sup.winner_generation, 2, 'superseded must name the generation that beat it');
  assert.equal(typeof sup.elapsed_ms, 'number');

  assert.match(h.messages.innerHTML, /hello from session-c/);
  assert.doesNotMatch(h.messages.innerHTML, /hello from session-b/);

  // A→B→A: the re-click of b hits the cached bootstrap — zero fetch, direct render.
  const fetchesBefore = h.bootstrapFetches.length;
  await h.context.switchSession('session-b');
  assert.equal(h.bootstrapFetches.length, fetchesBefore, 'cache hit must skip the bootstrap fetch');
  assert.match(h.messages.innerHTML, /hello from session-b/);
  assert.doesNotMatch(h.messages.innerHTML, /hello from session-c/);
  assert.equal(h.context.eventCursor, 6, 'cache-hit render must seed the replay cursor');
  assert.equal(h.context.switching, false);
  const completedB = h.telemetryPosts.filter((t) => t.phase === 'completed' && t.to_session === 'session-b');
  assert.equal(completedB.length, 1, 'the cache-hit path must log completed too');
  assert.equal(completedB[0].elapsed_ms < 50, true, 'cache-hit completion should be near-immediate');
});

test('cache hit happens only within the 60s window', async () => {
  const h = buildSwitchHarness();
  const p1 = h.context.switchSession('session-b');
  const p2 = h.context.switchSession('session-c');
  h.pendingSwitches[0].resolve();
  await p1;
  h.pendingSwitches[1].resolve();
  await p2;
  // SESSION_ID is now c; b's superseded bootstrap sits in the cache.

  // Age the context clock past the TTL: the re-click must fetch afresh.
  h.context.__clockSkewMs = 0;
  vm.runInContext(
      'const __realDateNow = Date.now; Date.now = () => __realDateNow() + globalThis.__clockSkewMs;',
      h.context);
  h.context.__clockSkewMs = 61000;
  const p3 = h.context.switchSession('session-b');
  h.resolveLastSwitch();
  await p3;
  h.context.__clockSkewMs = 0;

  assert.equal(h.bootstrapFetches.filter((u) => u.includes('/session-b/')).length, 2,
      'an expired cache entry must force a fresh fetch (original + aged re-click)');
  assert.match(h.messages.innerHTML, /hello from session-b/);
});

// ---------------------------------------------------------------------------
// Acceptance: failed bootstrap logs failed before the reload fallback
// ---------------------------------------------------------------------------

test('failed bootstrap logs failed telemetry before the full-load fallback', async () => {
  const h = buildSwitchHarness();
  h.failBootstrap.add('session-b');

  await h.context.switchSession('session-b');

  assert.deepEqual(h.sequence, [
    'telemetry:started', 'fetch:session-b', 'telemetry:failed', 'reload',
  ]);
  const failed = h.telemetryPosts.find((t) => t.phase === 'failed');
  assert.equal(failed.to_session, 'session-b');
  assert.equal(failed.error, '500');
  assert.equal(typeof failed.elapsed_ms, 'number');
  assert.match(h.context.location.href, /^\/\?session=session-b$/);
  assert.equal(h.context.switching, false, 'the failed path must release the switching flag');
});

// ---------------------------------------------------------------------------
// Acceptance: render_error logged then rethrown
// ---------------------------------------------------------------------------

test('render error is logged with the message and rethrown', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView = () => { throw new Error('boom'); };

  const click = h.context.switchSession('session-b');
  h.resolveLastSwitch();
  await assert.rejects(click, /boom/);

  assert.deepEqual(h.telemetryPosts.map((t) => t.phase), ['started', 'render_error']);
  const re = h.telemetryPosts[1];
  assert.equal(re.error, 'boom');
  assert.equal(re.to_session, 'session-b');
  assert.equal(typeof re.elapsed_ms, 'number');
});

// ---------------------------------------------------------------------------
// Acceptance: telemetry failure cannot break switching
// ---------------------------------------------------------------------------

test('telemetry failure cannot break the switch', async () => {
  const h = buildSwitchHarness();
  h.knobs.failTelemetry = true;

  const click = h.context.switchSession('session-b');
  h.resolveLastSwitch();
  await click;

  assert.match(h.messages.innerHTML, /hello from session-b/);
  assert.equal(h.context.switching, false);
  assert.deepEqual(h.telemetryPosts.map((t) => t.phase), ['started', 'completed']);
});
