const assert = require('node:assert/strict');
const test = require('node:test');

const {dockOf, findChildByClass, makeElement, clickElement, flushPromises} =
  require('./artifact_comments_dom_stub');

const {loadArtifactCommentsContext} = require('./artifact_comments_vm_context');
const {REC_TEXT, makeForkPage} = require('./artifact_fork_fixture');
const {SESSIONS_ROOT} = require('./sessions_root_stub');

const ARTIFACT_PATH = '/absolute_filepath' + SESSIONS_ROOT + '/sess-draft/artifacts/plan.html';
const DRAFT_KEY = 'cbc-draft:' + ARTIFACT_PATH;

// Objects created inside a vm context have a different Object.prototype than the
// test's outer context, so assert.deepStrictEqual rejects them as non-reference-equal.
// Compare JSON snapshots instead, and use primitive assert.equal for fields.
function jsonEqual(actual, expected) {
  assert.equal(JSON.stringify(actual), JSON.stringify(expected));
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
  const {context, window, body} = loadArtifactCommentsContext({
    pathname: opts.pathname || ARTIFACT_PATH,
    hash: opts.hash,
    framed: opts.framed,
    console: opts.console || silentConsole(),
    fetch: opts.fetch,
    sessionStorage: storage,
    serverSessionId: 'sess-draft',
    nodePreceding: true,
    bodyChildren: opts.bodyChildren,
  });
  return {context, window, body, storage};
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

test('serializeDraft strips el and keeps kind/quote/context/comment/occurrence, coercing kind', () => {
  const {window} = loadScript();
  const serialize = window.__cbcSerializeDraft;
  const out = serialize([
    {kind: 'block', el: makeElement(), quote: 'q1', context: 'c1', comment: 'cm1'},
    {kind: 'improve', el: null, quote: '', context: 'Imp', comment: 'p'},
    {kind: 'unknown', el: makeElement(), quote: 'q2', context: 'c2', comment: 'cm2'},
  ]);
  // The elements here are detached and the page has no rows, so every
  // occurrence reads 0; the same-quote counting is its own test below.
  jsonEqual(out, [
    {kind: 'block', quote: 'q1', context: 'c1', comment: 'cm1', occurrence: 0},
    {kind: 'improve', quote: '', context: 'Imp', comment: 'p', occurrence: 0},
    {kind: 'block', quote: 'q2', context: 'c2', comment: 'cm2', occurrence: 0},
  ]);
  assert.equal('el' in out[0], false, 'el is stripped from serialized entries');
  assert.deepEqual(Object.keys(out[0]), ['kind', 'quote', 'context', 'comment', 'occurrence']);
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

test('deserializeDraft reads a stored occurrence through and an absent one as 0', () => {
  const {window} = loadScript();
  const out = window.__cbcDeserializeDraft(JSON.stringify([
    {kind: 'block', quote: 'q', context: 'c', comment: 'cm', occurrence: 2},
    {kind: 'block', quote: 'q2', context: 'c2', comment: 'cm2'},
  ]));
  assert.equal(out[0].occurrence, 2, 'a stored occurrence survives the restore');
  assert.equal(out[1].occurrence, 0, 'an absent occurrence reads as 0');
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
    {kind: 'block', el: null, quote: 'q', context: '', comment: '', occurrence: 0},
  ]);
  jsonEqual(
    deserialize(JSON.stringify([{kind: 'block', quote: 'a'}, 'bad', null, {kind: 'improve', comment: 'b'}])),
    [
      {kind: 'block', el: null, quote: 'a', context: '', comment: '', occurrence: 0},
      {kind: 'improve', el: null, quote: '', context: '', comment: 'b', occurrence: 0},
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
  assert.equal(restored[0].occurrence, 0, 'a detached element saves as the first same-quote row');
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
      return ok ? {ok: true, status: 202, async json() { return {}; }} : {ok: false, status: 400};
    }
    throw new Error('unexpected fetch: ' + url);
  };
}

async function addShortcutAndSend(fetch, storage) {
  const res = loadScript({storage, fetch, console: silentConsole()});
  const shortcuts = findChildByClass(dockOf(res.body), '__cbc-shortcuts');
  const shortcutBtn = findChildByClass(shortcuts, '__cbc-shortcut');
  clickElement(shortcutBtn);
  assert.equal(res.storage.store.has(DRAFT_KEY), true, 'add persisted the draft');

  const tray = findChildByClass(dockOf(res.body), '__cbc-tray');
  const actions = findChildByClass(tray, '__cbc-tray-actions');
  const sendBtn = findChildByClass(actions, '__cbc-tray-send');
  await clickElement(sendBtn);
  await flushPromises(6);
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
  // Omitting the sessionStorage option leaves the context without the global —
  // every access must be guarded.
  const {window} = loadArtifactCommentsContext({
    console: silentConsole(),
    fetch() {
      throw new Error('unused');
    },
    nodePreceding: true,
  });
  assert.doesNotThrow(() => window.__cbcDraftKey(ARTIFACT_PATH));
  assert.equal(typeof window.__cbcLoadDraft, 'function');
  jsonEqual(window.__cbcLoadDraft(ARTIFACT_PATH), []);
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

  const tray = findChildByClass(dockOf(body), '__cbc-tray');
  const header = findChildByClass(tray, '__cbc-tray-header');
  assert.ok(
    header.textContent.startsWith('Pending comments (2)'),
    'restored 2 entries on init: ' + header.textContent
  );
  const list = findChildByClass(tray, '__cbc-tray-list');
  assert.equal(list.children.length, 2, 'two tray items rendered from the restored draft');
});

// ---------------------------------------------------------------------------
// Shortcut kinds: persistence round-trip and batch ordering
// ---------------------------------------------------------------------------

test('draft round-trip preserves every shortcut kind and coerces unknown kinds to block', () => {
  const {window} = loadScript();
  const serialize = window.__cbcSerializeDraft;
  const deserialize = window.__cbcDeserializeDraft;

  const stored = serialize([
    {kind: 'improve', el: null, quote: '', context: 'Improve', comment: 'a'},
    {kind: 'shorten', el: null, quote: '', context: 'Shorten', comment: 'b'},
    {kind: 'verify', el: null, quote: '', context: 'Verify', comment: 'c'},
    {kind: 'nope', el: makeElement(), quote: 'q', context: 'ctx', comment: 'd'},
  ]);
  assert.equal(
    stored.map((entry) => entry.kind).join(','),
    'improve,shorten,verify,block'
  );

  const restored = deserialize(JSON.stringify(stored));
  assert.equal(
    restored.map((entry) => entry.kind).join(','),
    'improve,shorten,verify,block'
  );
  assert.equal(restored.map((entry) => entry.comment).join(','), 'a,b,c,d');
});

test('send puts shortcut entries ahead of block comments and keeps their order', async () => {
  const storage = makeStorage();
  storage.setItem(
    DRAFT_KEY,
    JSON.stringify([
      {kind: 'block', quote: 'quoted text', context: 'Section 2', comment: 'block note'},
      {kind: 'verify', quote: '', context: 'Verify', comment: 'verify prompt'},
      {kind: 'improve', quote: '', context: 'Improve', comment: 'improve prompt'},
    ])
  );

  let posted = null;
  const fetch = async (url, options = {}) => {
    if (url === '/api/sessions/sess-draft') {
      return {ok: true, status: 200, async json() { return {name: 'Draft Session'}; }};
    }
    if (url === '/api/chat/sess-draft/message') {
      posted = JSON.parse(options.body).content;
      return {ok: true, status: 202, async json() { return {}; }};
    }
    throw new Error('unexpected fetch: ' + url);
  };

  const res = loadScript({storage, fetch, console: silentConsole()});
  const tray = findChildByClass(dockOf(res.body), '__cbc-tray');
  const actions = findChildByClass(tray, '__cbc-tray-actions');
  await clickElement(findChildByClass(actions, '__cbc-tray-send'));
  await flushPromises(6);

  assert.ok(posted, 'the batch was posted');
  const numbered = posted.split('\n').filter((line) => /^\d+\. /.test(line));
  assert.equal(numbered.length, 3);
  assert.equal(numbered[0], '1. ▸ Verify');
  assert.equal(numbered[1], '2. ▸ Improve');
  assert.equal(numbered[2], '3. ▸ Section 2 › "quoted text"');
});

// ---------------------------------------------------------------------------
// occurrence: the draft's own-row index among the page's same-quote rows.
// The fork fixture mirrors the 11-12 briefing page, where a draft on fork 2's
// rec line used to re-anchor to fork 1.
// ---------------------------------------------------------------------------

test('serializeDraft stores the 0-based same-quote occurrence of the anchored row', () => {
  const page = makeForkPage();
  const {window} = loadScript({bodyChildren: page.children, fetch: sessionFetch(true)});
  const serialize = window.__cbcSerializeDraft;
  const entry = {kind: 'block', el: null, quote: REC_TEXT, context: 'ctx', comment: 'cm'};
  entry.el = page.fork1.rec;
  assert.equal(serialize([entry])[0].occurrence, 0, 'the first same-quote row reads 0');
  entry.el = page.fork2.rec;
  assert.equal(serialize([entry])[0].occurrence, 1, 'the second same-quote row reads 1');
  entry.el = null;
  assert.equal(serialize([entry])[0].occurrence, 0, 'a detached element reads 0');
});

test('a draft on the second fork\'s repeated line re-anchors to that fork after reload', () => {
  const page = makeForkPage();
  const storage = makeStorage();
  storage.setItem(DRAFT_KEY, JSON.stringify([
    {kind: 'block', quote: REC_TEXT, context: 'ctx', comment: 'note', occurrence: 1},
  ]));
  const {body} = loadScript({storage, bodyChildren: page.children, fetch: sessionFetch(true)});
  assert.equal(page.fork1.rec.classList.contains('__cbc-marked'), false,
    'fork 1\'s row stays unmarked');
  assert.equal(page.fork2.rec.classList.contains('__cbc-marked'), true,
    'the draft re-anchors to fork 2\'s row, its own');
});

test('a stored draft without occurrence re-anchors to the first same-quote row', () => {
  const page = makeForkPage();
  const storage = makeStorage();
  storage.setItem(DRAFT_KEY, JSON.stringify([
    {kind: 'block', quote: REC_TEXT, context: 'ctx', comment: 'note'},
  ]));
  const {body} = loadScript({storage, bodyChildren: page.children, fetch: sessionFetch(true)});
  assert.equal(page.fork1.rec.classList.contains('__cbc-marked'), true,
    'an absent occurrence is the first same-quote row');
  assert.equal(page.fork2.rec.classList.contains('__cbc-marked'), false,
    'no other row is marked');
});
