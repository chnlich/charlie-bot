// ---------------------------------------------------------------------------
// loadPageScripts and loadPageStyles (utils.js): the feature scripts and stylesheets a page loads on first use.
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
    // markdown-renderer.js defines this before utils.js runs on the page; the math renderer's own download is the
    // one registration here.
    registerChatMathLoader() {},
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

test('a file on another origin keeps its own URL; a same-origin one takes the page version', async () => {
  const {context, appended} = buildContext();

  const scripts = context.loadPageScripts('feature', ['https://cdn.example.com/lib@1.0.0/x.js', '/b.js']);
  await settle();
  appended[0].onload();
  await settle();
  appended[1].onload();
  await scripts;
  const styles = context.loadPageStyles('feature', ['https://cdn.example.com/lib@1.0.0/x.css']);
  await settle();
  appended[2].onload();
  await styles;
  assert.deepEqual(appended.map((node) => node.src || node.href), [
    'https://cdn.example.com/lib@1.0.0/x.js',
    '/b.js?v=v1',
    'https://cdn.example.com/lib@1.0.0/x.css',
  ]);
});

test('a featured stylesheet loads once per key, and a failed link retries', async () => {
  const {context, appended} = buildContext();

  const first = context.loadPageStyles('feature', ['/a.css']);
  const joined = context.loadPageStyles('feature', ['/a.css']);
  await settle();
  assert.equal(appended.length, 1, 'the second need joins the first load');
  assert.equal(appended[0].rel, 'stylesheet');
  assert.equal(appended[0].href, '/a.css?v=v1');
  appended[0].onerror();
  await assert.rejects(first, /the page stylesheet did not load: \/a\.css/);
  await assert.rejects(joined, /the page stylesheet did not load: \/a\.css/);

  const retry = context.loadPageStyles('feature', ['/a.css']);
  await settle();
  assert.deepEqual(appended.map((link) => link.href), ['/a.css?v=v1', '/a.css?v=v1'],
    'a stylesheet re-inserted after a failure costs nothing');
  appended[1].onload();
  await retry;
});
