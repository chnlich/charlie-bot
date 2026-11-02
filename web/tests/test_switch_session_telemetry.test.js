const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {baseSessionContext, bootstrapPayload, createChatSidebarContext, installSessionDocumentLookups, makeSidebarRow,
  stubPageTimers} = require('./session_context_stub');
const {createElement} = require('./dom_element_stub');
const {readPackageStatic, readStatic} = require('./read_static');

// The wire path the diag package's switch-events.js posts each switch phase
// to; an endpoint move on either side breaks the pin.
const SWITCH_TELEMETRY_URL = '/api/diag/switch-events';

const tick = () => new Promise((resolve) => setImmediate(resolve));
const flush = async () => { await tick(); await tick(); };

// Core announces each switch phase as a page event, and the diag package's
// listener posts it: the harness document calls each listener of an event's
// type in registration order, as the page's document does.
function loadSwitchTelemetry(context) {
  const listeners = new Map();
  context.document.addEventListener = (type, listener) => {
    if (!listeners.has(type)) listeners.set(type, []);
    listeners.get(type).push(listener);
  };
  context.document.dispatchEvent = (event) => {
    for (const listener of listeners.get(event.type) || []) listener(event);
    return true;
  };
  vm.runInContext(readPackageStatic('features', 'diag', 'switch-events.js'), context, {filename: 'switch-events.js'});
}

// vm harness for switchSession: bootstrap fetches resolve through manual
// promises (pendingSwitches[i].resolve()), telemetry posts are recorded and
// answered immediately, and every other endpoint gets a benign empty JSON.
// The composer paths ride the real config.js, file-upload.js and chat/input.js
// (the chat module list loads input.js in page order), so a chat POST, a
// cancel, an upload, a rename and a session create are recorded like the
// telemetry posts are.
function buildSwitchHarness() {
  const bootstrapFetches = [];
  const pendingSwitches = [];
  const telemetryPosts = [];
  const readPosts = [];
  const chatPosts = [];
  const cancelPosts = [];
  const uploadPosts = [];
  const renamePosts = [];
  const sequence = [];
  const failBootstrap = new Set();
  const knobs = {failTelemetry: false};
  const payloads = {
    'session-a': bootstrapPayload('session-a', 0, false),
    'session-b': bootstrapPayload('session-b', 0, false),
    'session-c': bootstrapPayload('session-c', 0, false),
  };
  const messages = createElement({id: 'messages'});
  messages.clientHeight = 500;
  messages.scrollHeight = 1000;
  const headerName = createElement({id: 'header-session-name'});
  // getBoundingClientRect stands in for the row geometry startRename anchors on.
  const rowWithGeometry = (sessionId, name) => {
    const row = makeSidebarRow(sessionId, name);
    row.getBoundingClientRect = () => ({top: 0, left: 0, width: 100});
    return row;
  };
  const rows = [
    rowWithGeometry('session-a', 'Alpha'),
    rowWithGeometry('session-b', 'Beta'),
    rowWithGeometry('session-c', 'Gamma'),
  ];
  const elements = new Map([
    ['messages', messages],
    ['header-session-name', headerName],
    ['backend-badge', createElement()],
    ['input-model-badge', createElement()],
    ['msg-input', createElement()],
    ['rename-input', createElement({id: 'rename-input', className: 'hidden', select() {}})],
    ['thinking', createElement({className: 'hidden'})],
    ['thinking-time', createElement()],
    ...rows.map((row) => [row.id, row]),
  ]);

  const {context} = baseSessionContext({elements});
  context.eventCursor = 0;
  context.confirm = () => true;
  context.FormData = class { append() {} };
  installSessionDocumentLookups(context, elements, messages, rows);
  context.fetch = async (url, opts = {}) => {
    if (url === SWITCH_TELEMETRY_URL) {
      const body = JSON.parse(opts.body);
      telemetryPosts.push(body);
      sequence.push('telemetry:' + body.phase);
      if (knobs.failTelemetry) throw new Error('telemetry down');
      return {ok: true, status: 200, json: async () => ({ok: true})};
    }
    const readMatch = url.match(/\/api\/sessions\/([^/]+)\/read$/);
    if (readMatch && opts.method === 'POST') {
      readPosts.push(readMatch[1]);
      return {ok: true, status: 200, json: async () => ({})};
    }
    const chatMatch = url.match(/\/api\/chat\/([^/]+)\/message$/);
    if (chatMatch && opts.method === 'POST') {
      const body = JSON.parse(opts.body);
      chatPosts.push({sessionId: chatMatch[1], content: body.content, requestId: body.request_id});
      sequence.push('chat:' + chatMatch[1]);
      return {ok: true, status: 202, json: async () => ({})};
    }
    const cancelMatch = url.match(/\/api\/chat\/([^/]+)\/cancel$/);
    if (cancelMatch) {
      cancelPosts.push(cancelMatch[1]);
      sequence.push('cancel:' + cancelMatch[1]);
      return {ok: true, status: 200, json: async () => ({})};
    }
    const uploadMatch = url.match(/\/api\/chat\/([^/]+)\/upload$/);
    if (uploadMatch) {
      uploadPosts.push({sessionId: uploadMatch[1], filename: opts.body && opts.body.name});
      sequence.push('upload:' + uploadMatch[1]);
      return {ok: true, status: 200, json: async () => ({filename: 'notes.txt', path: '/tmp/notes.txt', size: 12})};
    }
    const archiveMatch = url.match(/\/api\/sessions\/([^/]+)\/archive$/);
    if (archiveMatch) {
      sequence.push('archive:' + archiveMatch[1]);
      return {ok: true, status: 200, json: async () => ({})};
    }
    const renameMatch = url.match(/\/api\/sessions\/([^/]+)$/);
    if (renameMatch && opts.method === 'PATCH') {
      renamePosts.push({sessionId: renameMatch[1], name: JSON.parse(opts.body).name});
      return {ok: true, status: 200, json: async () => ({})};
    }
    if (url === '/api/sessions/' && opts.method === 'POST') {
      const created = {id: 'session-new', name: 'Nova', backend: 'claude-opus-4.6', archive_offset: 0,
        thinking_since: null};
      sequence.push('create');
      return {ok: true, status: 200, json: async () => created};
    }
    const match = url.match(/\/api\/sessions\/([^/]+)\/bootstrap/);
    bootstrapFetches.push(url);
    sequence.push('fetch:' + (match ? match[1] : url));
    if (!match || failBootstrap.has(match[1])) {
      return {ok: false, status: 500, json: async () => ({})};
    }
    const entry = {sessionId: match[1]};
    entry.done = new Promise((resolve, reject) => { entry.resolve = resolve; entry.reject = reject; });
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
  stubPageTimers(context);

  createChatSidebarContext(context);
  // config.js and file-upload.js ride the page ahead of the module list's
  // runtime calls: the real saveDraftNow and showToast replace the base
  // stubs, and the upload gate answers the send path.
  vm.runInContext(readStatic('config.js'), context, {filename: 'config.js'});
  vm.runInContext(readStatic('file-upload.js'), context, {filename: 'file-upload.js'});
  loadSwitchTelemetry(context);
  // The optimistic bubble's one observable: appendMessage records the bubble
  // it renders, so a test asserts which view the bubble was appended against
  // through the sequence markers.
  const appendedMessages = [];
  context.appendMessage = (role, content, inputMode, timestamp, files, requestId) => {
    appendedMessages.push({role, content, requestId});
    sequence.push('append:' + (content || ''));
    return {role, content, request_id: requestId, delivery_status: 'Sending…'};
  };
  return {
    context,
    messages,
    elements,
    headerName,
    bootstrapFetches,
    pendingSwitches,
    telemetryPosts,
    readPosts,
    chatPosts,
    cancelPosts,
    uploadPosts,
    renamePosts,
    appendedMessages,
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
  assert.match(row.children[1].textContent, /Opening/);
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
  assert.deepEqual(h.readPosts, ['session-c', 'session-b'],
      'only landed renders post /read: c network switch, b cache-hit switch');
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
  assert.deepEqual(h.readPosts, ['session-c', 'session-b'],
      'the aged re-click renders after its fresh fetch and posts /read');
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

// ---------------------------------------------------------------------------
// Acceptance: the displayed session, the composer target and the held sends
// ---------------------------------------------------------------------------

// The two surfaces the displayed-session module paints agree on one name.
function assertDisplayedAgreement(h, expectedName, label) {
  assert.equal(h.headerName.textContent, expectedName, 'header at ' + label);
  assert.equal(h.context.document.title, expectedName + ' · CharlieBot@localhost:8000', 'title at ' + label);
}

test('a send pressed inside the switch window posts to the session the header names', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));

  const click = h.context.switchSession('session-b');  // bootstrap held: the header names Beta
  const input = h.elements.get('msg-input');
  input.value = 'held words';
  await h.context.sendMessage();

  assert.deepEqual(h.chatPosts, [], 'the send must wait for the switch to land');
  h.resolveLastSwitch();
  await click;
  await flush();

  assert.deepEqual(h.chatPosts.map((post) => post.sessionId), ['session-b'],
      'the message lands in the session the header named at press');
  assert.equal(h.chatPosts[0].content, 'held words');
  assert.deepEqual(h.appendedMessages, [
    {role: 'user', content: 'held words', requestId: h.chatPosts[0].requestId},
  ], 'the message shows in S2’s rendered view');
  assert.ok(h.sequence.indexOf('append:held words') > h.sequence.indexOf('telemetry:completed'),
      'the bubble is appended against the freshly rendered S2 view');
});

test('a send superseded by a newer click posts to the session it bound, and the new draft is untouched', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));
  h.context.localStorage.setItem('charliebot-draft-session-b', 'press-time words');
  h.context.localStorage.setItem('charliebot-draft-session-c', 'c draft');

  const clickB = h.context.switchSession('session-b');
  await h.context.sendMessage();  // the composer carries S2's draft; the send binds session-b

  const clickC = h.context.switchSession('session-c');
  h.resolveLastSwitch();
  await clickC;
  await flush();

  assert.deepEqual(h.chatPosts.map((post) => post.sessionId), ['session-b'],
      'the send posts to the session it bound before the newer click swapped the composer');
  assert.equal(h.chatPosts[0].content, 'press-time words');
  assert.equal(h.context.localStorage.getItem('charliebot-draft-session-b'), null,
      'the send cleared the bound session’s draft');
  assert.equal(h.context.localStorage.getItem('charliebot-draft-session-c'), 'c draft',
      'S3’s draft is untouched');
  assert.equal(h.elements.get('msg-input').value, 'c draft');
  assert.ok(h.sequence.indexOf('append:press-time words') < h.sequence.indexOf('fetch:session-c'),
      'the bubble is appended into the dying view before S3’s render, so S3 never shows it');
  h.pendingSwitches[0].resolve();  // b's superseded bootstrap still lands, into the cache
  await clickB;
});

test('header and tab title agree at click, after render and after renames', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));
  assertDisplayedAgreement(h, 'Session session-a', 'the initial render');

  const click = h.context.switchSession('session-b');
  assertDisplayedAgreement(h, 'Beta', 'the click');

  h.resolveLastSwitch();
  await click;
  assertDisplayedAgreement(h, 'Session session-b', 'the landed render');

  // Rename the displayed session through its sidebar menu.
  h.context.startRename({preventDefault() {}, stopPropagation() {}}, 'session-b');
  const renameInput = h.elements.get('rename-input');
  renameInput.value = 'Renamed B';
  await h.context.commitRename();
  assertDisplayedAgreement(h, 'Renamed B', 'the displayed session’s rename');

  // Rename a different session from its sidebar menu: nothing repaints.
  h.context.startRename({preventDefault() {}, stopPropagation() {}}, 'session-c');
  renameInput.value = 'Renamed C';
  await h.context.commitRename();
  assertDisplayedAgreement(h, 'Renamed B', 'another session’s rename');
  assert.deepEqual(h.renamePosts, [
    {sessionId: 'session-b', name: 'Renamed B'},
    {sessionId: 'session-c', name: 'Renamed C'},
  ]);

  // The send target is still the displayed session.
  const input = h.elements.get('msg-input');
  input.value = 'target check';
  await h.context.sendMessage();
  await flush();
  assert.deepEqual(h.chatPosts.map((post) => post.sessionId), ['session-b']);
});

test('the composer carries the target’s draft at click and keeps window typing across the landing', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));
  h.context.localStorage.setItem('charliebot-draft-session-b', 'b draft');

  const click = h.context.switchSession('session-b');
  const input = h.elements.get('msg-input');
  assert.equal(input.value, 'b draft', 'the composer shows S2’s draft at click');

  input.value = 'b draft typed on';
  h.context.saveDraftNow();  // the debounced save flushes against the live DRAFT_KEY

  h.resolveLastSwitch();
  await click;

  assert.equal(input.value, 'b draft typed on', 'the landing leaves the composer as the user left it');
  assert.equal(h.context.localStorage.getItem('charliebot-draft-session-b'), 'b draft typed on',
      'typing inside the window stored as S2’s draft');
});

test('Stop and attach pressed in the window act on the target after it lands and are dropped when superseded', async () => {
  // Both act on S2 once its switch lands.
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));
  const click = h.context.switchSession('session-b');
  await h.context.cancelMaster();
  await h.context.uploadFile({name: 'notes.txt', size: 12});
  assert.deepEqual(h.cancelPosts, [], 'Stop waits for the landing');
  assert.deepEqual(h.uploadPosts, [], 'the upload waits for the landing');
  h.resolveLastSwitch();
  await click;
  await flush();
  assert.deepEqual(h.cancelPosts, ['session-b']);
  assert.deepEqual(h.uploadPosts.map((upload) => upload.sessionId), ['session-b']);

  // S3 supersedes: both are dropped with a note by the composer.
  const h2 = buildSwitchHarness();
  h2.context.renderSessionView(h2.payloadFor('session-a'));
  const clickB = h2.context.switchSession('session-b');
  await h2.context.cancelMaster();
  await h2.context.uploadFile({name: 'notes.txt', size: 12});
  const toasts = [];
  h2.context.showToast = (msg, isError) => toasts.push({msg, isError});
  const clickC = h2.context.switchSession('session-c');
  h2.resolveLastSwitch();
  await clickC;
  await flush();
  assert.deepEqual(h2.cancelPosts, []);
  assert.deepEqual(h2.uploadPosts, []);
  assert.equal(toasts.length, 2, 'both drops leave a visible note');
  assert.equal(toasts.every((toast) => toast.isError), true);
  h2.pendingSwitches[0].resolve();  // b's superseded bootstrap still lands, into the cache
  await clickB;
});

test('a new session and the archive back to the welcome page keep the display and the send target together', async () => {
  // New session: created, displayed and sendable.
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));
  await h.context.createSession();
  assertDisplayedAgreement(h, 'Nova', 'the new session');
  assert.equal(h.context.SESSION_ID, 'session-new');
  const input = h.elements.get('msg-input');
  input.value = 'first words';
  await h.context.sendMessage();
  await flush();
  assert.deepEqual(h.chatPosts.map((post) => post.sessionId), ['session-new']);

  // Archive the displayed session with no other row left: the welcome view
  // restores the original title.
  const h2 = buildSwitchHarness();
  h2.context.renderSessionView(h2.payloadFor('session-a'));
  const rowA = h2.elements.get('session-session-a');
  h2.context.document.querySelectorAll = () => [rowA];
  const main = createElement({id: 'main'});
  const welcomeTpl = createElement({id: 'welcome-view'});
  welcomeTpl.innerHTML = '<div>welcome</div>';
  h2.elements.set('welcome-view', welcomeTpl);
  h2.context.document.querySelector = (sel) => (sel === 'main' ? main : null);
  await h2.context.archiveSession('session-a');
  assert.equal(h2.context.SESSION_ID, null);
  assert.equal(h2.context.document.title, 'CharlieBot@localhost:8000',
      'the welcome page restores the original title');
});

test('a send pressed in a failing switch’s window posts to the bound session before the page navigates', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));
  h.failBootstrap.add('session-b');

  const click = h.context.switchSession('session-b');
  const input = h.elements.get('msg-input');
  input.value = 'last words';
  await h.context.sendMessage();
  await click;
  await flush();

  assert.deepEqual(h.chatPosts.map((post) => post.sessionId), ['session-b'],
      'the held message posts to the session it bound');
  assert.equal(h.chatPosts[0].content, 'last words');
  assert.ok(h.sequence.indexOf('chat:session-b') < h.sequence.indexOf('reload'),
      'the POST goes out before the full-load navigation: ' + h.sequence.join(' -> '));
});

test('clicking back to the loaded session inside a pending switch lands on one session everywhere', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));

  const clickB = h.context.switchSession('session-b');
  await h.context.switchSession('session-a');  // the early return: a is still the loaded session
  assert.equal(h.headerName.textContent, 'Beta', 'the pending switch still owns the display');

  h.resolveLastSwitch();
  await clickB;
  await flush();

  assert.equal(h.context.SESSION_ID, 'session-b');
  assertDisplayedAgreement(h, 'Session session-b', 'the landing');
  assert.match(h.messages.innerHTML, /hello from session-b/,
      'the rendered session is the one every surface names');
  const input = h.elements.get('msg-input');
  input.value = 'target check';
  await h.context.sendMessage();
  await flush();
  assert.deepEqual(h.chatPosts.map((post) => post.sessionId), ['session-b'],
      'the send target is the rendered session');
});

test('page load starts the displayed session from the template name, with no bootstrap payload', () => {
  const h = buildSwitchHarness();
  assert.equal(h.context.SESSION_BOOTSTRAP, null);
  assert.equal(vm.runInContext('displayedSessionId', h.context), 'session-a');
  assert.equal(vm.runInContext('displayedSessionName', h.context), 'Session session-a');
});

test('a second click on the pending target keeps its held composer actions for the landing', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));

  const firstClick = h.context.switchSession('session-b');
  h.elements.get('msg-input').value = 'held words';
  await h.context.sendMessage();
  await h.context.cancelMaster();
  const secondClick = h.context.switchSession('session-b');  // a double click on the same row
  await flush();
  assert.deepEqual(h.chatPosts, [], 'the re-click leaves the held send waiting for the landing');
  assert.deepEqual(h.cancelPosts, [], 'the re-click leaves the held Stop waiting for the landing');

  h.resolveLastSwitch();
  await secondClick;
  await flush();
  h.pendingSwitches[0].resolve();  // the first click's superseded bootstrap
  await firstClick;
  assert.deepEqual(h.chatPosts.map((post) => post.sessionId), ['session-b']);
  assert.deepEqual(h.cancelPosts, ['session-b']);
});

test('a superseded switch that fails does not navigate the page away from the newer target', async () => {
  const h = buildSwitchHarness();
  h.context.renderSessionView(h.payloadFor('session-a'));

  const clickB = h.context.switchSession('session-b');
  const clickC = h.context.switchSession('session-c');
  h.elements.get('msg-input').value = 'for c';
  await h.context.sendMessage();  // held for session-c's switch
  h.pendingSwitches[0].reject(new Error('bootstrap 500'));  // session-b fails after the click moved on
  await clickB;
  assert.equal(h.context.location.href, '', 'the page stays on the view the newer click owns');

  h.resolveLastSwitch();
  await clickC;
  await flush();
  assert.deepEqual(h.chatPosts.map((post) => post.sessionId), ['session-c'],
      'the send held for session-c still lands in session-c');
});
