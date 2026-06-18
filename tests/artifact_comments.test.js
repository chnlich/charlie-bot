const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ARTIFACT_COMMENTS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'artifact-comments.js'),
  'utf8'
);

function makeElement() {
  return {
    style: {},
    children: [],
    classList: {add() {}, remove() {}},
    appendChild(child) {
      this.children.push(child);
    },
    addEventListener() {},
    setAttribute() {},
    focus() {},
    querySelector() {
      return null;
    },
  };
}

function loadArtifactCommentsScript(pathname, framed = false) {
  const listeners = [];
  const window = {
    location: {pathname},
    innerWidth: 1024,
    innerHeight: 768,
    addEventListener(type, handler, options) {
      listeners.push({target: 'window', type, handler, options});
    },
    setTimeout() {},
  };
  window.self = window;
  window.parent = framed ? {} : window;

  const head = makeElement();
  const body = makeElement();
  const document = {
    head,
    body,
    createElement() {
      return makeElement();
    },
    addEventListener(type, handler, options) {
      listeners.push({target: 'document', type, handler, options});
    },
    querySelectorAll() {
      return [];
    },
  };

  const context = {
    window,
    document,
    console,
    Node: {DOCUMENT_POSITION_FOLLOWING: 4},
    fetch() {
      throw new Error('fetch should not run while loading artifact-comments.js');
    },
  };
  vm.createContext(context);
  vm.runInContext(ARTIFACT_COMMENTS_JS, context, {filename: 'artifact-comments.js'});
  return {window, head, listeners};
}

test('extractSessionIdFromPath parses session ids from artifact file paths', () => {
  const {window} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );

  const parse = window.__cbcExtractSessionIdFromPath;
  assert.equal(
    parse('/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'),
    'session-270'
  );
  assert.equal(
    parse('/files/%2Fdata%2Fhome%2Fchaoli%2F.charliebot%2Fsessions%2Fabc123%2Fartifacts%2Fplan.html'),
    'abc123'
  );
  assert.equal(parse('/files/data/home/chaoli/.charliebot/artifacts/plan.html'), null);
});

test('artifact comments script stays inert inside frames', () => {
  const {window, head, listeners} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html',
    true
  );

  assert.equal(window.__cbcExtractSessionIdFromPath, undefined);
  assert.equal(window.__cbcBuildBatchMessage, undefined);
  assert.equal(head.children.length, 0);
  assert.equal(listeners.length, 0);
});

test('buildBatchMessage combines pending comments into one numbered message', () => {
  const {window} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );

  const buildBatchMessage = window.__cbcBuildBatchMessage;
  assert.equal(typeof buildBatchMessage, 'function');

  const entries = [
    {quote: 'First quoted block text', context: 'Risks', comment: 'Looks risky'},
    {quote: 'Second quoted block text', context: 'Plan', comment: 'Add a step'},
  ];

  const message = buildBatchMessage(entries);

  assert.ok(message.includes('[Artifact comments \u00B7 '), 'header prefix');
  assert.ok(message.includes('] (2)'), 'header count');
  assert.ok(message.includes('1. \u25B8 Risks \u203A "First quoted block text"'), 'first numbered entry');
  assert.ok(message.includes('2. \u25B8 Plan \u203A "Second quoted block text"'), 'second numbered entry');
  assert.ok(message.includes('\u21B3 Looks risky'), 'first comment marker');
  assert.ok(message.includes('\u21B3 Add a step'), 'second comment marker');
});
