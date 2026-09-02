// ---------------------------------------------------------------------------
// Two prefixes, one path. A file link under /files/ and the same link under
// /absolute_filepath/ resolve to one absolute path, one card and one plan
// badge, and dedupe against each other inside a message. A link whose target
// the server has nothing at is marked where it appears, in each of the three
// carriers the render already walks. The marking costs the render no request it
// was not already going to make. The fake DOM and the vm harness live in
// file_link_dom_stub.js, shared with chat_url_ascii_boundary.test.js.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const {
  PAGE_URL,
  ARTIFACT_ABS,
  TEXT_NODE,
  MARKER_CLASS,
  FakeText,
  anchor,
  inlineCode,
  makeMessage,
  loadArtifactsScript,
  render,
  markersIn,
  insertedCards,
  statusResponder,
} = require('./file_link_dom_stub');

const { SESSION_ID, SESSION_DIR } = require('./sessions_root_stub');

const PREFIXES = ['/files', '/absolute_filepath'];

// ---------------------------------------------------------------------------
// Parse equivalence
// ---------------------------------------------------------------------------

test('either prefix resolves to the same absolute path', () => {
  const {context} = loadArtifactsScript();
  const resolved = PREFIXES.map((prefix) => context.Chat.resolveHtmlArtifactLink(prefix + ARTIFACT_ABS));

  resolved.forEach((link, index) => assert.ok(link, PREFIXES[index] + ' resolves'));
  assert.deepEqual(new Set(resolved.map((link) => link.absPath)), new Set([ARTIFACT_ABS]));
  // The fetch URL keeps the prefix it arrived under, since the server answers on both.
  resolved.forEach((link, index) => assert.equal(link.fetchUrl, PREFIXES[index] + ARTIFACT_ABS));
});

test('an encoded path and a full URL resolve alike under the new prefix', () => {
  const {context} = loadArtifactsScript();
  const encoded = '/absolute_filepath/%2Ftmp%2Freport/artifacts/plot.html';
  const full = PAGE_URL.replace(/\/$/, '') + encoded;

  assert.equal(context.Chat.resolveHtmlArtifactLink(encoded).absPath, '//tmp/report/artifacts/plot.html');
  assert.equal(context.Chat.resolveHtmlArtifactLink(full).absPath, '//tmp/report/artifacts/plot.html');
});

test('the two forms of one artifact link dedupe to a single card inside one message', async () => {
  const {context, requests} = loadArtifactsScript();
  const {root, prose} = makeMessage([
    anchor('/files' + ARTIFACT_ABS, 'as the UI builds it'),
    new FakeText(' and '),
    inlineCode('/absolute_filepath' + ARTIFACT_ABS),
  ]);

  await render(context, root);

  const cards = insertedCards(prose.parentNode);
  assert.equal(cards.length, 1, 'one card for one file');
  assert.equal(cards[0].dataset.artifactPath, ARTIFACT_ABS);
  assert.deepEqual(requests, [], 'a card costs no request until it is expanded');
});

test('a plan version link carries the same badge under either prefix', async () => {
  const snapshot = {
    plans: [{
      id: 3,
      title: 'A plan',
      state: 'approved',
      takeoff: {v: 2},
      versions: [{v: 1, file: 'artifacts/plan_01.html'}, {v: 2, file: 'artifacts/plan_02.html'}],
    }],
  };
  const planPanel = {ready: () => Promise.resolve(), getRegistrySnapshot: () => snapshot};
  const badges = [];
  for (const prefix of PREFIXES) {
    const {context} = loadArtifactsScript({planPanel});
    const {root, prose} = makeMessage([anchor(prefix + SESSION_DIR + '/artifacts/plan_02.html')]);
    await render(context, root);
    const cards = insertedCards(prose.parentNode);
    assert.equal(cards.length, 1);
    badges.push(cards[0].innerHTML);
  }
  assert.match(badges[0], /plan-compact-card/);
  assert.match(badges[0], /plan-compact-version">v2</);
  assert.equal(badges[0], badges[1], 'the same card markup under both prefixes');
});

// ---------------------------------------------------------------------------
// Failure shapes: one real shape per class the link scanner distinguishes
// ---------------------------------------------------------------------------

test('a missing-prefix path in an anchor is marked and stays clickable', async () => {
  // The absolute prefix was dropped: /sessions/... instead of /home/user/.charliebot/sessions/...
  const href = '/absolute_filepath/sessions/' + SESSION_ID + '/data/trace.json';
  const {context, requests} = loadArtifactsScript({respond: statusResponder({})});
  const link = anchor(href);
  const {root, prose} = makeMessage([link]);

  await render(context, root);

  assert.deepEqual(requests, [{url: 'https://charliebot.example' + href, method: 'HEAD'}]);
  const markers = markersIn(prose);
  assert.equal(markers.length, 1);
  assert.equal(link.nextSibling, markers[0], 'the marker sits at the occurrence');
  assert.equal(link.getAttribute('href'), href, 'the anchor is left clickable');
});

test('a foreign-root path in inline code is marked', async () => {
  const {context, requests} = loadArtifactsScript({respond: statusResponder({})});
  const code = inlineCode('see /absolute_filepath/lustre/fsw/runs/step100/trace.json for the capture');
  const {root, prose} = makeMessage([code]);

  await render(context, root);

  assert.equal(requests.length, 1);
  assert.equal(requests[0].method, 'HEAD');
  assert.equal(markersIn(prose).length, 1);
  assert.equal(code.nextSibling, markersIn(prose)[0]);
});

test('a missing path in prose text is marked at the occurrence, splitting the text node', async () => {
  const {context} = loadArtifactsScript({respond: statusResponder({})});
  const missing = '/absolute_filepath/tmp/run-17/loss.csv';
  const {root, prose} = makeMessage([new FakeText('the numbers are at ' + missing + ' if you want them.')]);

  await render(context, root);

  const markers = markersIn(prose);
  assert.equal(markers.length, 1);
  const before = markers[0].parentNode.childNodes[markers[0].parentNode.childNodes.indexOf(markers[0]) - 1];
  assert.equal(before.nodeType, TEXT_NODE);
  assert.ok(before.nodeValue.endsWith(missing), 'the split lands just past the link');
  assert.equal(prose.textContent.indexOf('if you want them.') !== -1, true, 'the rest of the sentence survives');
});

test('a wrong artifact filename is marked on its card, by the fetch the expand already makes', async () => {
  const wrong = SESSION_DIR + '/artifacts/plan_absolute-filepath-prefix_v9.html';
  const {context, requests} = loadArtifactsScript({respond: statusResponder({})});
  const {root, prose} = makeMessage([anchor('/absolute_filepath' + wrong)]);

  await render(context, root);
  assert.deepEqual(requests, [], 'no probe is spent on an artifact link at render');

  const card = insertedCards(prose.parentNode)[0];
  await context.Chat.expandArtifactCard(card);

  assert.equal(requests.length, 1, 'the card fetch is the only request');
  assert.match(card.querySelector('.html-artifact-toolbar').innerHTML, new RegExp(MARKER_CLASS));
});

test('a target the server has adds no marking', async () => {
  const href = '/absolute_filepath/tmp/run-17/loss.csv';
  const {context, requests} = loadArtifactsScript({
    respond: statusResponder({[href]: 200}),
  });
  const {root, prose} = makeMessage([anchor(href)]);

  await render(context, root);

  assert.equal(requests.length, 1);
  assert.equal(markersIn(prose).length, 0);
});

test('a status other than 404 adds no marking', async () => {
  const {context} = loadArtifactsScript({
    respond: async () => ({ok: false, status: 403, text: async () => ''}),
  });
  const {root, prose} = makeMessage([anchor('/absolute_filepath/root/secret/notes.txt')]);

  await render(context, root);

  assert.equal(markersIn(prose).length, 0);
});

test('a network error adds no marking', async () => {
  const {context} = loadArtifactsScript({
    respond: async () => {
      throw new TypeError('Failed to fetch');
    },
  });
  const {root, prose} = makeMessage([anchor('/absolute_filepath/tmp/run-17/loss.csv')]);

  await render(context, root);

  assert.equal(markersIn(prose).length, 0);
});

// ---------------------------------------------------------------------------
// Glued-CJK boundary: a file link immediately followed by CJK prose scans to
// the same printable-ASCII boundary the markdown renderer cuts bare URLs at —
// the probe asks for the real path, and no mistaken missing marker lands in
// the middle of the sentence. See chat_url_ascii_boundary.test.js for the
// renderer side of the same boundary.
// ---------------------------------------------------------------------------

test('a text-carried link followed by CJK prose probes the plain path and marks nothing', async () => {
  const abs = '/tmp/run-17/debug_x_v1.html';
  const {context, requests} = loadArtifactsScript({
    respond: statusResponder({['/absolute_filepath' + abs]: 200}),
  });
  const {root, prose} = makeMessage([
    new FakeText('渲染产物在 /absolute_filepath' + abs + '（第 4 节新增）。更多说明。'),
  ]);

  await render(context, root);

  assert.equal(requests.length, 1);
  assert.equal(requests[0].method, 'HEAD');
  assert.match(requests[0].url, /^[\x21-\x7E]*$/, 'the probe URL carries no glued prose');
  assert.equal(new URL(requests[0].url).pathname, '/absolute_filepath' + abs);
  assert.equal(markersIn(prose).length, 0);
  assert.equal(prose.textContent, '渲染产物在 /absolute_filepath' + abs + '（第 4 节新增）。更多说明。',
    'the sentence survives whole, no mid-word marker split');
});

test('a code-carried link followed by CJK prose probes the plain path and marks nothing', async () => {
  const abs = '/tmp/run-17/debug_x_v1.html';
  const {context, requests} = loadArtifactsScript({
    respond: statusResponder({['/absolute_filepath' + abs]: 200}),
  });
  const {root, prose} = makeMessage([
    inlineCode('/absolute_filepath' + abs + '（第 4 节新增）。'),
  ]);

  await render(context, root);

  assert.equal(requests.length, 1);
  assert.equal(requests[0].method, 'HEAD');
  assert.match(requests[0].url, /^[\x21-\x7E]*$/, 'the probe URL carries no glued prose');
  assert.equal(new URL(requests[0].url).pathname, '/absolute_filepath' + abs);
  assert.equal(markersIn(prose).length, 0);
});

// ---------------------------------------------------------------------------
// Origin normalization
// ---------------------------------------------------------------------------

test('a link to this host with the wrong scheme or port is pulled back to the page origin', async () => {
  const wrongScheme = 'http://charliebot.example/absolute_filepath/tmp/a.txt';
  const wrongPort = 'https://charliebot.example:8080/absolute_filepath/tmp/b.txt';
  const {context, requests} = loadArtifactsScript({respond: statusResponder({})});
  const links = [anchor(wrongScheme), anchor(wrongPort)];
  const {root} = makeMessage(links);

  await render(context, root);

  assert.equal(links[0].getAttribute('href'), 'https://charliebot.example/absolute_filepath/tmp/a.txt');
  assert.equal(links[1].getAttribute('href'), 'https://charliebot.example/absolute_filepath/tmp/b.txt');
  assert.deepEqual(requests.map((request) => request.url), [
    'https://charliebot.example/absolute_filepath/tmp/a.txt',
    'https://charliebot.example/absolute_filepath/tmp/b.txt',
  ]);
});

test('a link to another hostname is left as written', async () => {
  const foreign = 'https://other.example/absolute_filepath/tmp/a.txt';
  const {context, requests} = loadArtifactsScript({respond: statusResponder({})});
  const link = anchor(foreign);
  const {root} = makeMessage([link]);

  await render(context, root);

  assert.equal(link.getAttribute('href'), foreign);
  assert.deepEqual(requests.map((request) => request.url), [foreign]);
});

// ---------------------------------------------------------------------------
// Probe budget
// ---------------------------------------------------------------------------

test('rendering an HTML artifact link issues no request at all', async () => {
  const {context, requests} = loadArtifactsScript({respond: statusResponder({})});
  const {root, prose} = makeMessage([
    anchor('/absolute_filepath' + ARTIFACT_ABS),
    new FakeText(' and the same file at /files' + ARTIFACT_ABS + ' '),
  ]);

  await render(context, root);

  assert.deepEqual(requests, []);
  assert.equal(insertedCards(prose.parentNode).length, 1);
  assert.equal(markersIn(prose).length, 0);
});

test('one HEAD per unique path, whichever prefix each occurrence used', async () => {
  const abs = '/tmp/run-17/loss.csv';
  const {context, requests} = loadArtifactsScript({respond: statusResponder({})});
  const {root, prose} = makeMessage([
    anchor('/files' + abs),
    new FakeText(' also written as /absolute_filepath' + abs + ' and as '),
    inlineCode('https://charliebot.example/absolute_filepath' + abs),
  ]);

  await render(context, root);

  assert.equal(requests.length, 1, 'three occurrences, one probe');
  assert.equal(requests[0].method, 'HEAD');
  assert.equal(markersIn(prose).length, 3, 'every occurrence carries its own marker');
});
