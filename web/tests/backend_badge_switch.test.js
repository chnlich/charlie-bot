const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');

// Minimal fake for a badge element. `innerHTML` replacement produces a
// queriable `select[data-backend-switch]` child; `textContent` assignment
// records the read-only label. Mirrors enough of the real DOM for the sidebar's
// header badge render + switch wiring.
function makeBadgeElement() {
  const el = {
    tagName: 'SPAN',
    _html: '',
    _text: '',
    _select: null,
    classList: { add() {}, remove() {}, toggle() {}, contains() { return false; } },
  };
  Object.defineProperty(el, 'textContent', {
    get() { return el._text; },
    set(v) { el._text = String(v || ''); },
  });
  Object.defineProperty(el, 'innerHTML', {
    get() { return el._html; },
    set(v) {
      el._html = v;
      el._select = {
        _value: '',
        value: '',
        addEventListener() {}, // change wiring isn't asserted through this fake
      };
    },
  });
  el.querySelector = (sel) => (sel === 'select[data-backend-switch]' ? el._select : null);
  return el;
}

function makeDocument(elements) {
  return { getElementById: (id) => elements.get(id) || null };
}

function loadSessionContext({elements, BACKEND_OPTIONS, BACKEND_POOLS, ACTIVE_POOL_MEMBER, ACTIVE_NATIVE_MODEL, SESSION_ID, fetchImpl}) {
  const context = {
    document: makeDocument(elements),
    console: { error: () => {}, log: () => {} },
    showToast: () => {},
    BACKEND_OPTIONS: BACKEND_OPTIONS || {},
    BACKEND_POOLS,
    ACTIVE_POOL_MEMBER,
    ACTIVE_NATIVE_MODEL,
    SESSION_ID: SESSION_ID || 'session-a',
    // config.js's shared literal pair; this harness skips config.js.
    JSON_HEADERS: {'Content-Type': 'application/json'},
    PROGRESS_BAR_FILL_CLASS: 'h-full rounded-full transition-all duration-300',
    fetch: fetchImpl || (() => Promise.resolve({ ok: true, async json() { return { id: SESSION_ID, backend: SESSION_ID }; } })),
    // session-view.js registers its stream-draft mode with usage.js and its
    // projection check with the turn engine at load; this harness loads neither
    // usage.js nor the chat modules.
    Chat: { setStreamDraftModeSupplier() {}, registerProjectionCheck() {} },
  };
  vm.createContext(context);
  vm.runInContext(readStatic('sidebar/namespace.js'), context, { filename: 'sidebar/namespace.js' });
  vm.runInContext(readStatic('sidebar/session-view.js'), context, { filename: 'sidebar/session-view.js' });
  return context;
}

function headerFixtures() {
  return new Map([
    ['backend-badge', makeBadgeElement()],
  ]);
}

// ---------------------------------------------------------------------------
// 1. Read-only badge when the same-domain list holds only the current backend.
// ---------------------------------------------------------------------------
test('updateActiveBackendBadges keeps a read-only badge for a single-backend domain', () => {
  const elements = headerFixtures();
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
  });
  context.setSwitchableBackends(['claude-opus-5']);
  context.setActiveBackendId('claude-opus-5');
  context.updateActiveBackendBadges();
  const badge = elements.get('backend-badge');
  assert.equal(badge.innerHTML, '');
  assert.equal(badge.textContent, 'Opus 5');
});

// ---------------------------------------------------------------------------
// 2. Multi-backend domain renders a dropdown listing exactly the same-domain
//    options (no domain logic in the client — the list rides the payload).
// ---------------------------------------------------------------------------
test('updateActiveBackendBadges renders a dropdown using the server-computed list', () => {
  const elements = headerFixtures();
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
  });
  context.setSwitchableBackends(['claude-opus-5', 'claude-fable-5']);
  context.setActiveBackendId('claude-opus-5');
  context.updateActiveBackendBadges();
  const badge = elements.get('backend-badge');
  assert.match(badge.innerHTML, /select\s+data-backend-switch/);
  assert.match(badge.innerHTML, /claude-opus-5/);
  assert.match(badge.innerHTML, /claude-fable-5/);
  // The active one is marked selected.
  assert.ok(badge.innerHTML.includes('claude-opus-5" selected'), badge.innerHTML);
});

// ---------------------------------------------------------------------------
// 3. switchBackend POSTs and, on 2xx, syncs the header; unknown id is refused.
// ---------------------------------------------------------------------------
test('switchBackend POSTs the target and updates the active id on success', async () => {
  const elements = headerFixtures();
  const calls = [];
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
    fetchImpl: (url, opts) => {
      calls.push({ url, opts });
      return Promise.resolve({ ok: true, async json() { return { id: 'session-a', backend: 'claude-fable-5' }; } });
    },
  });
  context.setSwitchableBackends(['claude-opus-5', 'claude-fable-5']);
  context.setActiveBackendId('claude-opus-5');
  await context.Sidebar.switchBackend('claude-fable-5');
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, '/api/sessions/session-a/backend');
  assert.equal(calls[0].opts.method, 'POST');
  assert.deepEqual(JSON.parse(calls[0].opts.body), { backend: 'claude-fable-5' });
  assert.equal(context.getActiveBackendId(), 'claude-fable-5');
});

// ---------------------------------------------------------------------------
// 4. On a non-2xx response the server `detail` is surfaced and the control
//    reverts to the active value (no domain logic in the client).
// ---------------------------------------------------------------------------
test('switchBackend surfaces the server detail and reverts the active id on failure', async () => {
  const elements = headerFixtures();
  const toasts = [];
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
    fetchImpl: () => Promise.resolve({
      ok: false,
      status: 400,
      async json() { return { detail: 'cross-domain: clone/fork instead' }; },
    }),
  });
  context.showToast = (msg, err) => toasts.push({ msg, err });
  context.setSwitchableBackends(['claude-opus-5', 'claude-fable-5']);
  context.setActiveBackendId('claude-opus-5');
  await context.Sidebar.switchBackend('claude-fable-5');
  assert.equal(context.getActiveBackendId(), 'claude-opus-5', 'active id reverts on failure');
  assert.ok(toasts.some((t) => t.msg.includes('clone/fork')), 'server detail is surfaced');
});

// ---------------------------------------------------------------------------
// 5. An Auto session's header badge reads "Auto → <member>", listing Auto in
//    the dropdown when it is switchable.
// ---------------------------------------------------------------------------
test('an Auto session badge shows the label and the current member', () => {
  const elements = headerFixtures();
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
    BACKEND_POOLS: [{ id: 'auto', label: 'Auto' }],
    ACTIVE_POOL_MEMBER: 'claude-opus-5',
  });
  context.setSwitchableBackends(['auto']);
  context.setActiveBackendId('auto');
  context.updateActiveBackendBadges();
  assert.equal(elements.get('backend-badge').textContent, 'Auto → Opus 5');
});

test('an Auto session badge names the recorded model after its member', () => {
  const elements = headerFixtures();
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
    BACKEND_POOLS: [{ id: 'auto', label: 'Auto' }],
    ACTIVE_POOL_MEMBER: 'claude-opus-5',
    ACTIVE_NATIVE_MODEL: 'claude-opus-5-5',
  });
  context.setSwitchableBackends(['auto']);
  context.setActiveBackendId('auto');
  context.updateActiveBackendBadges();
  assert.equal(elements.get('backend-badge').textContent, 'Auto → Opus 5 · claude-opus-5-5');
});

test('an Auto session without a recorded member shows the Auto label alone', () => {
  const elements = headerFixtures();
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
    BACKEND_POOLS: [{ id: 'auto', label: 'Auto' }],
  });
  context.setSwitchableBackends(['auto']);
  context.setActiveBackendId('auto');
  context.updateActiveBackendBadges();
  assert.equal(elements.get('backend-badge').textContent, 'Auto');
});

test('the switch dropdown offers Auto with its member label', () => {
  const elements = headerFixtures();
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
    BACKEND_POOLS: [{ id: 'auto', label: 'Auto' }],
    ACTIVE_POOL_MEMBER: 'claude-fable-5',
  });
  context.setSwitchableBackends(['claude-opus-5', 'auto']);
  context.setActiveBackendId('auto');
  context.updateActiveBackendBadges();
  const html = elements.get('backend-badge').innerHTML;
  assert.ok(html.includes('value="auto" selected'), html);
  assert.ok(html.includes('Auto → Fable 5'), html);
});

// ---------------------------------------------------------------------------
// 6. A session on a plain option lists Auto in its switch dropdown when the
//    server's switchable list carries it, and selecting Auto posts `auto`.
// ---------------------------------------------------------------------------
test('a plain-option session renders the Auto entry from the server-computed list', () => {
  const elements = headerFixtures();
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
    BACKEND_POOLS: [{ id: 'auto', label: 'Auto' }],
  });
  context.setSwitchableBackends(['claude-opus-5', 'claude-fable-5', 'auto']);
  context.setActiveBackendId('claude-opus-5');
  context.updateActiveBackendBadges();
  const html = elements.get('backend-badge').innerHTML;
  assert.ok(html.includes('value="claude-fable-5"'), html);
  assert.ok(html.includes('value="auto">Auto</option>'), html);
  // The session is not on Auto, so the Auto entry carries no member label.
  assert.ok(!html.includes('Auto →'), html);
});

test('selecting Auto in the switch dropdown posts auto and lands the session on Auto', async () => {
  const elements = headerFixtures();
  const calls = [];
  const context = loadSessionContext({
    elements,
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: { 'claude-opus-5': 'Opus 5', 'claude-fable-5': 'Fable 5' },
    BACKEND_POOLS: [{ id: 'auto', label: 'Auto' }],
    fetchImpl: (url, opts) => {
      calls.push({ url, opts });
      return Promise.resolve({ ok: true, async json() { return { id: 'session-a', backend: 'auto' }; } });
    },
  });
  context.setSwitchableBackends(['claude-opus-5', 'claude-fable-5', 'auto']);
  context.setActiveBackendId('claude-opus-5');
  await context.Sidebar.switchBackend('auto');
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, '/api/sessions/session-a/backend');
  assert.equal(calls[0].opts.method, 'POST');
  assert.deepEqual(JSON.parse(calls[0].opts.body), { backend: 'auto' });
  assert.equal(context.getActiveBackendId(), 'auto');
});
