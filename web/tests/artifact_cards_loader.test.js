// ---------------------------------------------------------------------------
// The artifact card loader (web/templates/artifacts/artifact-cards.js): the chat page carries it in place of the
// module, and it fetches the module only from a rendered message that names an artifact page.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const LOADER_JS = fs.readFileSync(
  path.join(__dirname, '..', 'templates', 'artifacts', 'artifact-cards.js'), 'utf8');

// The rendered-message root the loader's forward reads: anchors whose hrefs name paths, and the message's own text.
function messageRoot({hrefs = [], text = ''} = {}) {
  const anchors = hrefs.map((href) => ({tagName: 'A', getAttribute: (name) => (name === 'href' ? href : null)}));
  return {
    querySelectorAll: (selector) => (selector === '[href*="artifacts/"]'
      ? anchors.filter((anchor) => anchor.getAttribute('href').indexOf('artifacts/') !== -1)
      : []),
    textContent: text,
  };
}

function buildContext({moduleLoaded = false, loadFails = false} = {}) {
  const loads = [];
  const fileLinkLoads = [];
  const processors = [];
  const embedded = [];
  const errors = [];
  const context = {
    console: {error(...args) { errors.push(args.join(' ')); }},
    // The chat page's file-link loader: the artifact module reads the helpers the module installs.
    loadFileLinks() {
      fileLinkLoads.push(loads.length);
      return Promise.resolve();
    },
    loadPageScripts(key, paths) {
      loads.push({key, paths});
      if (loadFails) return Promise.reject(new Error('the page script did not load: ' + paths[0]));
      // The fetched module installs its renderer and its own post-processor as it runs.
      context.Chat.embedLinkedHtmlArtifacts = (root) => embedded.push(root);
      return Promise.resolve();
    },
    Chat: {
      registerMessagePostProcessor(processor) { processors.push(processor); },
    },
  };
  if (moduleLoaded) context.Chat.embedLinkedHtmlArtifacts = (root) => embedded.push(root);
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(LOADER_JS, context, {filename: 'artifact-cards.js'});
  assert.equal(processors.length, 1, 'the loader registers one post-processor');
  return {context, loads, fileLinkLoads, processors, embedded, errors};
}

const settle = () => new Promise((resolve) => setImmediate(resolve));

test('a message that names no artifact page fetches nothing', async () => {
  const {loads, fileLinkLoads, processors, embedded} = buildContext();
  processors[0](messageRoot({text: 'plain prose with no link'}));
  await settle();
  assert.deepEqual(loads, []);
  assert.deepEqual(fileLinkLoads, [], 'the file-link module is not fetched either');
  assert.deepEqual(embedded, []);
});

test('prose that mentions the artifacts directory names no artifact page, so it fetches nothing', async () => {
  const {loads, processors} = buildContext();
  processors[0](messageRoot({text: 'store the run output in the artifacts/ directory'}));
  await settle();
  assert.deepEqual(loads, []);
});

test('an anchor that links a non-HTML artifact path names no artifact page', async () => {
  const {loads, processors} = buildContext();
  processors[0](messageRoot({hrefs: ['/absolute_filepath/home/x/artifacts/notes.txt']}));
  await settle();
  assert.deepEqual(loads, []);
});

test('a message whose anchor links an artifact page loads the module, then names the message for its card', async () => {
  const {loads, fileLinkLoads, processors, embedded} = buildContext();
  const root = messageRoot({hrefs: ['/absolute_filepath/home/x/artifacts/plan.html']});
  processors[0](root);
  await settle();
  assert.deepEqual(fileLinkLoads, [0], 'the file-link module loads first, so its helpers exist for the card module');
  assert.deepEqual(loads.map((load) => load.key), ['artifacts-cards']);
  assert.equal(loads[0].paths[0], '/static/features/artifacts/artifact-cards.js');
  assert.equal(loads[0].paths.length, 1);
  assert.deepEqual(embedded, [root], 'the message that triggered the load gets its card once the module arrives');
});

test('a message whose plain text names an artifact page loads the module too', async () => {
  const {loads, processors} = buildContext();
  processors[0](messageRoot({text: 'see artifacts/demo.html for the plan'}));
  await settle();
  assert.deepEqual(loads.map((load) => load.key), ['artifacts-cards']);
});

test('a module that is already loaded leaves every message to its own post-processor', async () => {
  const {loads, processors, embedded} = buildContext({moduleLoaded: true});
  processors[0](messageRoot({hrefs: ['/artifacts/plan.html']}));
  await settle();
  assert.deepEqual(loads, [], 'a loaded module needs no fetch');
  assert.deepEqual(embedded, [], 'the forward must not process a message the module processor owns');
});

test('a failed load reports its error and leaves the next message to try again', async () => {
  const {loads, processors, errors} = buildContext({loadFails: true});
  processors[0](messageRoot({hrefs: ['/artifacts/plan.html']}));
  await settle();
  assert.equal(loads.length, 1);
  assert.match(errors[0], /^artifact cards: .*the page script did not load/);
});
