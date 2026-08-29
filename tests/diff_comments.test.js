const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const DIFF_COMMENTS_JS = readStatic('diff_comments.js');

function makeElement() {
  return {
    _innerHTML: '',
    _listeners: {},
    children: [],
    className: '',
    dataset: {},
    disabled: false,
    style: {},
    textContent: '',
    value: '',
    classList: {
      add() {},
      remove() {},
      contains() { return false; },
    },
    append(...children) {
      for (const child of children) this.appendChild(child);
    },
    appendChild(child) {
      this.children.push(child);
      child.parentNode = this;
      return child;
    },
    addEventListener(type, handler) {
      this._listeners[type] = handler;
    },
    setAttribute() {},
    querySelectorAll() { return []; },
    remove() {},
    get innerHTML() { return this._innerHTML; },
    set innerHTML(value) {
      this._innerHTML = String(value);
      if (value === '') this.children = [];
    },
  };
}

function loadDiffCommentsScript() {
  const head = makeElement();
  const body = makeElement();
  const output = makeElement();
  const window = {
    __cbdiffPage: {getComparison() { return null; }},
    location: {search: ''},
    addEventListener() {},
    confirm() { return true; },
    setTimeout() {},
  };
  const document = {
    head,
    body,
    getElementById(id) {
      assert.equal(id, 'diff-output');
      return output;
    },
    createElement() { return makeElement(); },
    createTextNode(text) { return {textContent: text}; },
    addEventListener() {},
  };
  const context = {
    URLSearchParams,
    console,
    document,
    fetch: async (url) => {
      assert.equal(url, '/api/sessions/');
      return {ok: true, status: 200, async json() { return []; }};
    },
    setTimeout,
    window,
  };
  vm.createContext(context);
  vm.runInContext(DIFF_COMMENTS_JS, context, {filename: 'diff_comments.js'});
  return window;
}

function context() {
  return {
    repo: '/workspace/example',
    base: 'main',
    head: 'feature',
    mode: 'three-dot',
    headSha: 'abcdef0123456789',
  };
}

test('buildBatchMessage emits the exact sorted batch scaffold', () => {
  const window = loadDiffCommentsScript();
  const build = window.__cbdcBuildBatchMessage;
  const cappedQuote = 'x'.repeat(405);
  const message = build(context(), [
    {
      filePath: 'z.js',
      side: 'old',
      startLine: 10,
      endLine: 10,
      quote: cappedQuote,
      comment: 'Keep this behavior',
      isSuggestion: false,
    },
    {
      filePath: 'a.js',
      side: 'new',
      startLine: 2,
      endLine: 3,
      quote: 'old\ncode',
      comment: '  replacement\n\nlast',
      isSuggestion: true,
    },
  ], 'Batch context');

  assert.equal(
    message,
    [
      '[Diff comments · /workspace/example · main..feature @ abcdef0] (2)',
      'overall: Batch context',
      '',
      '1. a.js:2-3 (new) [suggestion]',
      '   ▸ "old',
      '      code"',
      '   ↳ suggested replacement:',
      '        replacement',
      '      ',
      '      last',
      '',
      '2. z.js:10 (old)',
      `   ▸ "${'x'.repeat(400)}"`,
      '   ↳ Keep this behavior',
    ].join('\n')
  );
  assert.ok(message.indexOf('1. a.js') < message.indexOf('2. z.js'), 'entries sort by file path');
  assert.ok(message.includes('\n\n2. z.js'), 'entries have one blank line between them');
  assert.equal(message.includes('x'.repeat(401)), false, 'quotes are capped at 400 characters');
});

test('buildBatchMessage omits the overall line when it is empty', () => {
  const window = loadDiffCommentsScript();
  const message = window.__cbdcBuildBatchMessage(context(), [{
    filePath: 'a.js',
    side: 'new',
    startLine: 7,
    endLine: 7,
    quote: 'line',
    comment: 'Comment',
    isSuggestion: false,
  }], '   ');

  assert.equal(message.includes('overall:'), false);
  assert.ok(message.startsWith(
    '[Diff comments · /workspace/example · main..feature @ abcdef0] (1)\n\n1. a.js:7 (new)'
  ));
});

test('resolveTargetSession prioritizes the query param and falls back after a 404', () => {
  const window = loadDiffCommentsScript();
  const resolve = window.__cbdcResolveTargetSession;

  assert.equal(resolve('query-session', 'dropdown-session', 'found'), 'query-session');
  assert.equal(resolve('query-session', 'dropdown-session', 'loading'), null);
  assert.equal(resolve('query-session', 'dropdown-session', 'missing'), 'dropdown-session');
  assert.equal(resolve(null, 'dropdown-session', 'none'), 'dropdown-session');
});

test('mostRecentSessionId chooses the latest updated session', () => {
  const window = loadDiffCommentsScript();
  assert.equal(window.__cbdcMostRecentSessionId([
    {id: 'older', updated_at: '2026-01-01T00:00:00Z'},
    {id: 'newer', updated_at: '2026-02-01T00:00:00Z'},
  ]), 'newer');
  assert.equal(window.__cbdcMostRecentSessionId([]), null);
});
