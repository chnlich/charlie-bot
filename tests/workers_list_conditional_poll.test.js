const assert = require('node:assert/strict');
const test = require('node:test');

const {loadSidebarWorkersContext} = require('./sidebar_workers_context_stub');

const ROW = {
  id: 'thread-1',
  type: 'thread',
  status: 'running',
  description: 'task',
  created_at: '2026-07-01T12:00:00Z',
};

// fetch responses the harness hands out in order; each records the request it
// served so the test can assert the conditional request's URL and options.
function stubFetch(rounds) {
  const calls = [];
  const fetch = (url, opts) => {
    calls.push({url, opts: opts || {}});
    const round = rounds[Math.min(calls.length - 1, rounds.length - 1)];
    return Promise.resolve({
      ok: round.status === 200,
      status: round.status,
      headers: {get: (name) => (name === 'ETag' ? round.etag : null)},
      json: async () => round.items,
    });
  };
  return {fetch, calls};
}

test('the workers poll repeats the rendered ETag via ?etag= and skips the repaint on 204', async () => {
  const container = {
    innerHTML: '',
    children: [],
    prepend(child) { this.children.unshift(child); },
    appendChild(child) { this.children.push(child); },
  };
  const {fetch, calls} = stubFetch([
    {status: 200, etag: '"e1"', items: [ROW]},
    {status: 204},
  ]);
  const context = loadSidebarWorkersContext({
    document: {
      createElement: () => ({className: '', innerHTML: '', children: [], prepend() {}, appendChild() {}}),
      getElementById: (id) => (id === 'tab-workers' ? container : null),
      querySelectorAll: () => [],
    },
    startPageTimer: () => {},
    fetch,
  });

  await context.ensureWorkersLoadedForActiveSession({force: true});
  assert.equal(calls[0].url, '/api/threads/session-a/list');
  assert.equal(context.workersListEtag, '"e1"');
  assert.match(container.innerHTML, /thread-1/);

  await context.pollWorkers();
  assert.equal(calls[1].url, '/api/threads/session-a/list?etag=%22e1%22');
  assert.equal(calls[1].opts.cache, 'no-store');
  assert.equal(context.workersListEtag, '"e1"');
});

test('a 200 after a 204 refreshes the stored ETag for the next poll', async () => {
  const container = {
    innerHTML: '',
    children: [],
    prepend(child) { this.children.unshift(child); },
    appendChild(child) { this.children.push(child); },
  };
  const {fetch, calls} = stubFetch([
    {status: 200, etag: '"e1"', items: [ROW]},
    {status: 200, etag: '"e2"', items: [Object.assign({}, ROW, {id: 'thread-2'})]},
  ]);
  const context = loadSidebarWorkersContext({
    document: {
      createElement: () => ({className: '', innerHTML: '', children: [], prepend() {}, appendChild() {}}),
      getElementById: (id) => (id === 'tab-workers' ? container : null),
      querySelectorAll: () => [],
    },
    startPageTimer: () => {},
    fetch,
  });

  await context.ensureWorkersLoadedForActiveSession({force: true});
  await context.pollWorkers();
  assert.equal(calls[1].url, '/api/threads/session-a/list?etag=%22e1%22');
  assert.equal(context.workersListEtag, '"e2"');
});
