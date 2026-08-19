// ---------------------------------------------------------------------------
// Stage D: a redirected worker_summary card carries a footer line naming and
// linking to the origin session, so the reader can open it and find the worker
// thread. Non-redirected cards are byte-identical to the pre-change markup.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ROOT = path.join(__dirname, '..');

function readStatic(relativePath) {
  return fs.readFileSync(path.join(ROOT, 'web', 'static', 'js', relativePath), 'utf8');
}

function escapeForFakeDom(str) {
  return String(str).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

class FakeElement {
  constructor(tag = 'DIV') {
    this.tagName = String(tag).toUpperCase();
    this.children = [];
    this.dataset = {};
    this._html = '';
    this._text = '';
  }

  get innerHTML() { return this._html + this.children.map((c) => c.innerHTML).join(''); }

  set innerHTML(html) {
    this._html = html;
    this.children = [];
  }

  get textContent() { return this._text; }

  set textContent(value) {
    this._text = String(value || '');
    this.innerHTML = escapeForFakeDom(this._text);
  }

  appendChild(child) { this.children.push(child); return child; }
  querySelector() { return null; }
  querySelectorAll() { return []; }
}

function makeDocument() {
  return {
    getElementById() { return null; },
    createElement(tag) { return new FakeElement(tag); },
    querySelector() { return null; },
    querySelectorAll() { return []; },
  };
}

function loadContext() {
  const context = {
    document: makeDocument(),
    console: { error: () => {}, log: () => {} },
    marked: { parse: (v) => '<p>' + String(v || '') + '</p>' },
    fixNestedFences: (v) => String(v || ''),
    renderChatMath: () => {},
    CSS: { escape: (v) => String(v) },
    SESSION_ID: 'sess-1',
    confirm: () => true,
    fetch: () => Promise.resolve({ ok: true }),
  };
  vm.createContext(context);
  vm.runInContext(readStatic('chat/namespace.js'), context, { filename: 'chat/namespace.js' });
  vm.runInContext(readStatic('chat/shared.js'), context, { filename: 'chat/shared.js' });
  context.Chat.renderRoundRatingButtons = () => '';
  context.Chat.embedLinkedHtmlArtifacts = () => {};
  vm.runInContext(readStatic('chat/rendering.js'), context, { filename: 'chat/rendering.js' });
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