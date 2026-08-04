// ---------------------------------------------------------------------------
// Two prefixes, one path. A file link under /files/ and the same link under
// /absolute_filepath/ resolve to one absolute path, one card and one plan
// badge, and dedupe against each other inside a message. A link whose target
// the server has nothing at is marked where it appears, in each of the three
// carriers the render already walks. The marking costs the render no request it
// was not already going to make.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ARTIFACTS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'chat', 'artifacts.js'),
  'utf8'
);

const PAGE_URL = 'https://charliebot.example/';
const SESSIONS_ROOT = '/home/user/.charliebot/sessions';
const SESSION_ID = 'sess-42';
const SESSION_DIR = SESSIONS_ROOT + '/' + SESSION_ID;
const ARTIFACT_ABS = SESSION_DIR + '/artifacts/report.html';
const ELEMENT_NODE = 1;
const TEXT_NODE = 3;
const FRAGMENT_NODE = 11;
const MARKER_CLASS = 'file-link-missing';

// ---------------------------------------------------------------------------
// Fake DOM: nodes with real parent/child links, because the marking splits a
// text node and hangs a sibling off an element.
// ---------------------------------------------------------------------------
class FakeNode {
  constructor(nodeType) {
    this.nodeType = nodeType;
    this.childNodes = [];
    this.parentNode = null;
  }

  get isConnected() {
    return true;
  }

  get nextSibling() {
    if (!this.parentNode) return null;
    const siblings = this.parentNode.childNodes;
    return siblings[siblings.indexOf(this) + 1] || null;
  }

  get nextElementSibling() {
    let sibling = this.nextSibling;
    while (sibling && sibling.nodeType !== ELEMENT_NODE) sibling = sibling.nextSibling;
    return sibling;
  }

  appendChild(child) {
    return this.insertBefore(child, null);
  }

  insertBefore(child, before) {
    if (child.nodeType === FRAGMENT_NODE) {
      child.childNodes.slice().forEach((inner) => this.insertBefore(inner, before));
      return child;
    }
    if (child.parentNode) child.parentNode.removeChild(child);
    const at = before ? this.childNodes.indexOf(before) : -1;
    if (at === -1) this.childNodes.push(child);
    else this.childNodes.splice(at, 0, child);
    child.parentNode = this;
    return child;
  }

  removeChild(child) {
    const at = this.childNodes.indexOf(child);
    if (at !== -1) this.childNodes.splice(at, 1);
    child.parentNode = null;
    return child;
  }

  replaceChild(next, old) {
    this.insertBefore(next, old);
    return this.removeChild(old);
  }
}

class FakeText extends FakeNode {
  constructor(value) {
    super(TEXT_NODE);
    this.nodeValue = String(value);
  }

  get textContent() {
    return this.nodeValue;
  }
}

class FakeFragment extends FakeNode {
  constructor() {
    super(FRAGMENT_NODE);
  }
}

function matchesSelector(node, selector) {
  if (selector === 'a[href]') return node.tagName === 'A' && node.attributes.has('href');
  if (selector.charAt(0) === '.') return node.classList.contains(selector.slice(1));
  if (selector.charAt(0) === '#') return node.id === selector.slice(1);
  return node.tagName === selector.toUpperCase();
}

class FakeElement extends FakeNode {
  constructor(tag) {
    super(ELEMENT_NODE);
    this.tagName = String(tag).toUpperCase();
    this.dataset = {};
    this.attributes = new Map();
    this.className = '';
    this.title = '';
    this.id = '';
    this._html = '';
    this._text = null;
  }

  get classList() {
    const classes = String(this.className).split(/\s+/);
    return {contains: (name) => classes.indexOf(name) !== -1};
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  getAttribute(name) {
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }

  set textContent(value) {
    this._text = String(value);
    this.childNodes.length = 0;
  }

  get textContent() {
    if (this._text !== null) return this._text;
    return this.childNodes.map((child) => child.textContent || '').join('');
  }

  // The renderer writes card markup as a string. Only the toolbar is read back out of it, so
  // that is the one child this rebuilds.
  set innerHTML(value) {
    this._html = String(value);
    this.childNodes.length = 0;
    if (this._html.indexOf('html-artifact-toolbar') !== -1) {
      const toolbar = new FakeElement('div');
      toolbar.className = 'html-artifact-toolbar';
      this.appendChild(toolbar);
    }
  }

  get innerHTML() {
    return this._html;
  }

  insertAdjacentHTML(position, html) {
    assert.equal(position, 'beforeend');
    this._html += html;
  }

  closest(selector) {
    let node = this;
    while (node && node.nodeType === ELEMENT_NODE) {
      if (matchesSelector(node, selector)) return node;
      node = node.parentNode;
    }
    return null;
  }

  querySelectorAll(selector) {
    const found = [];
    const walk = (node) => {
      node.childNodes.forEach((child) => {
        if (child.nodeType !== ELEMENT_NODE) return;
        if (matchesSelector(child, selector)) found.push(child);
        walk(child);
      });
    };
    walk(this);
    return found;
  }

  querySelector(selector) {
    return this.querySelectorAll(selector)[0] || null;
  }
}

function element(tag, className) {
  const node = new FakeElement(tag);
  if (className) node.className = className;
  return node;
}

function anchor(href, label) {
  const node = element('a');
  node.setAttribute('href', href);
  node.appendChild(new FakeText(label || href));
  return node;
}

function inlineCode(text) {
  const node = element('code');
  node.textContent = text;
  return node;
}

function makeMessage(children) {
  const container = element('div');
  const prose = element('div', 'prose-msg');
  children.forEach((child) => prose.appendChild(child));
  container.appendChild(prose);
  return {root: container, prose};
}

function escapeHtml(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

// The card markup the renderer builds, read back as an element: its classes, its data-*
// attributes and its toolbar are the whole DOM surface the code under test touches.
function parseCardHtml(html) {
  const card = new FakeElement('div');
  const classMatch = html.match(/^<div class="([^"]*)"/);
  card.className = classMatch ? classMatch[1] : '';
  const dataAttribute = /data-([a-z-]+)="([^"]*)"/g;
  let match;
  while ((match = dataAttribute.exec(html)) !== null) {
    card.dataset[match[1].replace(/-([a-z])/g, (_, letter) => letter.toUpperCase())] = match[2];
  }
  card.innerHTML = html;
  return card;
}

function makeTemplate() {
  return {
    content: {firstElementChild: null},
    set innerHTML(value) {
      this.content.firstElementChild = parseCardHtml(String(value));
    },
  };
}

function loadArtifactsScript(opts) {
  const o = opts || {};
  const requests = [];
  const respond = o.respond || (async () => ({ok: true, status: 200, text: async () => '<html><body>ok</body></html>'}));
  const context = {
    SESSION_ID,
    escapeHtml,
    hljs: {highlight: (value) => ({value: escapeHtml(value)})},
    localStorage: {getItem: () => null, setItem: () => {}},
    window: {addEventListener: () => {}, SESSIONS_ROOT, location: {href: o.pageUrl || PAGE_URL}},
    console: {warn: () => {}, error: () => {}},
    URL: globalThis.URL,
    Node: {ELEMENT_NODE, TEXT_NODE},
    fetch: (url, init) => {
      requests.push({url: String(url), method: (init && init.method) || 'GET'});
      return respond(String(url), init);
    },
    document: {
      addEventListener: () => {},
      getElementById: () => null,
      querySelector: () => null,
      querySelectorAll: () => [],
      createElement: (tag) => (tag === 'template' ? makeTemplate() : new FakeElement(tag)),
      createTextNode: (value) => new FakeText(value),
      createDocumentFragment: () => new FakeFragment(),
    },
  };
  if (o.planPanel) context.planPanel = o.planPanel;
  vm.createContext(context);
  vm.runInContext(
    'globalThis.Chat = globalThis.Chat || {};' +
    'globalThis.Chat.expose = function expose(names) {' +
    '  for (var i = 0; i < names.length; i++) globalThis[names[i]] = globalThis.Chat[names[i]];' +
    '};',
    context,
    {filename: 'chat-namespace-stub.js'}
  );
  vm.runInContext(ARTIFACTS_JS, context, {filename: 'artifacts.js'});
  return {context, requests};
}

function settled() {
  return new Promise((resolve) => setImmediate(resolve));
}

async function render(context, root) {
  context.Chat.embedLinkedHtmlArtifacts(root);
  await settled();
}

function markersIn(node) {
  const found = [];
  const walk = (current) => {
    current.childNodes.forEach((child) => {
      if (child.nodeType !== ELEMENT_NODE) return;
      if (child.classList.contains(MARKER_CLASS)) found.push(child);
      walk(child);
    });
  };
  walk(node);
  return found;
}

function insertedCards(root) {
  return root.childNodes.filter(
    (child) => child.nodeType === ELEMENT_NODE && child.classList.contains('html-artifact'));
}

// The server keyed by pathname, so a card fetch (root-relative) and a probe (absolute) that
// name the same file get the same answer.
function statusResponder(statusByPath) {
  return async (url) => {
    const status = statusByPath[new URL(url, PAGE_URL).pathname] || 404;
    return {ok: status < 400, status, text: async () => '<html><body>ok</body></html>'};
  };
}

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
