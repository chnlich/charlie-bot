// ---------------------------------------------------------------------------
// Stage D: a redirected worker_summary card carries a footer line naming and
// linking to the origin session, so the reader can open it and find the worker
// thread. Non-redirected cards are byte-identical to the pre-change markup.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { loadChatRenderingModules, makeChatRenderContext } = require('./chat_rendering_context_stub');

function loadContext() {
  const context = makeChatRenderContext();
  vm.createContext(context);
  loadChatRenderingModules(context);
  return context;
}

function workerSummary(overrides) {
  const msg = {
    role: 'worker_summary',
    content: 'Worker finished the task.',
    thread_id: 'th-1',
    ...overrides,
  };
  return msg;
}

test('redirected worker_summary renders a footer link and thread id', () => {
  const ctx = loadContext();
  const html = ctx.renderMessage(
      workerSummary({ origin_session_id: 'origin-1' }),
      'sess-1');
  assert.match(html, /Ran in session/);
  assert.match(html, /href="\/\?session=origin-1"/);
  assert.match(html, />origin-1</);
  assert.match(html, /thread th-1/);
});

test('worker_summary whose origin equals the rendered session renders no footer', () => {
  const ctx = loadContext();
  const html = ctx.renderMessage(
      workerSummary({ origin_session_id: 'sess-1' }),
      'sess-1');
  assert.doesNotMatch(html, /Ran in session/);
  assert.doesNotMatch(html, /\/\?session=/);
});

test('worker_summary with no origin renders byte-identical markup to the current implementation', () => {
  const ctx = loadContext();
  const withNoOrigin = ctx.renderMessage(workerSummary(), 'sess-1');
  const withNoThread = ctx.renderMessage(workerSummary({ thread_id: undefined }), 'sess-1');
  const withMissingOriginField = ctx.renderMessage(
      workerSummary({ origin_session_id: undefined }), 'sess-1');
  assert.equal(withNoThread, withNoOrigin);
  assert.equal(withMissingOriginField, withNoOrigin);
  assert.doesNotMatch(withNoOrigin, /Ran in session/);
  assert.doesNotMatch(withNoOrigin, /\/\?session=/);
});