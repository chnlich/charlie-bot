// The chat page fetches the explain module after its document, so the module lands on rounds whose button groups
// were built before it registered anything. These tests hold the two cases: a parsed document, where the module adds
// its button to the rendered rounds and reads the shown session's statuses itself, and a document still parsing,
// where the page's own render adds the button and the module's session-data-load listener reads the statuses.
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readPackageStatic} = require('./read_static');

const EXPLAIN_JS = readPackageStatic('features', 'explain', 'explain.js');

function load({readyState = 'complete', sessionId = 'sid-1'} = {}) {
  const inserted = [];
  const repainted = [];
  const statusFetches = [];
  const group = {
    dataset: {roundEventIndex: '7'},
    querySelector: () => null,
    insertAdjacentHTML(position, html) {
      inserted.push({position, html});
    },
  };
  const context = {
    Chat: {
      escapeHtmlAttr: (value) => String(value == null ? '' : value),
      escapeJsSingleQuoted: (value) => String(value).replace(/\\/g, '\\\\').replace(/'/g, "\\'"),
      registerSeparatorButton() {},
      registerSeparatorPanel: () => ({}),
      repaintRoundButtons: (root) => repainted.push(root),
      wire: (globals, chatOnly) => Object.assign(context, globals, chatOnly || {}),
    },
    document: {
      readyState,
      addEventListener() {},
      // The rendered round group is what a parsed document holds; a parsing one holds only its button groups' absence.
      querySelectorAll: (selector) => (readyState !== 'loading' && selector === '.round-buttons[data-round-event-index]'
        ? [group] : []),
    },
    registerWsMessageHandler() {},
    viewRequestSignal: () => undefined,
    fetch: async (url) => {
      statusFetches.push(url);
      return {ok: true, json: async () => ({})};
    },
    console: {error: () => {}},
    SESSION_ID: sessionId,
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(EXPLAIN_JS, context, {filename: 'explain.js'});
  return {inserted, repainted, statusFetches};
}

test('a module that lands on a parsed document adds its button to the rendered rounds', () => {
  const {inserted, repainted} = load();
  assert.equal(inserted.length, 1);
  assert.equal(inserted[0].position, 'afterbegin');
  assert.match(inserted[0].html, /class="explain-toggle/);
  assert.match(inserted[0].html, /explainSession\('sid-1', 7\)/);
  assert.equal(repainted.length, 1);
});

test('a module that lands on a parsed document reads the shown session statuses itself', () => {
  const {statusFetches} = load();
  assert.deepEqual(statusFetches, ['/api/sessions/sid-1/explain/status']);
});

test('a module that lands while the document still parses leaves both to the page', () => {
  const {inserted, repainted, statusFetches} = load({readyState: 'loading'});
  assert.equal(inserted.length, 0);
  assert.equal(repainted.length, 0);
  assert.equal(statusFetches.length, 0);
});
