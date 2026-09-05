// ---------------------------------------------------------------------------
// Chat HTML artifacts render as compact link cards. No srcdoc iframe — and no
// artifact fetch — happens while messages render; the iframe exists only while
// a card is expanded, and both the fetch cache and the number of simultaneously
// expanded frames are bounded.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadArtifactsScript, makeCard } = require('./file_link_dom_stub');

const { makeAnchor, makeProseRoot } = require('./chat_prose_stub');

const { SESSION_DIR } = require('./sessions_root_stub');

const ARTIFACT_HREF = '/files' + SESSION_DIR + '/artifacts/report.html';
const ARTIFACT_ABS = SESSION_DIR + '/artifacts/report.html';

// ---------------------------------------------------------------------------
// Message rendering never creates an iframe
// ---------------------------------------------------------------------------

test('rendering a message with an artifact link creates a card, not an iframe', async () => {
  const {context, requests} = loadArtifactsScript();
  const {root, parent} = makeProseRoot({anchors: [makeAnchor(ARTIFACT_HREF)]});

  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(parent.inserted.length, 1);
  const html = parent.inserted[0].innerHTML;
  assert.equal(html.indexOf('<iframe'), -1, 'no iframe is created during message rendering');
  assert.equal(html.indexOf('srcdoc'), -1, 'no srcdoc document is built during message rendering');
  assert.deepEqual(requests, [], 'the artifact body is not fetched during message rendering');
  assert.match(html, /class="artifact-compact-card html-artifact"/);
  assert.match(html, /report\.html/, 'the card is labelled with the artifact filename');
  assert.match(html, /onclick="toggleHtmlArtifactEmbed\(this\)"/, 'the card carries an expand control');
  assert.match(html, /Open in tab/);
});

test('the rendered card carries the abs path and fetch url the expand needs', async () => {
  const {context} = loadArtifactsScript();
  const {root, parent} = makeProseRoot({anchors: [makeAnchor(ARTIFACT_HREF)]});

  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));

  const card = parent.inserted[0];
  assert.equal(card.dataset.artifactPath, ARTIFACT_ABS);
  assert.equal(card.dataset.artifactFetchUrl, ARTIFACT_HREF);
});

test('a registered plan version still renders the plan card and no iframe', async () => {
  const snapshot = {
    plans: [{
      id: 3,
      title: 'A plan',
      state: 'awaiting approval',
      takeoff: null,
      versions: [{v: 1, file: 'artifacts/plan_01.html'}],
    }],
  };
  const planPanel = {
    ready: () => Promise.resolve(),
    getRegistrySnapshot: () => snapshot,
  };
  const {context, requests} = loadArtifactsScript({planPanel});
  const {root, parent} = makeProseRoot({anchors: [makeAnchor('/files' + SESSION_DIR + '/artifacts/plan_01.html')]});

  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(parent.inserted.length, 1);
  const html = parent.inserted[0].innerHTML;
  assert.match(html, /class="plan-compact-card html-artifact"/);
  assert.match(html, /Open panel/);
  assert.equal(html.indexOf('<iframe'), -1);
  assert.deepEqual(requests, []);
});

test('a delayed registry still embeds an artifact in a detached engine fragment', async () => {
  let resolveReady;
  const planPanel = {ready: () => new Promise((resolve) => { resolveReady = resolve; })};
  const {context} = loadArtifactsScript({planPanel});
  const {root, parent, prose} = makeProseRoot({anchors: [makeAnchor(ARTIFACT_HREF)]});
  const anchor = prose.querySelectorAll('a[href]')[0];
  anchor.isConnected = false;
  prose.__turnEngineReadyFragment = true;

  context.Chat.embedLinkedHtmlArtifacts(root);
  resolveReady();
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(parent.inserted.length, 1, 'the detached ready fragment receives its card');
  assert.match(parent.inserted[0].innerHTML, /class="artifact-compact-card html-artifact"/);
});

// ---------------------------------------------------------------------------
// Expand / collapse
// ---------------------------------------------------------------------------

test('expanding a card fetches the artifact and builds the srcdoc iframe', async () => {
  const {context, requests} = loadArtifactsScript();
  const card = makeCard(ARTIFACT_ABS, ARTIFACT_HREF);

  await context.Chat.expandArtifactCard(card);

  assert.deepEqual(requests, [{url: ARTIFACT_HREF, method: 'GET'}]);
  assert.equal(card.dataset.artifactExpanded, '1');
  assert.match(card.innerHTML, /<iframe class="html-artifact-frame"/);
  assert.match(card.innerHTML, /srcdoc="/);
  assert.match(card.innerHTML, /sandbox="allow-scripts allow-popups allow-popups-to-escape-sandbox"/);
  assert.match(card.innerHTML, /html-artifact-height/, 'the resize postMessage protocol survives');
  assert.match(card.innerHTML, /<base href=&quot;/, 'link-behavior injection survives');
  assert.match(card.innerHTML, /#cbsession=sess-42/, 'the viewing-session fragment stamp survives');
  assert.match(card.innerHTML, /html-artifact-resize-handle/, 'manual resize handles survive');
});

test('collapsing a card destroys the iframe and restores the compact toolbar', async () => {
  const {context} = loadArtifactsScript();
  const card = makeCard(ARTIFACT_ABS, ARTIFACT_HREF);
  await context.Chat.expandArtifactCard(card);

  context.Chat.collapseArtifactCard(card);

  assert.equal(card.innerHTML.indexOf('<iframe'), -1, 'the live document is gone on collapse');
  assert.equal(card.dataset.artifactExpanded, undefined);
  assert.match(card.innerHTML, /onclick="toggleHtmlArtifactEmbed\(this\)"/);
  assert.match(card.innerHTML, /report\.html/);
});

test('toggleHtmlArtifactEmbed expands then collapses the same card', async () => {
  const {context} = loadArtifactsScript();
  const card = makeCard(ARTIFACT_ABS, ARTIFACT_HREF);
  const btn = {closest: (selector) => (selector === '.html-artifact' ? card : null)};

  context.toggleHtmlArtifactEmbed(btn);
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(card.dataset.artifactExpanded, '1');

  context.toggleHtmlArtifactEmbed(btn);
  assert.equal(card.dataset.artifactExpanded, undefined);
  assert.equal(card.innerHTML.indexOf('<iframe'), -1);
});

test('at most three cards stay expanded — a fourth collapses the oldest', async () => {
  const {context} = loadArtifactsScript();
  const cards = [1, 2, 3, 4].map((n) => {
    const abs = SESSION_DIR + '/artifacts/r' + n + '.html';
    return makeCard(abs, '/files' + abs);
  });

  for (const card of cards) await context.Chat.expandArtifactCard(card);

  assert.equal(cards[0].dataset.artifactExpanded, undefined, 'the oldest expansion was evicted');
  assert.equal(cards[0].innerHTML.indexOf('<iframe'), -1);
  for (const card of cards.slice(1)) {
    assert.equal(card.dataset.artifactExpanded, '1');
    assert.match(card.innerHTML, /<iframe/);
  }
  assert.equal(context.Chat.expandedArtifactCards.length, 3);
});

test('collapsing by hand frees a slot so the next expansion evicts nothing', async () => {
  const {context} = loadArtifactsScript();
  const cards = [1, 2, 3, 4].map((n) => {
    const abs = SESSION_DIR + '/artifacts/r' + n + '.html';
    return makeCard(abs, '/files' + abs);
  });

  for (const card of cards.slice(0, 3)) await context.Chat.expandArtifactCard(card);
  context.Chat.collapseArtifactCard(cards[1]);
  await context.Chat.expandArtifactCard(cards[3]);

  assert.equal(cards[0].dataset.artifactExpanded, '1', 'nothing was evicted');
  assert.equal(cards[1].dataset.artifactExpanded, undefined);
  assert.equal(context.Chat.expandedArtifactCards.length, 3);
});

// ---------------------------------------------------------------------------
// Bounded fetch cache
// ---------------------------------------------------------------------------

test('the artifact fetch cache holds at most eight entries, evicting least-recently-used', async () => {
  const {context, requests} = loadArtifactsScript();
  const cache = context.Chat.htmlArtifactFetchCache;
  const paths = [];
  for (let i = 0; i < 9; i++) paths.push(SESSION_DIR + '/artifacts/a' + i + '.html');

  for (const abs of paths.slice(0, 8)) await context.Chat.fetchHtmlArtifact(abs, '/files' + abs);
  assert.equal(cache.size, 8);

  // Re-read the oldest key so the second-oldest becomes least recently used.
  await context.Chat.fetchHtmlArtifact(paths[0], '/files' + paths[0]);
  assert.equal(requests.length, 8, 'a cache hit does not refetch');

  await context.Chat.fetchHtmlArtifact(paths[8], '/files' + paths[8]);
  assert.equal(cache.size, 8, 'the cache never grows past its bound');
  assert.equal(cache.has(paths[0]), true, 'the recently re-read entry survives');
  assert.equal(cache.has(paths[1]), false, 'the least recently used entry was evicted');
  assert.equal(cache.has(paths[8]), true);
});

test('a card detached while its fetch was in flight never enters the expanded set', async () => {
  const {context} = loadArtifactsScript();
  const card = makeCard(ARTIFACT_ABS, ARTIFACT_HREF);
  card.isConnected = false;

  await context.Chat.expandArtifactCard(card);

  assert.equal(card.dataset.artifactExpanded, undefined);
  assert.equal(card.innerHTML.indexOf('<iframe'), -1);
  assert.equal(context.Chat.expandedArtifactCards.length, 0);
});

test('a failed artifact fetch is not cached', async () => {
  const {context} = loadArtifactsScript();
  context.fetch = async () => ({ok: false, status: 404, text: async () => ''});
  const abs = SESSION_DIR + '/artifacts/missing.html';

  await assert.rejects(() => context.Chat.fetchHtmlArtifact(abs, '/files' + abs));
  assert.equal(context.Chat.htmlArtifactFetchCache.has(abs), false);
});
