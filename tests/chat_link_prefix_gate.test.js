// Same-origin file links get checked and retired aliases get marked. Cross-origin
// links stay byte-identical and unprobed because this page cannot answer for their origin.
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

async function renderLink(href, statusByPath = {}) {
  // A browser's cross-origin fetch rejects without CORS headers; the stub's respond is not
  // asked to care about origins, so this wrapper models the rejection explicitly. Without
  // it, the stub's 404 would fashion a missing marker a real page can never show.
  const respond = async (url) => {
    if (new URL(url).origin !== new URL(PAGE).origin) throw new TypeError('Failed to fetch');
    return statusResponder(statusByPath)(url);
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

test('another hostname is another server, left as written and unprobed', async () => {
  const href = 'https://other.example/absolute_filepath/tmp/a.html';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
  assert.equal(out.markers.length, 0);
});

// Cross-origin file links stay as written; only the page origin is probed.

test('a cross-origin absolute_filepath link stays as written and unprobed', async () => {
  const href = 'http://charliebot.example/absolute_filepath/tmp/a.html';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
  assert.equal(out.markers.length, 0);
});

test('a same-origin retired /files link is marked without a request', async () => {
  const href = PAGE + 'files/tmp/a.html';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
  assert.equal(out.markers.length, 1);
});

test('a same-origin retired /file link is marked without a request', async () => {
  const href = PAGE + 'file/tmp/a.html';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
  assert.equal(out.markers.length, 1);
});

test('an other-port retired link stays as written and unprobed', async () => {
  const href = 'https://charliebot.example:9999/files/tmp/a.html';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
  assert.equal(out.markers.length, 0);
});

test('a same-origin prefix link needs no correction', async () => {
  const out = await renderLink(PAGE + 'absolute_filepath/tmp/a.html');
  assert.equal(out.href, PAGE + 'absolute_filepath/tmp/a.html');
  assert.deepEqual(
      out.requests.map((entry) => entry.method + ' ' + entry.url), ['HEAD ' + PAGE + 'absolute_filepath/tmp/a.html']);
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

test('an absolute local path anchor is probed once and marked on 404', async () => {
  const href = '/home/u/x.rs:258';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.deepEqual(out.requests.map((entry) => entry.method + ' ' + entry.url), ['HEAD ' + PAGE.slice(0, -1) + href]);
  assert.equal(out.markers.length, 1);
});

test('a tilde local path anchor is probed once and marked on 404', async () => {
  const href = '~/x';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.deepEqual(out.requests.map((entry) => entry.method + ' ' + entry.url), ['HEAD ' + PAGE + href]);
  assert.equal(out.markers.length, 1);
});

test('an application route probe returning 405 stays unmarked', async () => {
  const href = '/diff?x';
  const out = await renderLink(href, {'/diff': 405});
  assert.equal(out.href, href);
  assert.deepEqual(out.requests.map((entry) => entry.method + ' ' + entry.url), ['HEAD ' + PAGE.slice(0, -1) + href]);
  assert.equal(out.markers.length, 0);
});

test('a session page link is not probed', async () => {
  const href = '/?session=synthetic-id';
  const out = await renderLink(href);
  assert.equal(out.href, href);
  assert.equal(out.requests.length, 0);
  assert.equal(out.markers.length, 0);
});
