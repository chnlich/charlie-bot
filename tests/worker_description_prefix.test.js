const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');
const { escapeHtml } = require('./escape_html_stub');

function fakeElement() {
  return {
    children: [],
    dataset: {},
    style: {},
    classList: { toggle() {}, contains() { return false; } },
    prepend(child) { this.children.unshift(child); return child; },
    appendChild(child) { this.children.push(child); return child; },
    querySelectorAll() { return []; },
    remove() {},
    innerHTML: '',
  };
}

function loadSidebarWorkers(elements, modalCalls) {
  const context = {
    SESSION_ID: 'session-a',
    BACKEND_OPTIONS: {},
    document: {
      createElement: () => fakeElement(),
      getElementById: (id) => elements.get(id) || null,
      querySelectorAll: () => [],
    },
    escapeHtml,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
    showTextModal: (title, text) => { modalCalls.push({ title, text }); },
    fetch: () => Promise.resolve({ ok: true, json: async () => ({ description: 'FULL DESCRIPTION' }) }),
    loadedThreads: { delete() {} },
    stopThreadPoll: () => {},
    fetchAndRenderEvents: () => Promise.resolve(),
    console: { warn: () => {}, error: () => {} },
  };
  vm.createContext(context);
  vm.runInContext(readStatic('sidebar/namespace.js'), context, { filename: 'sidebar/namespace.js' });
  vm.runInContext(readStatic('sidebar/workers.js'), context, { filename: 'sidebar/workers.js' });
  return context;
}

const FULL_THREAD = {
  id: 'thread-full',
  status: 'completed',
  description: 'short task',
  created_at: '2026-07-01T12:00:00Z',
};

const TRUNCATED_THREAD = {
  id: 'thread-truncated',
  status: 'completed',
  description: 'spec '.repeat(48),
  description_full_len: 1500,
  created_at: '2026-07-01T12:00:00Z',
};

test('a truncated row opens the modal through the fetch handler, never data-full', async () => {
  const container = fakeElement();
  const modalCalls = [];
  const context = loadSidebarWorkers(new Map([['tab-workers', container]]), modalCalls);

  context.renderWorkersTab([FULL_THREAD, TRUNCATED_THREAD], 'session-a', []);

  // The inline onclick resolves names against the window global, so the
  // handler must be exposed there (an IIFE-local function is a ReferenceError
  // on click).
  assert.equal(typeof context.fetchWorkerDescription, 'function');
  assert.match(container.innerHTML, /onclick="event\.stopPropagation\(\); fetchWorkerDescription\('thread-truncated', 'session-a'\)"/);
  assert.doesNotMatch(container.innerHTML, /thread-truncated[\s\S]*?data-full/);

  await context.fetchWorkerDescription('thread-truncated', 'session-a');
  assert.deepEqual(modalCalls, [{ title: 'Worker Description', text: 'FULL DESCRIPTION' }]);
});

test('an untruncated row keeps the inline data-full modal path', () => {
  const container = fakeElement();
  const context = loadSidebarWorkers(new Map([['tab-workers', container]]), []);

  context.renderWorkersTab([FULL_THREAD], 'session-a', []);

  assert.match(container.innerHTML, /data-full="short task"/);
  assert.match(container.innerHTML, /onclick="event\.stopPropagation\(\); showTextModal\('Worker Description', this\.dataset\.full\)"/);
});

test('addWorkerCard carries the truncation marker to the painted card', () => {
  const container = fakeElement();
  const context = loadSidebarWorkers(new Map([['tab-workers', container]]), []);

  context.addWorkerCard('thread-live', TRUNCATED_THREAD.description, '2026-07-01T12:00:00Z', '', TRUNCATED_THREAD.description_full_len);
  context.addWorkerCard('thread-short', FULL_THREAD.description, '2026-07-01T12:00:00Z', '');

  assert.match(container.children[1].innerHTML, /fetchWorkerDescription\('thread-live', 'session-a'\)/);
  assert.match(container.children[0].innerHTML, /data-full="short task"/);
});
