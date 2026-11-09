// ---------------------------------------------------------------------------
// loadPageScripts (utils.js): the feature scripts a page loads on first use.
// One promise per key, and a retry after a failed file starts at that file,
// because a script that already ran must not run twice: its registrations
// (controls, separator buttons) would be duplicates.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const UTILS_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'utils.js'), 'utf8');

function buildContext() {
  const appended = [];
  const context = {
    STATIC_ASSET_VERSION: 'v1',
    console: {error() {}},
    document: {
      createElement: () => ({}),
      head: {appendChild(script) { appended.push(script); }},
    },
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(UTILS_JS, context, {filename: 'utils.js'});
  return {context, appended};
}

const settle = () => new Promise((resolve) => setImmediate(resolve));

test('a failed page-script load retries from the failed file, never re-running an earlier one', async () => {
  const {context, appended} = buildContext();

  const first = context.loadPageScripts('feature', ['/a.js', '/b.js']);
  await settle();
  assert.deepEqual(appended.map((script) => script.src), ['/a.js?v=v1']);
  appended[0].onload();
  await settle();
  assert.deepEqual(appended.map((script) => script.src), ['/a.js?v=v1', '/b.js?v=v1']);
  appended[1].onerror();
  await assert.rejects(first, /the page script did not load: \/b\.js/);

  const second = context.loadPageScripts('feature', ['/a.js', '/b.js']);
  await settle();
  assert.deepEqual(appended.map((script) => script.src), ['/a.js?v=v1', '/b.js?v=v1', '/b.js?v=v1'],
    'the retry asks for the failed file only');
  appended[2].onload();
  await second;
});
