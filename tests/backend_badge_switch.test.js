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

function makeTextElement() {
  const el = {};
  Object.defineProperty(el, 'textContent', {
    get() { return el._text || ''; },
    set(v) { el._text = String(v || ''); },
  });
  return el;
}

function makeDocument(elements) {
  return { getElementById: (id) => elements.get(id) || null };
}

function loadSessionContext({elements, BACKEND_OPTIONS, SESSION_ID, fetchImpl}) {
  const context = {
    document: makeDocument(elements),
    console: { error: () => {}, log: () => {} },
    showToast: () => {},
    BACKEND_OPTIONS: BACKEND_OPTIONS || {},
    SESSION_ID: SESSION_ID || 'session-a',
    // config.js's shared header literal; this harness skips config.js.
    JSON_HEADERS: {'Content-Type': 'application/json'},
    fetch: fetchImpl || (() => Promise.resolve({ ok: true, async json() { return { id: SESSION_ID, backend: SESSION_ID }; } })),
  };
  vm.createContext(context);
  vm.runInContext(readStatic('sidebar/namespace.js'), context, { filename: 'sidebar/namespace.js' });
  vm.runInContext(readStatic('sidebar/session-view.js'), context, { filename: 'sidebar/session-view.js' });
  return context;
}

function headerFixtures() {
  return new Map([
    ['backend-badge', makeBadgeElement()],
    ['input-model-badge', makeTextElement()],
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
  assert.equal(elements.get('input-model-badge').textContent, 'Opus 5');
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
  await context.switchBackend('claude-fable-5');
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
  await context.switchBackend('claude-fable-5');
  assert.equal(context.getActiveBackendId(), 'claude-opus-5', 'active id reverts on failure');
  assert.ok(toasts.some((t) => t.msg.includes('clone/fork')), 'server detail is surfaced');
});
