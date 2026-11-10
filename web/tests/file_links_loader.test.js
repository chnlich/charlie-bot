'use strict';
// File-link loader (templates/js/chat/file-links.js): the document carries this loader in the module's place. A
// rendered message that names a file path starts the download, and the roots that asked for it are marked when the
// module lands. A message with no file path asks for nothing. A failed download is reported and the next need
// retries, with the root that asked still waiting for the module.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

const ROOT = path.join(__dirname, '..');
const readLoader = () => fs.readFileSync(path.join(ROOT, 'templates/js/chat/file-links.js'), 'utf8');
const flush = () => new Promise((resolve) => setImmediate(resolve));

// One rendered prose element: its text, its anchors and whether it is the live streaming bubble.
function prose({text = '', hrefs = [], streaming = false} = {}) {
  return {
    id: streaming ? 'streaming-msg' : '',
    closest: (selector) => (streaming && selector === '#streaming-msg' ? prose() : null),
    textContent: text,
    querySelectorAll: (selector) => (selector === 'a[href]'
      ? hrefs.map((href) => ({getAttribute: (name) => (name === 'href' ? href : null)}))
      : []),
  };
}

function messageRoot(proses) {
  return {querySelectorAll: (selector) => (selector === '.prose-msg' ? proses : [])};
}

function loadPage() {
  const downloadCalls = [];
  const marked = [];
  const errors = [];
  let resolveLoad = null;
  let rejectLoad = null;
  const context = {
    loadPageScripts(key, paths) {
      downloadCalls.push({key, paths});
      return new Promise((resolve, reject) => {
        resolveLoad = resolve;
        rejectLoad = reject;
      });
    },
    console: {error: (message) => errors.push(message)},
    URL,
    window: {location: {href: 'http://app.test/', origin: 'http://app.test'}},
    Chat: {},
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(readLoader(), context, {filename: 'templates/js/chat/file-links.js'});
  // The module's own definition takes the name when it lands; every call it receives is recorded.
  const land = () => {
    context.Chat.markFileLinks = (root) => marked.push(root);
  };
  return {
    context, downloadCalls, marked, errors, land,
    resolveLoad: () => resolveLoad(),
    rejectLoad: (error) => rejectLoad(error),
  };
}

test('a message with no file path fetches nothing', () => {
  const rig = loadPage();
  rig.context.Chat.markFileLinks(messageRoot([prose({text: 'plain prose about the cache', hrefs: ['/?session=abc']})]));
  assert.deepEqual(rig.downloadCalls, [], 'the document loads no file-link code for a message without a path');
});

test('a message whose text names a file-server path starts the module download', () => {
  const rig = loadPage();
  rig.context.Chat.markFileLinks(messageRoot([
    prose({text: 'read /absolute_filepath/home/x/report.txt for the numbers'}),
  ]));
  assert.equal(rig.downloadCalls.length, 1);
  assert.equal(rig.downloadCalls[0].key, 'chat-file-links');
  assert.deepEqual(Array.from(rig.downloadCalls[0].paths), ['/static/js/chat/file-links.js']);
});

test('a message whose anchor is a same-origin path starts the download', () => {
  const rig = loadPage();
  rig.context.Chat.markFileLinks(messageRoot([prose({hrefs: ['/token-usage']})]));
  assert.equal(rig.downloadCalls.length, 1, 'the module probes a same-origin file anchor for a missing file');
});

test('a message whose anchor is a fragment, a query or another origin asks for nothing', () => {
  const rig = loadPage();
  rig.context.Chat.markFileLinks(messageRoot([
    prose({hrefs: ['#section', '?tab=1', 'https://example.invalid/x', '//other.invalid/x']}),
  ]));
  assert.deepEqual(rig.downloadCalls, [], 'another origin\'s anchor is another server\'s file');
});

test('a message whose text names another origin\'s file-server path asks for nothing', () => {
  const rig = loadPage();
  rig.context.Chat.markFileLinks(messageRoot([
    prose({text: 'https://other.invalid/absolute_filepath/home/x/report.txt'}),
  ]));
  assert.deepEqual(rig.downloadCalls, [], 'the scanner leaves another origin\'s URL as written');
});

test('a message whose text names this page\'s file server by its full URL starts the download', () => {
  const rig = loadPage();
  rig.context.Chat.markFileLinks(messageRoot([
    prose({text: 'http://app.test/absolute_filepath/home/x/report.txt'}),
  ]));
  assert.equal(rig.downloadCalls.length, 1, 'the scanner resolves the full URL to this page\'s origin');
});

test('the streaming bubble asks for nothing', () => {
  const rig = loadPage();
  rig.context.Chat.markFileLinks(messageRoot([
    prose({text: '/absolute_filepath/home/x/live.txt', streaming: true}),
  ]));
  assert.deepEqual(rig.downloadCalls, []);
});

test('the roots that asked are marked when the module lands, and later calls go to the module', async () => {
  const rig = loadPage();
  const first = messageRoot([prose({text: '/absolute_filepath/home/x/a.txt'})]);
  rig.context.Chat.markFileLinks(first);
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.deepEqual(rig.marked, [first], 'the root that asked is marked once the module arrives');
  const second = messageRoot([prose({text: 'nothing'})]);
  rig.context.Chat.markFileLinks(second);
  assert.deepEqual(rig.marked, [first, second], 'a later call reaches the module even with no file path');
  assert.equal(rig.downloadCalls.length, 1, 'the module is fetched once');
});

test('a failed download rejects the load, so a caller that needs the module\'s helpers stops', async () => {
  const rig = loadPage();
  const loading = rig.context.loadFileLinks();
  rig.rejectLoad(new Error('offline'));
  await assert.rejects(loading, /offline/);
  assert.match(rig.errors[0], /did not load/);
});

test('a failed download is reported and the next need retries', async () => {
  const rig = loadPage();
  const root = messageRoot([prose({text: '/absolute_filepath/home/x/a.txt'})]);
  rig.context.Chat.markFileLinks(root);
  rig.rejectLoad(new Error('offline'));
  await flush();
  assert.match(rig.errors[0], /did not load/);
  rig.context.Chat.markFileLinks(root);
  assert.equal(rig.downloadCalls.length, 2, 'the next need retries the download');
  assert.deepEqual(rig.marked, [], 'the retry marks nothing until the module lands');
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.deepEqual(rig.marked, [root], 'the root that asked is marked when the retry lands');
});
