const assert = require('node:assert/strict');
const test = require('node:test');

const { loadSidebarStatusContext } = require('./sidebar_status_context_stub');
// ---------------------------------------------------------------------------
// markSessionRead standalone: sidebar/status.js through the shared stub loader.
// ---------------------------------------------------------------------------

test('markSessionRead issues one POST to /api/sessions/<id>/read', async () => {
  const calls = [];
  const context = loadSidebarStatusContext({
    fetch: async (url, opts) => {
      calls.push({url, opts});
      return {ok: true, json: async () => ({})};
    },
  });

  context.markSessionRead('session-x');
  await new Promise((r) => setImmediate(r));

  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, '/api/sessions/session-x/read');
  assert.equal(calls[0].opts.method, 'POST');
});

test('markSessionRead swallows a fetch failure with a console log only', async () => {
  const errors = [];
  const context = loadSidebarStatusContext({
    console: {error: (...args) => errors.push(args), log: () => {}},
    fetch: () => Promise.reject(new Error('network down')),
  });

  context.markSessionRead('session-x');
  // Two macrotask turns flush the rejected fetch's .catch hop.
  await new Promise((r) => setImmediate(r));
  await new Promise((r) => setImmediate(r));

  assert.equal(errors.length, 1, 'the failure must be logged, not propagated');
});

