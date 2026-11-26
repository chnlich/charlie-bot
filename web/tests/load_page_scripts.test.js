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
const RELOAD_GUARD_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'reload-guard.js'), 'utf8');
// pageAssetUrl is defined in config.js, which the page loads before utils.js. The harness runs that one function,
// so the loaders' URLs are the page's own.
const PAGE_ASSET_URL_JS = /^function pageAssetUrl\(path\) \{\n[\s\S]*?\n\}/m.exec(
  fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'config.js'), 'utf8'))[0];

function buildContext() {
  const appended = [];
  const reloads = [];
  const fetched = [];
  const context = {
    STATIC_ASSET_VERSION: 'v1',
    // The release the server answers a feature file with. A test sets it to a later release to model a stale page.
    serverRelease: 'v1',
    fetch: (url) => {
      fetched.push(url);
      return Promise.resolve({ok: true, headers: {get: () => context.serverRelease}});
    },
    reloadPastStoredShell: (cause) => reloads.push({cause, path: 'main'}),
    reloadAfterRefreshingShell: (cause) => reloads.push({cause, path: 'refresh'}),
    console: {error() {}},
    sessionStorage: {
      items: new Map(),
      getItem(key) { return this.items.has(key) ? this.items.get(key) : null; },
      setItem(key, value) { this.items.set(key, String(value)); },
    },
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
  vm.runInContext(PAGE_ASSET_URL_JS, context, {filename: 'config.js'});
  vm.runInContext(RELOAD_GUARD_JS, context, {filename: 'reload-guard.js'});
  vm.runInContext(UTILS_JS, context, {filename: 'utils.js'});
  return {context, appended, reloads, fetched};
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

test('a feature script checks its release with a fetch of the page version, then runs', async () => {
  const {context, appended, reloads, fetched} = buildContext();

  const loading = context.loadPageScripts('feature', ['/a.js']);
  await settle();
  assert.deepEqual(fetched, ['/a.js?v=v1']);
  assert.deepEqual(appended.map((script) => script.src), ['/a.js?v=v1']);
  appended[0].onload();
  await loading;
  assert.deepEqual(reloads, []);
});

test('a feature script from another release reloads the page with its cause and runs nothing', async () => {
  const {context, appended, reloads} = buildContext();
  context.serverRelease = 'v2';

  const loading = context.loadPageScripts('feature', ['/a.js']);
  await assert.rejects(loading, /the page runs another release; the script waits for its reload: \/a\.js/);
  assert.deepEqual(appended, [], 'the script does not run');
  assert.deepEqual(reloads, [{cause: 'feature_version', path: 'refresh'}]);
});

test('a feature script that finds another release again within the guard window reloads once, not again', async () => {
  const {context, reloads} = buildContext();
  context.serverRelease = 'v2';
  await assert.rejects(context.loadPageScripts('feature', ['/a.js']), /another release/);
  await assert.rejects(context.loadPageScripts('feature-again', ['/a.js']), /another release/);
  assert.deepEqual(reloads, [{cause: 'feature_version', path: 'refresh'}], 'the guard holds the second reload');
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

// The sidebar's interaction bundle (sidebar/namespace.js) on the same loader: one stylesheet and four scripts start
// in one batch, the move picker's file waits for session-actions.js, and a failed file retries alone.
const NAMESPACE_JS = fs.readFileSync(path.join(__dirname, '..', 'static', 'js', 'sidebar', 'namespace.js'), 'utf8');

function buildSidebarContext() {
  const {context, appended} = buildContext();
  const errors = [];
  context.console = {error: (message) => errors.push(message)};
  vm.runInContext(NAMESPACE_JS, context, {filename: 'namespace.js'});
  context.Sidebar.deferSidebarInteractions();
  return {context, appended, errors};
}

const nodeName = (node) => node.src || node.href;
const SIDEBAR = '/static/js/sidebar/';

test('the interaction bundle starts together, and the move picker waits for session-actions.js', async () => {
  const {context, appended} = buildSidebarContext();

  context.sidebarControl('archiveSession');
  await settle();
  assert.deepEqual(appended.map(nodeName), [
    '/static/css/sidebar-interactions.css?v=v1',
    SIDEBAR + 'row-menu.js?v=v1',
    SIDEBAR + 'modals.js?v=v1',
    SIDEBAR + 'session-actions.js?v=v1',
    SIDEBAR + 'session-menus.js?v=v1',
  ], 'one batch, and the picker is not requested while session-actions.js is in flight');

  appended[3].onload();
  await settle();
  assert.equal(nodeName(appended[5]), SIDEBAR + 'session-move.js?v=v1');
  assert.equal(appended.length, 6);
});

test('a failed interaction file retries alone, and the files that loaded never reload', async () => {
  const {context, appended, errors} = buildSidebarContext();

  context.sidebarControl('archiveSession');
  await settle();
  appended.filter((node) => nodeName(node) !== SIDEBAR + 'modals.js?v=v1').forEach((node) => node.onload());
  appended.find((node) => nodeName(node) === SIDEBAR + 'modals.js?v=v1').onerror();
  await settle();
  assert.deepEqual(errors, ['the sidebar interaction scripts did not load: the page script did not load: '
    + SIDEBAR + 'modals.js']);

  context.sidebarControl('archiveSession');
  await settle();
  assert.deepEqual(appended.slice(6).map(nodeName), [SIDEBAR + 'modals.js?v=v1'],
    'the retry asks for the failed file only');
});

test('a failed interaction stylesheet retries alone while its scripts are still loading', async () => {
  const {context, appended, errors} = buildSidebarContext();

  context.sidebarControl('archiveSession');
  await settle();
  appended[0].onerror();
  await settle();
  assert.deepEqual(errors, ['the page stylesheet did not load: /static/css/sidebar-interactions.css']);

  context.sidebarControl('archiveSession');
  await settle();
  assert.deepEqual(appended.slice(5).map(nodeName), ['/static/css/sidebar-interactions.css?v=v1'],
    'the retry asks for the failed stylesheet only');
});
