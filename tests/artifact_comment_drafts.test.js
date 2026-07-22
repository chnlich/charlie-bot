const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ARTIFACT_COMMENTS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'artifact-comments.js'),
  'utf8'
);

const ARTIFACT_PATH = '/files/home/user/.charliebot/sessions/sess-draft/artifacts/plan.html';
const DRAFT_KEY = 'cbc-draft:' + ARTIFACT_PATH;

// Objects created inside a vm context have a different Object.prototype than the
// test's outer context, so assert.deepStrictEqual rejects them as non-reference-equal.
// Compare JSON snapshots instead, and use primitive assert.equal for fields.
function jsonEqual(actual, expected) {
  assert.equal(JSON.stringify(actual), JSON.stringify(expected));
}

// Minimal element stub mirroring tests/artifact_comments.test.js (no jsdom).
function makeElement() {
  return {
    _textContent: '',
    _innerHTML: '',
    _listeners: {},
    attributes: {},
    style: {},
    children: [],
    childNodes: [],
    nodeType: 1,
    tagName: 'DIV',
    className: '',
    parentNode: null,
    parentElement: null,
    display: 'block',
    get textContent() {
      if (this.childNodes.length === 0) return this._textContent;
      return this.childNodes.map((node) => node.textContent || '').join('');
    },
    set textContent(value) {
      this._textContent = String(value);
      this.childNodes = [];
      this.children = [];
      this._innerHTML = '';
    },
    get innerHTML() {
      return this._innerHTML;
    },
    set innerHTML(value) {
      this._innerHTML = String(value);
      if (this._innerHTML === '') {
        this.children = [];
        this.childNodes = [];
      }
    },
    get innerText() {
      return this.textContent;
    },
    classList: {add() {}, remove() {}},
    appendChild(child) {
      this.children.push(child);
      this.childNodes.push(child);
      child.parentNode = this;
      child.parentElement = this;
      return child;
    },
    replaceChild(next, prev) {
      const index = this.children.indexOf(prev);
      assert.notEqual(index, -1, 'replaceChild target exists');
      this.children[index] = next;
      const nodeIndex = this.childNodes.indexOf(prev);
      if (nodeIndex !== -1) this.childNodes[nodeIndex] = next;
      next.parentNode = this;
      next.parentElement = this;
      prev.parentNode = null;
      prev.parentElement = null;
      return prev;
    },
    addEventListener(type, handler) {
      if (!this._listeners[type]) this._listeners[type] = [];
      this._listeners[type].push(handler);
    },
    setAttribute(name, value = '') {
      this.attributes[name] = String(value);
    },
    focus() {},
    select() {},
    querySelector(selector) {
      if (!selector.startsWith('.')) return null;
      const targetClass = selector.slice(1);
      const stack = this.children.slice();
      while (stack.length > 0) {
        const child = stack.shift();
        const classes = String(child.className || '').split(/\s+/);
        if (classes.includes(targetClass)) return child;
        stack.push(...child.children);
      }
      return null;
    },
  };
}

function makeStorage() {
  const store = new Map();
  return {
    store,
    getItem(key) {
      return store.has(key) ? store.get(key) : null;
    },
    setItem(key, value) {
      store.set(key, String(value));
    },
    removeItem(key) {
      store.delete(key);
    },
  };
}

function silentConsole() {
  return {warn() {}, error() {}, log() {}};
}

function loadScript(opts = {}) {
  const storage = opts.storage || makeStorage();
  const pathname = opts.pathname || ARTIFACT_PATH;
  const window = {
    location: {pathname, hash: opts.hash || ''},
    innerWidth: 1024,
    innerHeight: 768,
    addEventListener() {},
    setTimeout() {},
    clearTimeout() {},
    getComputedStyle(el) {
      return {display: el.display || 'block'};
    },
  };
  window.self = window;
  window.parent = opts.framed ? {} : window;

  const head = makeElement();
  const body = makeElement();
  const document = {
    head,
    body,
    createElement() {
      return makeElement();
    },
    addEventListener() {},
    querySelectorAll(selector) {
      if (selector === '.__cbc-shortcuts') {
        return body.children.filter((child) => child.className === '__cbc-shortcuts');
      }
      return [];
    },
  };

  const context = {
    window,
    document,
    console: opts.console || silentConsole(),
    Node: {DOCUMENT_POSITION_FOLLOWING: 4, DOCUMENT_POSITION_PRECEDING: 2},
    sessionStorage: storage,
    fetch:
      opts.fetch ||
      function () {
        throw new Error('fetch should not run while loading artifact-comments.js');
      },
  };
  vm.createContext(context);
  vm.runInContext(ARTIFACT_COMMENTS_JS, context, {filename: 'artifact-comments.js'});
  return {context, window, head, body, storage};
}

function findChildByClass(parent, className) {
  return parent.children.find((child) => child.className === className);
}

function clickElement(el) {
  assert.ok(el && el._listeners.click && el._listeners.click.length > 0, 'click listener exists');
  return el._listeners.click[0]({preventDefault() {}, stopPropagation() {}});
}

function flush() {
  return new Promise((resolve) => setImmediate(resolve));
}

async function flushPromises(times = 6) {
  for (let i = 0; i < times; i++) await flush();
}

// ---------------------------------------------------------------------------
// Pure helpers: draftKey / serializeDraft / deserializeDraft
// ---------------------------------------------------------------------------

test('draftKey uses the cbc-draft: prefix with the artifact abs path', () => {
  const {window} = loadScript();
  assert.equal(window.__cbcDraftKey(ARTIFACT_PATH), DRAFT_KEY);
  assert.equal(window.__cbcDraftKey(''), 'cbc-draft:');
  assert.equal(window.__cbcDraftKey(undefined), 'cbc-draft:');
});

test('serializeDraft strips el and keeps kind/quote/context/comment, coercing kind', () => {
  const {window} = loadScript();
  const serialize = window.__cbcSerializeDraft;
  const out = serialize([
    {kind: 'block', el: makeElement(), quote: 'q1', context: 'c1', comment: 'cm1'},
    {kind: 'improve', el: null, quote: '', context: 'Imp', comment: 'p'},
    {kind: 'unknown', el: makeElement(), quote: 'q2', context: 'c2', comment: 'cm2'},
  ]);
  jsonEqual(out, [
    {kind: 'block', quote: 'q1', context: 'c1', comment: 'cm1'},
    {kind: 'improve', quote: '', context: 'Imp', comment: 'p'},
    {kind: 'block', quote: 'q2', context: 'c2', comment: 'cm2'},
  ]);
  assert.equal('el' in out[0], false, 'el is stripped from serialized entries');
  assert.deepEqual(Object.keys(out[0]), ['kind', 'quote', 'context', 'comment']);
});

test('deserializeDraft restores entries with el null and coerces fields', () => {
  const {window} = loadScript();
  const deserialize = window.__cbcDeserializeDraft;
  const out = deserialize(
    JSON.stringify([
      {kind: 'block', quote: 'q', context: 'c', comment: 'cm'},
      {kind: 'improve', quote: '', context: 'Imp', comment: 'p'},
    ])
  );
  assert.equal(out.length, 2);
  assert.equal(out[0].el, null, 'block entry el downgraded to null');
  assert.equal(out[0].kind, 'block');
  assert.equal(out[0].quote, 'q');
  assert.equal(out[0].context, 'c');
  assert.equal(out[0].comment, 'cm');
  assert.equal(out[1].el, null);
  assert.equal(out[1].kind, 'improve');
});

test('deserializeDraft returns empty for null/garbage and filters bad entries', () => {
  const {window} = loadScript();
  const deserialize = window.__cbcDeserializeDraft;
  jsonEqual(deserialize(null), []);
  jsonEqual(deserialize(undefined), []);
  jsonEqual(deserialize('not json'), []);
  jsonEqual(deserialize(JSON.stringify(null)), []);
  jsonEqual(deserialize(JSON.stringify('string')), []);
  jsonEqual(deserialize(JSON.stringify([{kind: 'block', quote: 'q'}])), [
    {kind: 'block', el: null, quote: 'q', context: '', comment: ''},
  ]);
  jsonEqual(
    deserialize(JSON.stringify([{kind: 'block', quote: 'a'}, 'bad', null, {kind: 'improve', comment: 'b'}])),
    [
      {kind: 'block', el: null, quote: 'a', context: '', comment: ''},
      {kind: 'improve', el: null, quote: '', context: '', comment: 'b'},
    ]
  );
});

// ---------------------------------------------------------------------------
// save/load/clear round-trip through a sessionStorage stub
// ---------------------------------------------------------------------------

test('save/load/clear round-trip: empty -> save -> restore (el:null) -> clear', () => {
  const {window, storage} = loadScript();
  jsonEqual(window.__cbcLoadDraft(ARTIFACT_PATH), [], 'empty when nothing saved');

  const entries = [
    {kind: 'block', el: makeElement(), quote: 'q1', context: 'c1', comment: 'cm1'},
    {kind: 'improve', el: null, quote: '', context: 'Imp', comment: 'p'},
  ];
  window.__cbcSaveDraft(entries, ARTIFACT_PATH);

  assert.equal(storage.store.has(DRAFT_KEY), true, 'save writes the keyed entry');
  const raw = storage.store.get(DRAFT_KEY);
  assert.equal(JSON.parse(raw)[0].el, undefined, 'el stripped before storage');

  const restored = window.__cbcLoadDraft(ARTIFACT_PATH);
  assert.equal(restored.length, 2);
  assert.equal(restored[0].el, null, 'block entry el downgraded to null on restore');
  assert.equal(restored[0].kind, 'block');
  assert.equal(restored[0].quote, 'q1');
  assert.equal(restored[0].comment, 'cm1');
  assert.equal(restored[1].kind, 'improve');

  window.__cbcClearDraft(ARTIFACT_PATH);
  assert.equal(storage.store.has(DRAFT_KEY), false, 'clear removes the key');
  jsonEqual(window.__cbcLoadDraft(ARTIFACT_PATH), [], 'empty after clear');
});

test('restored el:null entries do not break buildBatchMessage output format', () => {
  const {window} = loadScript();
  const restored = window.__cbcDeserializeDraft(
    JSON.stringify([
      {kind: 'block', quote: 'q1', context: 'c1', comment: 'cm1'},
      {kind: 'improve', quote: '', context: 'Imp', comment: 'p'},
    ])
  );
  const message = window.__cbcBuildBatchMessage(restored);
  assert.ok(message.includes('[Artifact comments \u00B7 ' + ARTIFACT_PATH + '] (2)'));
  assert.ok(message.includes('1. \u25B8 c1 \u203A "q1"'));
  assert.ok(message.includes('\u21B3 cm1'));
  assert.ok(message.includes('2. \u25B8 Imp'));
  assert.ok(message.includes('\u21B3 p'));
});

// ---------------------------------------------------------------------------
// send flow: success clears the draft, failure keeps it
// ---------------------------------------------------------------------------

function sessionFetch(ok) {
  return async (url) => {
    if (url === '/api/sessions/sess-draft') {
      return {ok: true, status: 200, async json() { return {name: 'Draft Session'}; }};
    }
    if (url === '/api/chat/sess-draft/message') {
      return ok ? {ok: true, status: 200} : {ok: false, status: 500};
    }
    throw new Error('unexpected fetch: ' + url);
  };
}

async function addShortcutAndSend(fetch, storage) {
  const res = loadScript({storage, fetch, console: silentConsole()});
  const shortcuts = findChildByClass(res.body, '__cbc-shortcuts');
  const shortcutBtn = findChildByClass(shortcuts, '__cbc-shortcut');
  clickElement(shortcutBtn);
  assert.equal(res.storage.store.has(DRAFT_KEY), true, 'add persisted the draft');

  const tray = findChildByClass(res.body, '__cbc-tray');
  const actions = findChildByClass(tray, '__cbc-tray-actions');
  const sendBtn = findChildByClass(actions, '__cbc-tray-send');
  await clickElement(sendBtn);
  await flushPromises();
  return res;
}

test('successful send clears the draft', async () => {
  const res = await addShortcutAndSend(sessionFetch(true), makeStorage());
  assert.equal(res.storage.store.has(DRAFT_KEY), false, 'successful send clears the draft');
});

test('failed send keeps the draft', async () => {
  const res = await addShortcutAndSend(sessionFetch(false), makeStorage());
  assert.equal(res.storage.store.has(DRAFT_KEY), true, 'failed send keeps the draft');
});

// ---------------------------------------------------------------------------
// storage unavailable / quota -> degrade silently, no crash
// ---------------------------------------------------------------------------

test('storage failures degrade silently without crashing', () => {
  const broken = {
    getItem() {
      throw new Error('quota');
    },
    setItem() {
      throw new Error('quota');
    },
    removeItem() {
      throw new Error('quota');
    },
  };
  const {window} = loadScript({storage: broken, console: silentConsole()});
  jsonEqual(window.__cbcLoadDraft(ARTIFACT_PATH), [], 'load returns empty on storage error');
  assert.doesNotThrow(() =>
    window.__cbcSaveDraft(
      [{kind: 'block', el: null, quote: 'q', context: 'c', comment: 'cm'}],
      ARTIFACT_PATH
    )
  );
  assert.doesNotThrow(() => window.__cbcClearDraft(ARTIFACT_PATH));
});

test('script loads with no sessionStorage global at all (try/catch guards access)', () => {
  // No sessionStorage on the context — every access must be guarded.
  const ctx = {
    window: {
      location: {pathname: ARTIFACT_PATH, hash: ''},
      innerWidth: 1024,
      innerHeight: 768,
      addEventListener() {},
      setTimeout() {},
      clearTimeout() {},
      getComputedStyle(el) {
        return {display: el.display || 'block'};
      },
    },
    document: {
      head: makeElement(),
      body: makeElement(),
      createElement() {
        return makeElement();
      },
      addEventListener() {},
      querySelectorAll() {
        return [];
      },
    },
    console: silentConsole(),
    Node: {DOCUMENT_POSITION_FOLLOWING: 4, DOCUMENT_POSITION_PRECEDING: 2},
    fetch() {
      throw new Error('unused');
    },
  };
  ctx.window.self = ctx.window;
  ctx.window.parent = ctx.window;
  vm.createContext(ctx);
  assert.doesNotThrow(() =>
    vm.runInContext(ARTIFACT_COMMENTS_JS, ctx, {filename: 'artifact-comments.js'})
  );
  assert.equal(typeof ctx.window.__cbcLoadDraft, 'function');
  jsonEqual(ctx.window.__cbcLoadDraft(ARTIFACT_PATH), []);
});

// ---------------------------------------------------------------------------
// tray init restores a pre-seeded draft with el null
// ---------------------------------------------------------------------------

test('tray restores pending draft on init with el null', () => {
  const storage = makeStorage();
  storage.setItem(
    DRAFT_KEY,
    JSON.stringify([
      {kind: 'block', quote: 'restored quote', context: 'restored ctx', comment: 'restored cm'},
      {kind: 'improve', quote: '', context: 'Imp', comment: 'p'},
    ])
  );
  const {body} = loadScript({
    storage,
    fetch: async (url) => {
      if (url === '/api/sessions/sess-draft') {
        return {ok: true, status: 200, async json() { return {name: 'Draft Session'}; }};
      }
      throw new Error('unexpected fetch: ' + url);
    },
  });

  const tray = findChildByClass(body, '__cbc-tray');
  const header = findChildByClass(tray, '__cbc-tray-header');
  assert.ok(
    header.textContent.startsWith('Pending comments (2)'),
    'restored 2 entries on init: ' + header.textContent
  );
  const list = findChildByClass(tray, '__cbc-tray-list');
  assert.equal(list.children.length, 2, 'two tray items rendered from the restored draft');
});
