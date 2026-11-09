const assert = require('node:assert/strict');
const test = require('node:test');

const { loadSidebarStatusContext } = require('./sidebar_status_context_stub');
// ---------------------------------------------------------------------------
// markSessionRead standalone: sidebar/status.js through the shared stub loader.
// ---------------------------------------------------------------------------

test('markSessionRead clears the local fact and POSTs /api/sessions/<id>/read, with a click’s dot', async () => {
  const calls = [];
  const context = loadSidebarStatusContext({
    document: {getElementById: () => null},
    fetch: async (url, opts) => {
      calls.push({url, opts});
      return {ok: true, json: async () => ({})};
    },
  });
  context.recordUnreadFact('session-x', true);

  context.markSessionRead('session-x', null);
  context.markSessionRead('session-y', true);
  await new Promise((r) => setImmediate(r));

  assert.equal(context.ownUnread('session-x'), false);
  assert.equal(calls.length, 2);
  assert.equal(calls[0].url, '/api/sessions/session-x/read');
  assert.equal(calls[0].opts.method, 'POST');
  assert.equal(calls[1].url, '/api/sessions/session-y/read?page_dot=true', 'a click’s read reports the row’s dot');
});

test('markSessionRead swallows a fetch failure with a console log only', async () => {
  const errors = [];
  const context = loadSidebarStatusContext({
    console: {error: (...args) => errors.push(args), log: () => {}},
    document: {getElementById: () => null},
    fetch: () => Promise.reject(new Error('network down')),
  });

  context.markSessionRead('session-x', null);
  // Two macrotask turns flush the rejected fetch's .catch hop.
  await new Promise((r) => setImmediate(r));
  await new Promise((r) => setImmediate(r));

  assert.equal(errors.length, 1, 'the failure must be logged, not propagated');
});
