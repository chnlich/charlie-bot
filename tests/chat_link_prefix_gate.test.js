// Same-host links whose scheme or port was written from memory are repaired only under the
// file-server prefixes, the routes the page origin itself serves; every other path on this
// host names another frontend and navigates exactly as written. P1 navigation fidelity:
// non-prefix links come back byte-identical with no probes. P2 correction preserved:
// prefix links with a wrong scheme or port land on the page origin. The channel-2 missing
// probe sees same-origin literals only; a cross-port candidate is neither rewritten nor
// probed, because the page origin never served that path and cross-origin status is
// unreadable anyway.
const assert = require('node:assert/strict');
const test = require('node:test');

const {
  anchor,
  makeMessage,
  loadArtifactsScript,
  render,
  markersIn,
  statusResponder,
} = require('./file_link_dom_stub');

const PAGE = 'https://charliebot.example:18498/';

async function renderLink(href) {
  // A browser's cross-origin fetch rejects without CORS headers; the stub's respond is not
  // asked to care about origins, so this wrapper models the rejection explicitly. Without
  // it, the stub's 404 would fashion a missing marker a real page can never show.
  const respond = async (url) => {
    if (new URL(url).origin !== new URL(PAGE).origin) throw new TypeError('Failed to fetch');
    return statusResponder({})(url);
  };
  const {context, requests} = loadArtifactsScript({pageUrl: PAGE, respond});
  const {root} = makeMessage([anchor(href, 'link')]);
  await render(context, root);
  const anchorNode = root.querySelector('a[href]');
  assert.ok(anchorNode, 'anchor survives the render');
  return {href: anchorNode.getAttribute('href'), requests, markers: markersIn(root)};
}

// P1: navigation fidelity — non-prefix links on this host pass through as written.

test('a publish-lane link stays on its own port, byte-identical, unprobed', async () => {
  const href = 'https://charliebot.example/charliebot_pub/x.html';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
});

test('a neighboring-port link stays on its own port, byte-identical, unprobed', async () => {
  const href = 'https://charliebot.example:18503/metrics';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
});

test('the prefix match holds on a path segment boundary', async () => {
  const href = 'https://charliebot.example/fileserver/x';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
});

test('another hostname is another server, left as written, unmarked', async () => {
  // A cross-origin probe still fires (pre-existing, outside this change): the request is
  // recorded, the browser rejects the response, and neither a marker nor a rewrite can
  // come of it.
  const href = 'https://other.example/absolute_filepath/tmp/a.html';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 1);
  assert.equal(out.markers.length, 0);
});

// P2: correction preserved — prefix links with a wrong scheme or port land on the page origin.

test('a wrong-scheme absolute_filepath link is pulled back to the page origin', async () => {
  const out = await renderLink('http://charliebot.example/absolute_filepath/tmp/a.html');
  const expected = PAGE + 'absolute_filepath/tmp/a.html';
  assert.equal(out.href, expected);
  assert.deepEqual(out.requests.map((entry) => entry.method + ' ' + entry.url), ['HEAD ' + expected]);
});

test('a wrong-port /files link is pulled back to the page origin', async () => {
  const out = await renderLink('https://charliebot.example:9999/files/tmp/a.html');
  const expected = PAGE + 'files/tmp/a.html';
  assert.equal(out.href, expected);
  assert.deepEqual(out.requests.map((entry) => entry.method + ' ' + entry.url), ['HEAD ' + expected]);
});

test('a same-origin prefix link needs no correction', async () => {
  const out = await renderLink(PAGE + 'files/tmp/a.html');
  assert.equal(out.href, PAGE + 'files/tmp/a.html');
  assert.deepEqual(out.requests.map((entry) => entry.method + ' ' + entry.url), ['HEAD ' + PAGE + 'files/tmp/a.html']);
});

// Channel-2 missing probe: same-origin literals are still probed, cross-port candidates are not.

test('a same-origin mis-composed artifact link is probed at its literal URL and marked', async () => {
  const href = PAGE + 'artifacts/report.html';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.deepEqual(out.requests.map((entry) => entry.method + ' ' + entry.url), ['HEAD ' + href]);
  assert.equal(out.markers.length, 1);
});

test('a cross-port mis-composed artifact link is neither rewritten nor probed', async () => {
  const href = 'https://charliebot.example/artifacts/report.html';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
  assert.equal(out.markers.length, 0);
});
