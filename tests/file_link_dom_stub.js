// ---------------------------------------------------------------------------
// Fake DOM and vm harness shared by the chat file-link tests
// (chat_file_link_prefixes.test.js, chat_url_ascii_boundary.test.js): nodes
// with real parent/child links, because the marking splits a text node and
// hangs a sibling off an element, plus loadArtifactsScript, which loads the
// real chat/namespace.js and chat/artifacts.js in a vm against stubs for every
// non-artifacts global the modules touch. fetch is recorded, so tests read the
// probe requests the render would have made.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const { escapeHtml } = require('./escape_html_stub');

const { SESSIONS_ROOT, SESSION_ID, SESSION_DIR } = require('./sessions_root_stub');

const NAMESPACE_JS = readStatic('chat/namespace.js');
const ARTIFACTS_JS = readStatic('chat/artifacts.js');

const PAGE_URL = 'https://charliebot.example/';
const ARTIFACT_ABS = SESSION_DIR + '/artifacts/report.html';
const ELEMENT_NODE = 1;
const TEXT_NODE = 3;
const FRAGMENT_NODE = 11;
const MARKER_CLASS = 'file-link-missing';

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
  vm.runInContext(NAMESPACE_JS, context, {filename: 'chat/namespace.js'});
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

module.exports = {
  PAGE_URL,
  ARTIFACT_ABS,
  ELEMENT_NODE,
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
};
