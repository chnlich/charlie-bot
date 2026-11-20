const assert = require('node:assert/strict');
const test = require('node:test');

const {dockOf, stackOf, findChildByClass, makeElement, clickElement, addBlockComment, flushPromises} =
  require('./artifact_comments_dom_stub');

const {loadArtifactCommentsContext} = require('./artifact_comments_vm_context');
const {HEADING_11_12, REC_TEXT, Q1_TEXT, Q2_TEXT, makeFork, makeForkPage} = require('./artifact_fork_fixture');
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
  const {context, window, body, listeners} = loadArtifactCommentsContext({
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
  return {context, window, body, storage, listeners};
}

// Fires the page's window `load` listeners, as the browser does once the page has finished loading.
function firePageLoad(listeners) {
  for (const l of listeners) {
    if (l.target === 'window' && l.type === 'load') l.handler({});
  }
}

// A block that directly owns `text`, as the page source gives a paragraph, cell or heading.
function blockOf(text, tagName = 'DIV') {
  const el = makeElement();
  el.tagName = tagName;
  el.childNodes = [{nodeType: 3, textContent: text}];
  return el;
}

// Two questions whose text is the same and longer than the 32 characters the before words cover, so the
// rows under them share their words before; OTHER_QUESTION differs at its end, so its row's words differ.
const SAME_QUESTION = '两个问题下面的同一句话，前面的三十二个字也都一样，所以它们的前文相同。';
const OTHER_QUESTION = '这一句和上面的不一样，它前面的三十二个字各不相同，所以两处的前文不同。';
const SAME_BEFORE = SAME_QUESTION.slice(-32);

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
    {kind: 'improve', el: null, quote: '', context: 'Imp', comment: 'p', occurrence: 0},
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
  const shortcuts = findChildByClass(stackOf(res.body), '__cbc-shortcuts');
  const shortcutBtn = findChildByClass(shortcuts, '__cbc-shortcut');
  clickElement(shortcutBtn);
  assert.equal(res.storage.store.has(DRAFT_KEY), true, 'add persisted the draft');

  const tray = findChildByClass(stackOf(res.body), '__cbc-tray');
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

  const tray = findChildByClass(stackOf(body), '__cbc-tray');
  const header = findChildByClass(tray, '__cbc-tray-header');
  assert.ok(
    header.textContent.startsWith('Pending (2)'),
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
  const tray = findChildByClass(stackOf(res.body), '__cbc-tray');
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
  const detached = {kind: 'block', el: makeElement(), quote: 'a quote no row holds', context: 'ctx', comment: 'cm'};
  assert.equal(serialize([detached])[0].occurrence, 0, 'a detached element reads 0');
  entry.el = null;
  entry.occurrence = 1;
  assert.equal(serialize([entry])[0].occurrence, 1, 'an unattached draft keeps its stored occurrence');
});

test('a draft on a row whose quote repeats stores the words before it; a unique quote stores none', () => {
  const page = makeForkPage();
  const {window} = loadScript({bodyChildren: page.children, fetch: sessionFetch(true)});
  const serialize = window.__cbcSerializeDraft;
  const repeated = {kind: 'block', el: page.fork2.rec, quote: REC_TEXT, context: 'ctx', comment: 'cm'};
  const unique = {kind: 'block', el: page.heading, quote: HEADING_11_12, context: '', comment: 'cm'};
  const [stored, plain] = serialize([repeated, unique]);
  assert.equal(stored.before, page.fork2.q.textContent.slice(-32), 'the words before the row');
  assert.equal(stored.occurrence, 1);
  assert.equal('before' in plain, false, 'a quote held by one row stores no before');
});

test('an unattached draft keeps its stored before and occurrence when it is saved again', () => {
  const storage = makeStorage();
  storage.setItem(DRAFT_KEY, JSON.stringify([
    {kind: 'block', quote: REC_TEXT, context: 'ctx', comment: 'note', before: 'stored words', occurrence: 1},
  ]));
  const {window} = loadScript({storage, bodyChildren: [], fetch: sessionFetch(true)});
  window.__cbcSaveDraft(window.__cbcLoadDraft(ARTIFACT_PATH), ARTIFACT_PATH);
  jsonEqual(JSON.parse(storage.store.get(DRAFT_KEY)), [
    {kind: 'block', quote: REC_TEXT, context: 'ctx', comment: 'note', occurrence: 1, before: 'stored words'},
  ]);
});

test('a stored draft with a before attaches to the one row whose quote and before both match', () => {
  const page = makeForkPage();
  const storage = makeStorage();
  storage.setItem(DRAFT_KEY, JSON.stringify([
    {kind: 'block', quote: REC_TEXT, context: 'ctx', comment: 'note',
      before: page.fork2.q.textContent.slice(-32), occurrence: 0},
  ]));
  loadScript({storage, bodyChildren: page.children, fetch: sessionFetch(true)});
  assert.equal(page.fork1.rec.classList.contains('__cbc-marked'), false, 'fork 1\'s row stays unmarked');
  assert.equal(page.fork2.rec.classList.contains('__cbc-marked'), true,
    'the one matching row is taken, whatever the stored occurrence says');
});

test('a draft with a before and several matching rows takes the row at its stored occurrence', () => {
  const page = {
    heading: blockOf(HEADING_11_12, 'H2'),
    fork1: makeFork('1', SAME_QUESTION),
    fork2: makeFork('2', SAME_QUESTION),
  };
  page.children = [page.heading, page.fork1.fork, page.fork2.fork];
  const storage = makeStorage();
  storage.setItem(DRAFT_KEY, JSON.stringify([
    {kind: 'block', quote: REC_TEXT, context: 'ctx', comment: 'note', before: SAME_BEFORE, occurrence: 1},
  ]));
  loadScript({storage, bodyChildren: page.children, fetch: sessionFetch(true)});
  assert.equal(page.fork1.rec.classList.contains('__cbc-marked'), false, 'the row at occurrence 0 is not taken');
  assert.equal(page.fork2.rec.classList.contains('__cbc-marked'), true, 'the row at occurrence 1 is taken');
});

test('a draft with a before stays unattached when its stored occurrence row is not a match', () => {
  const page = {
    heading: blockOf(HEADING_11_12, 'H2'),
    fork1: makeFork('1', SAME_QUESTION),
    fork2: makeFork('2', OTHER_QUESTION),
    fork3: makeFork('3', SAME_QUESTION),
  };
  page.children = [page.heading, page.fork1.fork, page.fork2.fork, page.fork3.fork];
  const storage = makeStorage();
  storage.setItem(DRAFT_KEY, JSON.stringify([
    {kind: 'block', quote: REC_TEXT, context: 'ctx', comment: 'note', before: SAME_BEFORE, occurrence: 1},
  ]));
  loadScript({storage, bodyChildren: page.children, fetch: sessionFetch(true)});
  for (const row of [page.fork1.rec, page.fork2.rec, page.fork3.rec]) {
    assert.equal(row.classList.contains('__cbc-marked'), false, 'no row is marked');
  }
});

test('a stored draft without a before stays unattached when its quote repeats', () => {
  const page = makeForkPage();
  const storage = makeStorage();
  storage.setItem(DRAFT_KEY, JSON.stringify([
    {kind: 'block', quote: REC_TEXT, context: 'ctx', comment: 'note'},
  ]));
  loadScript({storage, bodyChildren: page.children, fetch: sessionFetch(true)});
  assert.equal(page.fork1.rec.classList.contains('__cbc-marked'), false, 'no row is marked');
  assert.equal(page.fork2.rec.classList.contains('__cbc-marked'), false, 'no row is marked');
});

// ---------------------------------------------------------------------------
// A draft written on one page version finds its row on the next: the words
// before the row place it, the stored occurrence only picks among matches.
// ---------------------------------------------------------------------------

function capturingFetch(posted) {
  return async (url, options = {}) => {
    if (url === '/api/sessions/sess-draft') {
      return {ok: true, status: 200, async json() { return {name: 'Draft Session'}; }};
    }
    if (url === '/api/chat/sess-draft/message') {
      posted.push(JSON.parse(options.body).content);
      return {ok: true, status: 202, async json() { return {}; }};
    }
    throw new Error('unexpected fetch: ' + url);
  };
}

async function sendTray(res) {
  const tray = findChildByClass(stackOf(res.body), '__cbc-tray');
  await clickElement(findChildByClass(findChildByClass(tray, '__cbc-tray-actions'), '__cbc-tray-send'));
  await flushPromises(6);
}

test('a draft on a repeated row restores to its own row after one item is inserted above', () => {
  const old = makeForkPage();
  const first = loadScript({bodyChildren: old.children, fetch: sessionFetch(true)});
  addBlockComment(first.body, first.listeners, old.fork2.rec, 'note');

  const inserted = makeFork('1', '要不要处理新插入的一项？');
  const shiftedFirst = makeFork('2', Q1_TEXT);
  const shiftedSecond = makeFork('3', Q2_TEXT);
  loadScript({
    storage: first.storage,
    bodyChildren: [blockOf(HEADING_11_12, 'H2'), inserted.fork, shiftedFirst.fork, shiftedSecond.fork],
    fetch: sessionFetch(true),
  });
  assert.equal(shiftedSecond.rec.classList.contains('__cbc-marked'), true,
    'the draft attaches to the row of the item it was written on');
  assert.equal(shiftedFirst.rec.classList.contains('__cbc-marked'), false, 'the row above stays unmarked');
  assert.equal(inserted.rec.classList.contains('__cbc-marked'), false, 'the inserted row stays unmarked');
});

test('a draft is restored at page load when the page rewrites its text after the script ran', () => {
  const SENTENCE = '页面改写后两处相同的句子，它们前面的文字不同，所以前文也不同。';
  const final = loadScript({
    bodyChildren: [blockOf('第一段'), blockOf(SENTENCE), blockOf('第二段'), blockOf(SENTENCE)],
    fetch: sessionFetch(true),
  });
  addBlockComment(final.body, final.listeners, final.body.children[3], 'note');

  const placeholder = [blockOf('第一段'), blockOf('占位'), blockOf('第二段'), blockOf('占位')];
  const rewritten = loadScript({storage: final.storage, bodyChildren: placeholder, fetch: sessionFetch(true)});
  assert.equal(placeholder[3].classList.contains('__cbc-marked'), false, 'the placeholder text holds no draft');

  placeholder[1].childNodes = [{nodeType: 3, textContent: SENTENCE}];
  placeholder[3].childNodes = [{nodeType: 3, textContent: SENTENCE}];
  firePageLoad(rewritten.listeners);
  assert.equal(placeholder[3].classList.contains('__cbc-marked'), true,
    'at page load the draft attaches to the row with its words before');
  assert.equal(placeholder[1].classList.contains('__cbc-marked'), false, 'the other row stays unmarked');
});

test('a draft saved on a unique row attaches to the one other row that holds its sentence', () => {
  const SENTENCE = '只出现一次的句子，原来的那一行之后会被删掉。';
  const before = loadScript({bodyChildren: [blockOf('标题'), blockOf(SENTENCE)], fetch: sessionFetch(true)});
  addBlockComment(before.body, before.listeners, before.body.children[1], 'note');
  assert.equal('before' in JSON.parse(before.storage.store.get(DRAFT_KEY))[0], false,
    'a quote that held one row stores no before');

  const moved = blockOf(SENTENCE);
  loadScript({storage: before.storage, bodyChildren: [blockOf('标题'), blockOf('别的内容'), moved],
    fetch: sessionFetch(true)});
  assert.equal(moved.classList.contains('__cbc-marked'), true,
    'the one row that holds the sentence takes the draft');
});

test('an unattached draft with a stored before sends no before line and no (k of m)', async () => {
  const storage = makeStorage();
  storage.setItem(DRAFT_KEY, JSON.stringify([
    {kind: 'block', quote: 'Shared sentence in two rows', context: 'Plan', comment: 'first note',
      before: 'the words before', occurrence: 0},
    {kind: 'block', quote: 'Shared sentence in two rows', context: 'Plan', comment: 'second note',
      before: 'the words before', occurrence: 0},
  ]));
  const posted = [];
  const res = loadScript({storage, bodyChildren: [blockOf('Something else entirely')], fetch: capturingFetch(posted)});
  await sendTray(res);
  assert.equal(posted.length, 1, 'the batch was posted once');
  assert.equal(posted[0], [
    '[Artifact comments · ' + ARTIFACT_PATH + '] (2)',
    '',
    '1. ▸ Plan › "Shared sentence in two rows"',
    '   ↳ first note',
    '',
    '2. ▸ Plan › "Shared sentence in two rows"',
    '   ↳ second note',
  ].join('\n'));
});

test('a whole-section draft with a repeated row quote stores no before', () => {
  const quote = 'Synthetic shared section text';
  const section = makeElement();
  section.tagName = 'SECTION';
  section.appendChild(blockOf(quote));
  const other = blockOf(quote);
  const {window} = loadScript({bodyChildren: [section, other], fetch: sessionFetch(true)});
  const [stored] = window.__cbcSerializeDraft([
    {kind: 'block', el: section, quote, context: 'Section', comment: 'note'},
  ]);
  assert.equal('before' in stored, false, 'whole-section drafts keep the existing schema');
});
