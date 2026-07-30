const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ARTIFACT_COMMENTS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'artifact-comments.js'),
  'utf8'
);

function makeClassList() {
  const classes = new Set();
  return {
    add(...tokens) { for (const t of tokens) classes.add(t); },
    remove(...tokens) { for (const t of tokens) classes.delete(t); },
    contains(token) { return classes.has(token); },
    toggle(token, force) {
      if (force === true) { classes.add(token); return true; }
      if (force === false) { classes.delete(token); return false; }
      if (classes.has(token)) { classes.delete(token); return false; }
      classes.add(token); return true;
    },
  };
}

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
    offsetHeight: 40,
    offsetWidth: 300,
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
    classList: makeClassList(),
    closest() { return null; },
    appendChild(child) {
      const prior = child.parentNode;
      if (prior) {
        const ci = prior.children.indexOf(child);
        if (ci !== -1) prior.children.splice(ci, 1);
        const ni = prior.childNodes.indexOf(child);
        if (ni !== -1) prior.childNodes.splice(ni, 1);
      }
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
    removeChild(child) {
      const index = this.children.indexOf(child);
      if (index !== -1) this.children.splice(index, 1);
      const nodeIndex = this.childNodes.indexOf(child);
      if (nodeIndex !== -1) this.childNodes.splice(nodeIndex, 1);
      child.parentNode = null;
      child.parentElement = null;
      return child;
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
    getBoundingClientRect() {
      return {left: 0, top: 0, right: 0, bottom: 0, width: 0, height: 0};
    },
    scrollIntoView() {},
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
    querySelectorAll(selector) {
      const all = [];
      const stack = this.children.slice();
      while (stack.length > 0) {
        const child = stack.shift();
        all.push(child);
        stack.push(...child.children);
      }
      if (selector === '*') return all;
      if (!selector.startsWith('.')) return [];
      const targetClass = selector.slice(1);
      return all.filter((el) => String(el.className || '').split(/\s+/).includes(targetClass));
    },
  };
}

function makeTextNode(text) {
  return {nodeType: 3, textContent: text};
}

// Build a block element that directly owns `text`, with optional child elements.
function makeBlock(text, {display = 'block', childNodes = []} = {}) {
  const el = makeElement();
  el.display = display;
  el.childNodes = text ? [makeTextNode(text), ...childNodes] : childNodes.slice();
  for (const child of el.childNodes) {
    if (child.nodeType === 1) child.parentElement = el;
  }
  return el;
}

function loadArtifactCommentsScript(pathname, framed = false, opts = {}) {
  const listeners = [];
  const window = {
    location: {pathname, hash: opts.hash || ''},
    innerWidth: opts.innerWidth !== undefined ? opts.innerWidth : 1024,
    innerHeight: 768,
    addEventListener(type, handler, options) {
      listeners.push({target: 'window', type, handler, options});
    },
    setTimeout() {},
    requestAnimationFrame(fn) { fn(); return 0; },
    getComputedStyle(el) {
      return {display: el.display || 'block'};
    },
  };
  window.self = window;
  window.parent = framed ? (opts.parent || {}) : window;

  const head = makeElement();
  const body = makeElement();
  if (opts.bodyChildren) {
    for (const child of opts.bodyChildren) {
      body.appendChild(child);
    }
  }
  const documentElement = makeElement();
  documentElement.tagName = 'HTML';
  documentElement.clientWidth = opts.clientWidth || window.innerWidth;
  const document = {
    documentElement,
    head,
    body,
    createElement() {
      return makeElement();
    },
    addEventListener(type, handler, options) {
      listeners.push({target: 'document', type, handler, options});
    },
    querySelectorAll(selector) {
      if (!selector.startsWith('.')) return [];
      const targetClass = selector.slice(1);
      const matches = [];
      const stack = body.children.slice();
      while (stack.length > 0) {
        const child = stack.shift();
        const classes = String(child.className || '').split(/\s+/);
        if (classes.includes(targetClass)) matches.push(child);
        stack.push(...child.children);
      }
      return matches;
    },
  };

  const context = {
    window,
    document,
    console: opts.console || console,
    Node: {DOCUMENT_POSITION_FOLLOWING: 4},
    fetch: opts.fetch || function() {
      throw new Error('fetch should not run while loading artifact-comments.js');
    },
  };
  if (opts.sessionStorage !== undefined) {
    context.sessionStorage = opts.sessionStorage;
  }
  vm.createContext(context);
  vm.runInContext(ARTIFACT_COMMENTS_JS, context, {filename: 'artifact-comments.js'});
  // documentElement rides along so tests can assert the layer never writes it.
  return {window, head, body, documentElement, listeners};
}

function clickElement(el) {
  assert.ok(el._listeners.click && el._listeners.click.length > 0, 'click listener exists');
  return el._listeners.click[0]({preventDefault() {}, stopPropagation() {}});
}

function waitForPromises() {
  return new Promise((resolve) => setImmediate(resolve));
}

async function flushPromises(times = 3) {
  for (let i = 0; i < times; i++) await waitForPromises();
}

function findChildByClass(parent, className) {
  return parent.children.find((child) => child.className === className);
}

function dockOf(body) {
  return findChildByClass(body, '__cbc-dock');
}

function rectsIntersect(a, b) {
  return a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top;
}

function makeSessionStorage() {
  const data = {};
  return {
    getItem(key) { return key in data ? data[key] : null; },
    setItem(key, value) { data[key] = String(value); },
    removeItem(key) { delete data[key]; },
  };
}

function seedDraft(pathname, entries) {
  const artifactPath = decodeURIComponent(String(pathname || '').replace(/%2F/gi, '/'));
  const key = 'cbc-draft:' + artifactPath;
  const data = JSON.stringify(entries.map((e) => ({
    kind: e.kind || 'block',
    quote: String(e.quote == null ? '' : e.quote),
    context: String(e.context == null ? '' : e.context),
    comment: String(e.comment == null ? '' : e.comment),
  })));
  const storage = makeSessionStorage();
  storage.setItem(key, data);
  return storage;
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

test('resolveSessionId prefers a valid cbsession hash over the artifact path session', () => {
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(pathName, false, {hash: '#cbsession=view-session'});
  const resolve = window.__cbcResolveSessionId;

  assert.equal(resolve(pathName, '#cbsession=view-session'), 'view-session');
  assert.equal(resolve(pathName, '#cbsession=%20view-session%20'), 'view-session');
});

test('resolveSessionId keeps path-derived behavior when hash is absent', () => {
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(pathName);

  assert.equal(window.__cbcResolveSessionId(pathName, ''), 'path-session');
});

test('resolveSessionId falls back to the path for malformed cbsession hashes', () => {
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(pathName);
  const resolve = window.__cbcResolveSessionId;

  assert.equal(resolve(pathName, '#cbsession='), 'path-session');
  assert.equal(resolve(pathName, '#cbsession=%20'), 'path-session');
  assert.equal(resolve(pathName, '#cbsession=bad%2Fsession'), 'path-session');
});

test('batch tray labels and POST target use the hash session when it resolves', async () => {
  const calls = [];
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {body} = loadArtifactCommentsScript(pathName, false, {
    hash: '#cbsession=view-session',
    fetch: async (url, options = {}) => {
      calls.push({url, method: options.method || 'GET'});
      if (url === '/api/sessions/view-session') {
        return {ok: true, status: 200, async json() { return {name: 'Viewing Session'}; }};
      }
      if (url === '/api/chat/view-session/message') {
        return {ok: true, status: 200};
      }
      throw new Error('Unexpected fetch: ' + url);
    },
  });

  const shortcuts = findChildByClass(dockOf(body), '__cbc-shortcuts');
  const shortcutBtn = findChildByClass(shortcuts, '__cbc-shortcut');
  clickElement(shortcutBtn);
  await flushPromises();

  const tray = findChildByClass(dockOf(body), '__cbc-tray');
  const header = findChildByClass(tray, '__cbc-tray-header');
  const actions = findChildByClass(tray, '__cbc-tray-actions');
  const sendBtn = findChildByClass(actions, '__cbc-tray-send');
  assert.equal(header.textContent, 'Pending comments (1) \u2192 Viewing Session');
  assert.equal(sendBtn.textContent, 'Send 1 \u2192 Viewing Session');

  await clickElement(sendBtn);
  assert.ok(calls.some((call) => call.url === '/api/chat/view-session/message' && call.method === 'POST'));
});

test('hash session name 404 falls back to the artifact path session', async () => {
  const calls = [];
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {body} = loadArtifactCommentsScript(pathName, false, {
    hash: '#cbsession=missing-session',
    console: {warn() {}, error() {}},
    fetch: async (url, options = {}) => {
      calls.push({url, method: options.method || 'GET'});
      if (url === '/api/sessions/missing-session') {
        return {ok: false, status: 404};
      }
      if (url === '/api/sessions/path-session') {
        return {ok: true, status: 200, async json() { return {name: 'Path Session'}; }};
      }
      if (url === '/api/chat/path-session/message') {
        return {ok: true, status: 200};
      }
      throw new Error('Unexpected fetch: ' + url);
    },
  });

  const shortcuts = findChildByClass(dockOf(body), '__cbc-shortcuts');
  const shortcutBtn = findChildByClass(shortcuts, '__cbc-shortcut');
  clickElement(shortcutBtn);
  await flushPromises(5);

  const tray = findChildByClass(dockOf(body), '__cbc-tray');
  const header = findChildByClass(tray, '__cbc-tray-header');
  const actions = findChildByClass(tray, '__cbc-tray-actions');
  const sendBtn = findChildByClass(actions, '__cbc-tray-send');
  assert.equal(header.textContent, 'Pending comments (1) \u2192 Path Session');
  assert.equal(sendBtn.textContent, 'Send 1 \u2192 Path Session');

  await clickElement(sendBtn);
  assert.ok(calls.some((call) => call.url === '/api/chat/path-session/message' && call.method === 'POST'));
  assert.equal(calls.some((call) => call.url === '/api/chat/missing-session/message'), false);
});

test('artifact comments script stays inert inside frames', () => {
  const {window, head, listeners} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html',
    true
  );

  assert.equal(window.__cbcExtractSessionIdFromPath, undefined);
  assert.equal(window.__cbcResolveSessionId, undefined);
  assert.equal(window.__cbcBuildBatchMessage, undefined);
  assert.equal(window.__cbcBuildTrayItem, undefined);
  assert.equal(window.__cbcFindBlock, undefined);
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

test('buildBatchMessage preserves newline quote and comment content', () => {
  const artifactPath = '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(artifactPath);
  const buildBatchMessage = window.__cbcBuildBatchMessage;
  const entries = [
    {
      quote: 'Checklist bullet\nRun fenced and gate hung.',
      context: 'Risks',
      comment: 'Comment on checklist row',
    },
    {
      quote: 'Second quoted block text',
      context: 'Plan',
      comment: 'First line\nsecond line',
    },
  ];

  const message = buildBatchMessage(entries);

  assert.equal(
    message,
    [
      '[Artifact comments \u00B7 ' + artifactPath + '] (2)',
      '',
      '1. \u25B8 Risks \u203A "Checklist bullet',
      '   Run fenced and gate hung."',
      '   \u21B3 Comment on checklist row',
      '',
      '2. \u25B8 Plan \u203A "Second quoted block text"',
      '   \u21B3 First line',
      '   second line',
    ].join('\n')
  );
  assert.equal(
    message.split('\n').filter((line) => line.trimStart().startsWith('\u21B3 ')).length,
    entries.length
  );
});

test('buildBatchMessage renders no-quote Improve entries with context only', () => {
  const artifactPath = '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(artifactPath);
  const buildBatchMessage = window.__cbcBuildBatchMessage;
  const entries = [
    {
      kind: 'improve',
      quote: '',
      context: 'Improve',
      comment: 'Think from scratch, how to improve this?',
    },
  ];

  const message = buildBatchMessage(entries);

  assert.equal(
    message,
    [
      '[Artifact comments \u00B7 ' + artifactPath + '] (1)',
      '',
      '1. \u25B8 Improve',
      '   \u21B3 Think from scratch, how to improve this?',
    ].join('\n')
  );
});

test('comment trigger avoids overlapping artifact shortcut controls', () => {
  const {window, body, listeners} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );
  const shortcuts = findChildByClass(dockOf(body), '__cbc-shortcuts');
  assert.ok(shortcuts, 'shortcut controls are installed');

  const shortcutRect = {left: 960, top: 700, right: 1018, bottom: 760};
  shortcuts.getBoundingClientRect = () => shortcutRect;

  const block = makeBlock('Full height plan section');
  block.getBoundingClientRect = () => ({left: 0, top: 760, right: 2000, bottom: 1600});
  const naturalTriggerRect = {left: 982, top: 726, right: 1016, bottom: 760};
  assert.equal(rectsIntersect(naturalTriggerRect, shortcutRect), true, 'test setup overlaps naturally');

  const mouseover = listeners.find((listener) => (
    listener.target === 'document' && listener.type === 'mouseover'
  ));
  mouseover.handler({target: block});

  const trigger = body.children.find((child) => child.className === '__cbc-trigger');
  assert.ok(trigger, 'comment trigger is installed');
  const left = Number.parseFloat(trigger.style.left);
  const top = Number.parseFloat(trigger.style.top);
  const triggerRect = {left, top, right: left + 34, bottom: top + 34};

  assert.equal(rectsIntersect(triggerRect, shortcutRect), false);
  assert.ok(triggerRect.left >= 8);
  assert.ok(triggerRect.top >= 8);
  assert.ok(triggerRect.right <= window.innerWidth - 8);
  assert.ok(triggerRect.bottom <= window.innerHeight - 8);
});

test('dock owns the corner coordinates for the shortcut column and the tray', () => {
  const {head, body} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );

  const dock = dockOf(body);
  assert.ok(dock, '__cbc-dock is installed as a direct child of body');

  const shortcuts = findChildByClass(dock, '__cbc-shortcuts');
  const tray = findChildByClass(dock, '__cbc-tray');
  assert.ok(shortcuts, 'shortcut column is a child of the dock');
  assert.ok(tray, 'tray is a child of the dock');
  assert.equal(shortcuts.parentNode, dock, 'shortcuts and tray share the dock as parent');
  assert.equal(tray.parentNode, dock, 'tray and shortcuts share the dock as parent');
  assert.equal(shortcuts.parentNode, tray.parentNode, 'shortcuts and tray share one parent node');

  const styleText = head.children[0].textContent;
  const shortcutsRule = cssRule(styleText, '.__cbc-shortcuts');
  const trayRule = cssRule(styleText, '.__cbc-tray');
  assert.doesNotMatch(shortcutsRule, /position:/, 'shortcuts rule carries no position declaration');
  assert.doesNotMatch(shortcutsRule, /right:/, 'shortcuts rule carries no right declaration');
  assert.doesNotMatch(shortcutsRule, /bottom:/, 'shortcuts rule carries no bottom declaration');
  assert.doesNotMatch(shortcutsRule, /z-index:/, 'shortcuts rule carries no z-index declaration');
  assert.doesNotMatch(trayRule, /position:/, 'tray rule carries no position declaration');
  assert.doesNotMatch(trayRule, /right:/, 'tray rule carries no right declaration');
  assert.doesNotMatch(trayRule, /bottom:/, 'tray rule carries no bottom declaration');
  assert.doesNotMatch(trayRule, /z-index:/, 'tray rule carries no z-index declaration');

  const dockRule = cssRule(styleText, '.__cbc-dock');
  assert.match(dockRule, /position:fixed/, 'dock owns the corner position');
  assert.match(dockRule, /right:14px/, 'dock owns the right offset');
  assert.match(dockRule, /bottom:64px/, 'dock owns the bottom offset');
  assert.match(dockRule, /z-index:2147483646/, 'dock owns the stacking layer');

  const triggerRule = cssRule(styleText, '.__cbc-trigger');
  assert.match(triggerRule, /z-index:2147483645/, 'trigger sits one level below the dock');
});

function loadFindBlock() {
  const {window} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );
  assert.equal(typeof window.__cbcFindBlock, 'function');
  return window.__cbcFindBlock;
}

test('findBlock returns a block div that owns its own text', () => {
  const findBlock = loadFindBlock();
  const block = makeBlock('Hello world');
  assert.equal(findBlock(block), block);
});

test('findBlock skips a pure wrapper whose direct children are only blocks + whitespace', () => {
  const findBlock = loadFindBlock();
  const inner = makeBlock('Inner text');
  const wrapper = makeBlock('', {childNodes: [makeTextNode('\n  '), inner, makeTextNode('\n')]});
  const owner = makeBlock('Owner text', {childNodes: [wrapper]});

  // Hovering the wrapper itself: it owns no text, so the climb continues to the owner.
  assert.equal(findBlock(wrapper), owner);
});

test('findBlock climbs from an inline span that owns text to the nearest owning block', () => {
  const findBlock = loadFindBlock();
  const span = makeBlock('inline fragment', {display: 'inline'});
  const block = makeBlock('Block text', {childNodes: [span]});

  assert.equal(findBlock(span), block);
});

test('findBlock keeps a td commentable even when it contains a display:block child (regression)', () => {
  const findBlock = loadFindBlock();
  const small = makeBlock('note', {display: 'block'});
  const td = makeBlock('KR text', {display: 'table-cell', childNodes: [small]});

  assert.equal(findBlock(td), td);
  // The display:block <small> owns its own text, so it is independently commentable.
  assert.equal(findBlock(small), small);
});

test('findBlock returns pre for nested code and td for blank-area target in a code-only table cell', () => {
  const findBlock = loadFindBlock();
  const code = makeBlock('git status --short', {display: 'inline'});
  const pre = makeBlock('', {childNodes: [code]});
  pre.tagName = 'PRE';
  const td = makeBlock('', {display: 'table-cell', childNodes: [pre]});
  td.tagName = 'TD';

  assert.equal(findBlock(code), pre);
  assert.equal(findBlock(pre), pre);
  assert.equal(findBlock(td), td);
});

test('findBlock still returns pre and li blocks that own text', () => {
  const findBlock = loadFindBlock();
  const pre = makeBlock('code line');
  const li = makeBlock('list item');

  assert.equal(findBlock(pre), pre);
  assert.equal(findBlock(li), li);
});


test('resolveEntryDraft returns default comment lines when no draft override', () => {
  const {window} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );

  const resolveEntryDraft = window.__cbcResolveEntryDraft;
  assert.equal(typeof resolveEntryDraft, 'function');

  const entry = {quote: 'quoted text', context: 'Context', comment: 'comment text'};
  const lines = resolveEntryDraft(entry);
  assert.equal(lines.length, 2);
  assert.equal(lines[0], '▸ Context › "quoted text"');
  assert.equal(lines[1], '↳ comment text');
});

function cssRule(styleText, selector) {
  const start = styleText.indexOf(selector + '{');
  assert.notEqual(start, -1, selector + ' rule exists');
  const end = styleText.indexOf('}', start);
  assert.notEqual(end, -1, selector + ' rule closes');
  return styleText.slice(start, end + 1);
}

test('tray item layout keeps controls in normal flow beside bounded preview text', () => {
  const {window, head} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );
  const buildTrayItem = window.__cbcBuildTrayItem;
  assert.equal(typeof buildTrayItem, 'function');

  const entry = {
    quote: 'A very long quote '.repeat(20),
    context: 'Context',
    comment: 'A very long outbound comment preview '.repeat(30),
  };
  const item = buildTrayItem(0, entry);

  assert.equal(item.className, '__cbc-tray-item');
  assert.equal(item.children.length, 1);
  const main = item.children[0];
  assert.equal(main.className, '__cbc-tray-item-main');
  assert.equal(main.children.length, 2);

  const body = main.children[0];
  const controls = main.children[1];
  assert.equal(body.className, '__cbc-tray-item-body');
  assert.equal(controls.className, '__cbc-tray-item-controls');
  assert.deepEqual(
    controls.children.map((child) => child.className),
    ['__cbc-tray-remove', '__cbc-tray-edit-btn']
  );

  const draft = body.querySelector('.__cbc-tray-item-comment');
  assert.ok(draft, 'draft preview exists in the text column');
  assert.equal(draft.textContent, entry.comment);

  const styleText = head.children[0].textContent;
  assert.match(cssRule(styleText, '.__cbc-tray-item-main'), /display:flex/);
  assert.match(cssRule(styleText, '.__cbc-tray-item-body'), /min-width:0/);
  assert.doesNotMatch(cssRule(styleText, '.__cbc-tray-item-comment'), /-webkit-line-clamp/);
  assert.match(cssRule(styleText, '.__cbc-tray-item-comment'), /overflow-wrap:anywhere/);
  assert.doesNotMatch(cssRule(styleText, '.__cbc-tray-edit-btn'), /position:absolute/);
  assert.doesNotMatch(cssRule(styleText, '.__cbc-tray-remove'), /position:absolute/);

  const quoteRule = cssRule(styleText, '.__cbc-tray-item-quote');
  const commentRule = cssRule(styleText, '.__cbc-tray-item-comment');
  for (const token of ['line-clamp', 'max-height', 'nowrap', 'text-overflow']) {
    assert.doesNotMatch(quoteRule, new RegExp(token), 'quote rule omits ' + token);
    assert.doesNotMatch(commentRule, new RegExp(token), 'comment rule omits ' + token);
  }
  assert.match(quoteRule, /overflow-wrap:anywhere/, 'quote rule still wraps long words');
});

test('tray item rule refuses to shrink so cards keep natural height and the list scrolls', () => {
  const {head} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );
  const styleText = head.children[0].textContent;
  const itemRule = cssRule(styleText, '.__cbc-tray-item');
  assert.match(itemRule, /flex-shrink:0/, 'tray item carries a no-shrink declaration');
  const listRule = cssRule(styleText, '.__cbc-tray-list');
  assert.match(listRule, /max-height:320px/, 'list keeps the 320px cap');
  assert.match(listRule, /overflow:auto/, 'list keeps overflow:auto so it scrolls');
});

test('popover and tray edit windows are widened and share a single width source', () => {
  const {head} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );
  const styleText = head.children[0].textContent;

  assert.match(ARTIFACT_COMMENTS_JS, /var POPOVER_WIDTH = 460;/, 'POPOVER_WIDTH constant is defined once');
  assert.match(
    ARTIFACT_COMMENTS_JS,
    /Math\.min\(POPOVER_WIDTH, window\.innerWidth - 16\)/,
    'positionPopover uses the POPOVER_WIDTH constant'
  );
  assert.doesNotMatch(ARTIFACT_COMMENTS_JS, /Math\.min\(300,/, 'no literal 300 popover width remains in positioning math');

  const popover = cssRule(styleText, '.__cbc-popover');
  assert.ok(popover.includes('min(460px,calc(100vw - 16px))'), 'popover width uses the constant value');
  assert.doesNotMatch(popover, /min\(300px/, 'popover rule no longer hardcodes 300px');

  const popoverTextarea = cssRule(styleText, '.__cbc-popover textarea');
  assert.match(popoverTextarea, /min-height:140px/, 'popover textarea is widened to 140px');
  assert.match(popoverTextarea, /resize:vertical/, 'popover textarea keeps resize:vertical');

  const tray = cssRule(styleText, '.__cbc-tray');
  assert.ok(tray.includes('min(400px'), 'tray width is widened to 400px');
  assert.ok(tray.includes('calc(100vw - 28px)'), 'tray width keeps the narrow-screen cap');

  const trayList = cssRule(styleText, '.__cbc-tray-list');
  assert.match(trayList, /max-height:320px/, 'tray list is widened to 320px');

  const trayEdit = cssRule(styleText, '.__cbc-tray-edit');
  assert.match(trayEdit, /min-height:130px/, 'tray edit textarea is widened to 130px');
  assert.match(trayEdit, /resize:vertical/, 'tray edit textarea keeps resize:vertical');

  assert.doesNotMatch(styleText, /__cbc-tray-edited/, 'the draftText-keyed edited indicator rule is gone');
});

// ---------------------------------------------------------------------------
// Panel review marker guard change (C3)
// ---------------------------------------------------------------------------

test('comment tray activates inside a framed page when the fragment carries the panel marker', () => {
  const {window, head, listeners} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html',
    true,
    {hash: '#cbsession=session-270&cbpanel=1'}
  );

  assert.equal(typeof window.__cbcExtractSessionIdFromPath, 'function');
  assert.equal(typeof window.__cbcResolveSessionId, 'function');
  assert.equal(typeof window.__cbcBuildBatchMessage, 'function');
  assert.equal(typeof window.__cbcBuildTrayItem, 'function');
  assert.equal(typeof window.__cbcFindBlock, 'function');
  assert.ok(head.children.length > 0, 'styles are installed');
  assert.ok(listeners.length > 0, 'listeners are installed');
});

test('comment tray stays inactive inside a framed page without the panel marker', () => {
  const {window, head, listeners} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html',
    true,
    {hash: '#cbsession=session-270'}
  );

  assert.equal(window.__cbcExtractSessionIdFromPath, undefined);
  assert.equal(window.__cbcResolveSessionId, undefined);
  assert.equal(window.__cbcBuildBatchMessage, undefined);
  assert.equal(head.children.length, 0);
  assert.equal(listeners.length, 0);
});

test('comment tray is active when not framed regardless of marker (unchanged)', () => {
  const {window, head} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html',
    false
  );

  assert.equal(typeof window.__cbcExtractSessionIdFromPath, 'function');
  assert.ok(head.children.length > 0, 'styles are installed');
});

test('resolveSessionId extracts the session from a fragment with the panel marker', () => {
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(pathName, false, {
    hash: '#cbsession=marker-session&cbpanel=1',
  });
  const resolve = window.__cbcResolveSessionId;

  assert.equal(resolve(pathName, '#cbsession=marker-session&cbpanel=1'), 'marker-session');
});

test('resolveSessionId still extracts the session from a plain cbsession fragment without marker', () => {
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(pathName);
  const resolve = window.__cbcResolveSessionId;

  assert.equal(resolve(pathName, '#cbsession=plain-session'), 'plain-session');
});

// ---------------------------------------------------------------------------
// Framed live-target routing: when embedded in the plan panel, the tray must
// POST to window.parent.planPanel.currentSessionId() (the live active session)
// whenever it returns a truthy value, and fall back to the artifact-URL-derived
// session otherwise. The displayed label must always agree with the POST target.
// ---------------------------------------------------------------------------

function pendingTrayParts(body) {
  const shortcuts = findChildByClass(dockOf(body), '__cbc-shortcuts');
  const shortcutBtn = findChildByClass(shortcuts, '__cbc-shortcut');
  clickElement(shortcutBtn);
  const tray = findChildByClass(dockOf(body), '__cbc-tray');
  return {
    header: findChildByClass(tray, '__cbc-tray-header'),
    sendBtn: findChildByClass(findChildByClass(tray, '__cbc-tray-actions'), '__cbc-tray-send'),
  };
}

test('framed tray POSTs to the live parent session when it differs from the artifact-URL session', async () => {
  const calls = [];
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {body} = loadArtifactCommentsScript(pathName, true, {
    hash: '#cbsession=path-session&cbpanel=1',
    parent: {planPanel: {currentSessionId: () => 'live-session'}},
    fetch: async (url, options = {}) => {
      calls.push({url, method: options.method || 'GET'});
      if (url === '/api/sessions/live-session') {
        return {ok: true, status: 200, async json() { return {name: 'Live Session'}; }};
      }
      if (url === '/api/chat/live-session/message') {
        return {ok: true, status: 200};
      }
      throw new Error('Unexpected fetch: ' + url);
    },
  });

  const {header, sendBtn} = pendingTrayParts(body);
  await flushPromises(5);
  assert.equal(header.textContent, 'Pending comments (1) \u2192 Live Session');
  assert.equal(sendBtn.textContent, 'Send 1 \u2192 Live Session');

  await clickElement(sendBtn);
  assert.ok(calls.some((call) => call.url === '/api/chat/live-session/message' && call.method === 'POST'),
    'POST targets the live parent session');
  assert.equal(calls.some((call) => call.url === '/api/chat/path-session/message'), false,
    'POST does not target the stale artifact-URL session');
});

test('framed tray falls back to the artifact session when window.parent.planPanel is absent', async () => {
  const calls = [];
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {body} = loadArtifactCommentsScript(pathName, true, {
    hash: '#cbsession=path-session&cbpanel=1',
    // No opts.parent → window.parent is the bare {} from the harness default.
    fetch: async (url, options = {}) => {
      calls.push({url, method: options.method || 'GET'});
      if (url === '/api/sessions/path-session') {
        return {ok: true, status: 200, async json() { return {name: 'Path Session'}; }};
      }
      if (url === '/api/chat/path-session/message') {
        return {ok: true, status: 200};
      }
      throw new Error('Unexpected fetch: ' + url);
    },
  });

  const {header, sendBtn} = pendingTrayParts(body);
  await flushPromises(5);
  assert.equal(header.textContent, 'Pending comments (1) \u2192 Path Session');
  assert.equal(sendBtn.textContent, 'Send 1 \u2192 Path Session');

  await clickElement(sendBtn);
  assert.ok(calls.some((call) => call.url === '/api/chat/path-session/message' && call.method === 'POST'),
    'POST falls back to the artifact-URL session');
});

test('framed tray falls back to the artifact session when currentSessionId throws', async () => {
  const calls = [];
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {body} = loadArtifactCommentsScript(pathName, true, {
    hash: '#cbsession=path-session&cbpanel=1',
    parent: {planPanel: {currentSessionId: () => { throw new Error('cross-window boom'); }}},
    console: {warn() {}, error() {}, log() {}},
    fetch: async (url, options = {}) => {
      calls.push({url, method: options.method || 'GET'});
      if (url === '/api/sessions/path-session') {
        return {ok: true, status: 200, async json() { return {name: 'Path Session'}; }};
      }
      if (url === '/api/chat/path-session/message') {
        return {ok: true, status: 200};
      }
      throw new Error('Unexpected fetch: ' + url);
    },
  });

  const {header, sendBtn} = pendingTrayParts(body);
  await flushPromises(5);
  assert.equal(header.textContent, 'Pending comments (1) \u2192 Path Session');
  assert.equal(sendBtn.textContent, 'Send 1 \u2192 Path Session');

  await clickElement(sendBtn);
  assert.ok(calls.some((call) => call.url === '/api/chat/path-session/message' && call.method === 'POST'),
    'POST falls back when the live accessor throws');
});

test('framed tray falls back to the artifact session when currentSessionId returns a falsy value', async () => {
  const calls = [];
  const pathName = '/files/data/home/chaoli/.charliebot/sessions/path-session/artifacts/plan.html';
  const {body} = loadArtifactCommentsScript(pathName, true, {
    hash: '#cbsession=path-session&cbpanel=1',
    parent: {planPanel: {currentSessionId: () => null}},
    fetch: async (url, options = {}) => {
      calls.push({url, method: options.method || 'GET'});
      if (url === '/api/sessions/path-session') {
        return {ok: true, status: 200, async json() { return {name: 'Path Session'}; }};
      }
      if (url === '/api/chat/path-session/message') {
        return {ok: true, status: 200};
      }
      throw new Error('Unexpected fetch: ' + url);
    },
  });

  const {header, sendBtn} = pendingTrayParts(body);
  await flushPromises(5);
  assert.equal(header.textContent, 'Pending comments (1) \u2192 Path Session');
  assert.equal(sendBtn.textContent, 'Send 1 \u2192 Path Session');

  await clickElement(sendBtn);
  assert.ok(calls.some((call) => call.url === '/api/chat/path-session/message' && call.method === 'POST'),
    'POST falls back when the live accessor returns null');
});

// ---------------------------------------------------------------------------
// Shortcut buttons: one per SHORTCUTS entry, each deduped on its own kind
// ---------------------------------------------------------------------------

const SHORTCUT_PATH = '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html';

function loadWithShortcuts(width = 1024) {
  return loadArtifactCommentsScript(SHORTCUT_PATH, false, {
    innerWidth: width,
    console: {warn() {}, error() {}},
    fetch: async () => ({ok: true, status: 200, async json() { return {name: 'S'}; }}),
  });
}

function shortcutButtons(body) {
  const shortcuts = findChildByClass(dockOf(body), '__cbc-shortcuts');
  return shortcuts.children.filter((child) => child.className === '__cbc-shortcut');
}

function trayItemCount(body) {
  const tray = findChildByClass(dockOf(body), '__cbc-tray');
  const list = findChildByClass(tray, '__cbc-tray-list');
  return list.children.filter((child) => child.className === '__cbc-tray-item').length;
}

test('shortcut tray renders one button per shortcut: Improve, Shorten, Verify', () => {
  const {body} = loadWithShortcuts();
  const buttons = shortcutButtons(body);

  assert.equal(buttons.map((button) => button.textContent).join(','), 'Improve,Shorten,Verify');
  for (const button of buttons) {
    assert.ok(button.title.length > 0, 'button carries its prompt as the tooltip');
    assert.equal(button.attributes['aria-label'], button.title);
  }
});

test('each shortcut dedups on its own kind without blocking the other shortcuts', () => {
  const {body} = loadWithShortcuts(800);
  const [improve, shorten, verify] = shortcutButtons(body);

  clickElement(shorten);
  clickElement(shorten);
  assert.equal(trayItemCount(body), 1, 'a second Shorten click is a no-op');

  clickElement(verify);
  clickElement(improve);
  assert.equal(trayItemCount(body), 3, 'all three shortcuts coexist in one batch');

  clickElement(verify);
  clickElement(improve);
  assert.equal(trayItemCount(body), 3, 'repeat clicks stay deduped per kind');
});

// ---------------------------------------------------------------------------
// stackCards: pure greedy placement for the anchored comment gutter (part 1).
// Four properties must hold jointly: order preserved, no overlap, never floats
// up, and exact when uncontested. "no overlap" and "order preserved" are driven
// with 200 randomised anchor/height sets; the property is asserted, never a
// pixel literal.
// ---------------------------------------------------------------------------

function mulberry32(seed) {
  let s = seed | 0;
  return function () {
    s = (s + 0x6D2B79F5) | 0;
    let t = Math.imul(s ^ (s >>> 15), 1 | s);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function loadStackCards() {
  const {window} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );
  assert.equal(typeof window.__cbcStackCards, 'function', 'stackCards is exported');
  return window.__cbcStackCards;
}

function randomisedSets(rng, sets) {
  const out = [];
  for (let s = 0; s < sets; s++) {
    const n = 1 + Math.floor(rng() * 8);
    const anchors = [];
    const heights = [];
    for (let i = 0; i < n; i++) {
      anchors.push(Math.floor(rng() * 1000));
      heights.push(1 + Math.floor(rng() * 200));
    }
    const gap = Math.floor(rng() * 20);
    out.push({anchors, heights, gap});
  }
  return out;
}

function sortByAnchor(anchors, heights) {
  const pairs = anchors.map((a, i) => [a, heights[i]]).sort((x, y) => x[0] - y[0]);
  return {
    sortedAnchors: pairs.map((p) => p[0]),
    sortedHeights: pairs.map((p) => p[1]),
  };
}

test('stackCards preserves ascending-anchor order over 200 randomised sets', () => {
  const stackCards = loadStackCards();
  const rng = mulberry32(20260728);
  for (const {anchors, heights, gap} of randomisedSets(rng, 200)) {
    const tops = stackCards(anchors, heights, gap);
    // Output is in ascending-anchor order, so placed tops are non-decreasing.
    for (let i = 1; i < tops.length; i++) {
      assert.ok(tops[i] >= tops[i - 1], 'tops non-decreasing: ' + JSON.stringify({anchors, tops}));
    }
  }
});

test('stackCards prevents overlap over 200 randomised sets', () => {
  const stackCards = loadStackCards();
  const rng = mulberry32(20260729);
  for (const {anchors, heights, gap} of randomisedSets(rng, 200)) {
    const tops = stackCards(anchors, heights, gap);
    const {sortedHeights} = sortByAnchor(anchors, heights);
    for (let i = 1; i < tops.length; i++) {
      assert.ok(
        tops[i] >= tops[i - 1] + sortedHeights[i - 1] + gap,
        'no overlap: ' + JSON.stringify({anchors, heights, gap, tops})
      );
    }
  }
});

test('stackCards never floats a card above its own anchor', () => {
  const stackCards = loadStackCards();
  const rng = mulberry32(20260730);
  for (const {anchors, heights, gap} of randomisedSets(rng, 200)) {
    const tops = stackCards(anchors, heights, gap);
    const {sortedAnchors} = sortByAnchor(anchors, heights);
    for (let i = 0; i < tops.length; i++) {
      assert.ok(tops[i] >= sortedAnchors[i], 'top >= anchor: ' + JSON.stringify({anchors, tops, i}));
    }
  }
});

test('stackCards places an uncontested card exactly at its anchor', () => {
  const stackCards = loadStackCards();
  // Deterministic sanity: well-separated anchors never collide, so every card
  // lands on its own anchor (the "exact when uncontested" property, not a pixel
  // literal). Compared via JSON because the array is created inside the vm
  // context and has a different prototype than the test's outer Array.
  const detAnchors = [100, 500, 900];
  const detTops = stackCards(detAnchors, [40, 40, 40], 10);
  assert.equal(JSON.stringify(detTops), JSON.stringify(detAnchors));
  // Randomised: any card that does not collide with the previous lands on its anchor.
  const rng = mulberry32(20260731);
  for (const {anchors, heights, gap} of randomisedSets(rng, 200)) {
    const tops = stackCards(anchors, heights, gap);
    const {sortedAnchors, sortedHeights} = sortByAnchor(anchors, heights);
    for (let i = 1; i < tops.length; i++) {
      if (sortedAnchors[i] >= tops[i - 1] + sortedHeights[i - 1] + gap) {
        assert.equal(tops[i], sortedAnchors[i], 'uncontested at anchor: ' + JSON.stringify({anchors, heights, gap, tops, i}));
      }
    }
  }
});

// ---------------------------------------------------------------------------
// fitColumn / chooseWidth: the margin column's two new pure functions, exported
// as window.__cbcFitColumn / window.__cbcChooseWidth alongside __cbcStackCards.
// Properties are asserted over seeded randomised sets, never pixel literals:
// adjacent pairs stay at least gap apart, the last card lies wholly inside the
// band, the cap only ever raises a card, the chosen width fits with a gap on
// both sides and lands in [240,300], and it never narrows as the window widens.
// fitColumn's input contract is the document-space stack in sorted anchor
// order, with bands large enough for the anchored regime (band >= total).
// ---------------------------------------------------------------------------

function loadFitColumnAndChooseWidth() {
  const {window} = loadArtifactCommentsScript(SHORTCUT_PATH);
  assert.equal(typeof window.__cbcStackCards, 'function', 'stackCards is exported');
  assert.equal(typeof window.__cbcFitColumn, 'function', 'fitColumn is exported');
  assert.equal(typeof window.__cbcChooseWidth, 'function', 'chooseWidth is exported');
  return {
    stackCards: window.__cbcStackCards,
    fitColumn: window.__cbcFitColumn,
    chooseWidth: window.__cbcChooseWidth,
  };
}

// Anchored-regime inputs: anchors and heights sorted together into anchor
// order, a band at least as tall as the whole stack, and any scroll offset.
function randomisedAnchoredSets(rng, sets) {
  const out = [];
  for (let s = 0; s < sets; s++) {
    const {anchors, heights, gap} = randomisedSets(rng, 1)[0];
    const {sortedAnchors, sortedHeights} = sortByAnchor(anchors, heights);
    let total = 0;
    for (let i = 0; i < sortedHeights.length; i++) total += sortedHeights[i] + (i ? gap : 0);
    out.push({
      anchors: sortedAnchors,
      heights: sortedHeights,
      gap,
      band: total + Math.floor(rng() * 400),
      scrolled: Math.floor(rng() * 2000) - 500,
    });
  }
  return out;
}

test('fitColumn keeps every adjacent pair at least gap apart over 300 randomised sets', () => {
  const {stackCards, fitColumn} = loadFitColumnAndChooseWidth();
  const rng = mulberry32(20260732);
  for (const {anchors, heights, gap, band, scrolled} of randomisedAnchoredSets(rng, 300)) {
    const docTops = stackCards(anchors, heights, gap);
    const tops = fitColumn(docTops, heights, gap, band, scrolled);
    for (let i = 1; i < tops.length; i++) {
      assert.ok(
        tops[i] >= tops[i - 1] + heights[i - 1] + gap - 1e-9,
        'adjacent pair kept >= gap apart: ' + JSON.stringify({anchors, heights, gap, band, scrolled, tops, i})
      );
    }
  }
});

test('fitColumn leaves the last card wholly inside the band over 300 randomised sets', () => {
  const {stackCards, fitColumn} = loadFitColumnAndChooseWidth();
  const rng = mulberry32(20260733);
  for (const {anchors, heights, gap, band, scrolled} of randomisedAnchoredSets(rng, 300)) {
    const docTops = stackCards(anchors, heights, gap);
    const tops = fitColumn(docTops, heights, gap, band, scrolled);
    const last = tops.length - 1;
    // The cap makes room from the bottom edge. There is deliberately no floor
    // on the top edge: an anchor scrolled above the band takes its card with it.
    assert.ok(
      tops[last] + heights[last] <= band + 1e-9,
      'last card bottom inside the band: ' + JSON.stringify({anchors, heights, gap, band, scrolled, tops})
    );
  }
});

test('fitColumn only ever raises a card, never lowers it, over 300 randomised sets', () => {
  const {stackCards, fitColumn} = loadFitColumnAndChooseWidth();
  const rng = mulberry32(20260734);
  for (const {anchors, heights, gap, band, scrolled} of randomisedAnchoredSets(rng, 300)) {
    const docTops = stackCards(anchors, heights, gap);
    const tops = fitColumn(docTops, heights, gap, band, scrolled);
    for (let i = 0; i < tops.length; i++) {
      assert.ok(
        tops[i] <= docTops[i] - scrolled + 1e-9,
        'the cap never pushed a card down: ' + JSON.stringify({anchors, heights, gap, band, scrolled, docTops, tops, i})
      );
    }
  }
});

test('fitColumn keeps the r7 regression input at least gap apart (no floor guard)', () => {
  const {stackCards, fitColumn} = loadFitColumnAndChooseWidth();
  // r7 pinned the top card to 0 here and drove it 42px into its neighbour;
  // fitColumn must have no floor guard, and adding one reintroduces that bug.
  const tops = fitColumn(stackCards([-50, 5], [100, 50], 8), [100, 50], 8, 600, 0);
  assert.ok(
    tops[1] - (tops[0] + 100) >= 8,
    'the two cards stay at least gap apart: tops=' + JSON.stringify(tops)
  );
});

test('chooseWidth fits with a gap on both sides and lands in [240,300] over 300 randomised sets', () => {
  const {chooseWidth} = loadFitColumnAndChooseWidth();
  const rng = mulberry32(20260735);
  for (let s = 0; s < 300; s++) {
    const clientWidth = 320 + Math.floor(rng() * 2400);
    const contentRight = Math.floor(rng() * clientWidth);
    const w = chooseWidth(clientWidth, contentRight, 8, 240, 300);
    if (!w) continue; // margin cannot hold the narrowest column: the corner list takes over
    assert.ok(w >= 240 && w <= 300, 'chosen width lands in [240,300]: ' + JSON.stringify({clientWidth, contentRight, w}));
    assert.ok(
      contentRight + 8 + w + 8 <= clientWidth + 1e-9,
      'chosen column leaves a gap on both sides: ' + JSON.stringify({clientWidth, contentRight, w})
    );
  }
});

test('chooseWidth never narrows as the window widens over 300 randomised sets', () => {
  const {chooseWidth} = loadFitColumnAndChooseWidth();
  const rng = mulberry32(20260736);
  for (let s = 0; s < 300; s++) {
    const clientWidth = 320 + Math.floor(rng() * 2400);
    const contentRight = Math.floor(rng() * clientWidth);
    const w = chooseWidth(clientWidth, contentRight, 8, 240, 300);
    assert.ok(
      chooseWidth(clientWidth + 1, contentRight, 8, 240, 300) >= w,
      'widening the window never narrows the column: ' + JSON.stringify({clientWidth, contentRight, w})
    );
  }
});

// ---------------------------------------------------------------------------
// comment-gutter (part 2a): wiring stackCards to a right-hand gutter at >=900px.
// Gutter mode activates only when an anchored (el != null) entry exists, so the
// corner-list tests below never flip; the threshold still governs placement.
// ---------------------------------------------------------------------------

function addBlockComment(body, listeners, block, text) {
  const click = listeners.find((l) => l.target === 'document' && l.type === 'click').handler;
  click({target: block, preventDefault() {}, stopPropagation() {}});
  const popover = body.children.find((c) => c.className === '__cbc-popover');
  popover.children[0].value = text;
  const actions = popover.children.find((c) => c.className === '__cbc-actions');
  clickElement(actions.children.find((c) => c.textContent === 'Add'));
}

test('gutter cards are positioned by the stackCards pure function (render glue)', async () => {
  // Colliding anchors force stackCards to push cards apart; raw anchors would not match.
  // The rects also give the fake layout a 640px-wide article column so that
  // measureContent() keeps the one-band reserve clean and gutter mode engages.
  const tops = [100, 110, 120];
  const blocks = tops.map((t) => {
    const b = makeBlock('section at ' + t);
    b.getBoundingClientRect = () => ({left: 0, top: t, right: 640, bottom: t + 50, width: 640, height: 50});
    return b;
  });
  const {window, head, body, listeners} = loadArtifactCommentsScript(SHORTCUT_PATH, false, {
    bodyChildren: blocks,
    fetch: async () => ({ok: true, status: 200, async json() { return {name: 'S'}; }}),
  });
  const stackCards = window.__cbcStackCards;
  const gap = window.__cbcGutterGap;

  for (const b of blocks) addBlockComment(body, listeners, b, 'cmt');
  await flushPromises();

  const gutter = body.children.find((c) => c.className === '__cbc-gutter');
  assert.ok(gutter, 'gutter is installed in gutter mode');
  const cards = gutter.children.filter((c) => c.className === '__cbc-tray-item');
  assert.equal(cards.length, blocks.length, 'one gutter card per anchored entry');

  const anchors = blocks.map((b) => b.getBoundingClientRect().top + (window.scrollY || 0));
  const heights = cards.map((c) => c.offsetHeight);
  const expected = stackCards(anchors, heights, gap);
  for (let i = 0; i < cards.length; i++) {
    assert.equal(cards[i].style.top, expected[i] + 'px', 'card ' + i + ' top is the stackCards output');
  }

  const styleText = head.children[0].textContent;
  const gutterCardRule = cssRule(styleText, '.__cbc-gutter .__cbc-tray-item');
  assert.doesNotMatch(gutterCardRule, /max-height/, 'gutter cards never cap their own height');
  assert.doesNotMatch(gutterCardRule, /overflow:hidden/, 'gutter cards never clip their own content');
});

test('gutter mode never writes the artifact\'s own layout', async () => {
  // The mechanism that replaces the removed body-padding reserve: the layer
  // writes no artifact style at all. The body's and documentElement's style
  // attribute strings (el.attributes.style is this double's getAttribute) are
  // captured at entry and asserted identical at every step, including after
  // entry, which is where the old code reserved 316px of padding.
  const block = makeBlock('anchored section');
  block.getBoundingClientRect = () => ({left: 0, top: 200, right: 640, bottom: 250, width: 640, height: 50});
  const {window, body, documentElement, listeners} = loadArtifactCommentsScript(SHORTCUT_PATH, false, {
    bodyChildren: [block],
    fetch: async () => ({ok: true, status: 200, async json() { return {name: 'S'}; }}),
  });
  const fireResize = () => {
    for (const l of listeners) {
      if (l.target === 'window' && l.type === 'resize') l.handler();
    }
  };
  const contentRight = block.getBoundingClientRect().right;
  assert.ok(
    window.__cbcChooseWidth(documentElement.clientWidth, contentRight, window.__cbcGutterGap, 240, 300) >= 240,
    'the column engages in this fake layout'
  );
  // Entry happens at load (the decision depends only on the window) and the
  // layer runs synchronously inside loadArtifactCommentsScript, so the
  // attribute strings captured here are also the pre-entry ones.
  const bodyStyleAttr = body.attributes.style;
  const documentStyleAttr = documentElement.attributes.style;

  addBlockComment(body, listeners, block, 'cmt');
  await flushPromises();
  assert.equal(body.attributes.style, bodyStyleAttr, 'body style attribute identical after entry');
  assert.equal(documentElement.attributes.style, documentStyleAttr, 'documentElement style attribute identical after entry');
  assert.equal(body.style.paddingRight, undefined, 'paddingRight is never set');

  window.innerWidth = 800;
  fireResize();
  assert.equal(body.attributes.style, bodyStyleAttr, 'body style attribute identical below the threshold');
  assert.equal(documentElement.attributes.style, documentStyleAttr, 'documentElement style attribute identical below the threshold');
  assert.equal(body.style.paddingRight, undefined, 'paddingRight is never set on exit either');

  window.innerWidth = 1024;
  fireResize();
  assert.equal(body.attributes.style, bodyStyleAttr, 'body style attribute identical after re-entry');
  assert.equal(documentElement.attributes.style, documentStyleAttr, 'documentElement style attribute identical after re-entry');

  window.innerWidth = 800;
  fireResize();
  // A pre-existing inline paddingRight on the body belongs to the artifact: it
  // must survive entry and exit untouched, never reserved over, never emptied.
  body.style.paddingRight = '10px';
  window.innerWidth = 1024;
  fireResize();
  assert.equal(body.style.paddingRight, '10px', 'pre-existing inline paddingRight survives entry untouched');
  assert.equal(body.attributes.style, bodyStyleAttr, 'body style attribute still identical after entry with preset padding');
  window.innerWidth = 800;
  fireResize();
  assert.equal(body.style.paddingRight, '10px', 'pre-existing inline paddingRight survives exit untouched');
});

test('gutter mode aligns the dock to the column and restores its prior inline values on exit', async () => {
  // The mechanism that replaces the removed inline-right reserve: the bar is
  // never moved, it only aligns its left edge and width to the column, and on
  // exit every inline value returns to what was there before entry.
  const block = makeBlock('anchored section');
  block.getBoundingClientRect = () => ({left: 0, top: 200, right: 640, bottom: 250, width: 640, height: 50});
  const {window, body, documentElement, listeners} = loadArtifactCommentsScript(SHORTCUT_PATH, false, {
    bodyChildren: [block],
    fetch: async () => ({ok: true, status: 200, async json() { return {name: 'S'}; }}),
  });
  const fireResize = () => {
    for (const l of listeners) {
      if (l.target === 'window' && l.type === 'resize') l.handler();
    }
  };
  const dock = dockOf(body);
  const tray = findChildByClass(dock, '__cbc-tray');

  addBlockComment(body, listeners, block, 'cmt');
  await flushPromises();

  // Every expectation derives from the fake layout's own numbers: the bar's
  // left edge is the content's right edge plus the gap, its width is the width
  // chooseWidth picks for this layout, its right is auto, and the tray fills it.
  const contentRight = block.getBoundingClientRect().right;
  const gap = window.__cbcGutterGap;
  const columnWidth = window.__cbcChooseWidth(documentElement.clientWidth, contentRight, gap, 240, 300);
  assert.ok(columnWidth >= 240, 'the column engages in this fake layout');
  assert.equal(dock.style.left, contentRight + gap + 'px', 'dock left edge aligns to the column left edge');
  assert.equal(dock.style.right, 'auto', 'dock right is auto in the column');
  assert.equal(dock.style.width, columnWidth + 'px', 'dock width is the chosen column width');
  assert.equal(tray.style.width, '100%', 'tray fills the bar in the column');

  window.innerWidth = 800;
  fireResize();
  assert.ok(!dock.style.left, 'dock inline left removed on exit when no inline value existed before');
  assert.ok(!dock.style.right, 'dock inline right removed on exit when no inline value existed before');
  assert.ok(!dock.style.width, 'dock inline width removed on exit when no inline value existed before');
  assert.ok(!tray.style.width, 'tray inline width removed on exit when no inline value existed before');

  // A preset inline value is captured on entry and restored exactly, not emptied.
  dock.style.right = '14px';
  window.innerWidth = 1024;
  fireResize();
  assert.equal(dock.style.left, contentRight + gap + 'px', 'dock left edge aligns to the column again on re-entry');
  assert.equal(dock.style.right, 'auto', 'dock right is auto again in the column');
  assert.equal(dock.style.width, columnWidth + 'px', 'dock width is the chosen column width again on re-entry');
  assert.equal(tray.style.width, '100%', 'tray fills the bar again on re-entry');
  window.innerWidth = 800;
  fireResize();
  assert.ok(!dock.style.left, 'dock inline left removed again when no inline value existed before');
  assert.equal(dock.style.right, '14px', 'preset inline dock right restored exactly, not emptied');
  assert.ok(!dock.style.width, 'dock inline width removed again when no inline value existed before');
  assert.ok(!tray.style.width, 'tray inline width removed again when no inline value existed before');
});

test('gutter mode never relocates the action bar as window width changes', async () => {
  // The reserved-column and card-column migration is gone: the bar stays a
  // direct child of body at every width, its inline left tracks the column's
  // left edge while the column exists, and no width change ever writes the
  // artifact.
  const block = makeBlock('anchored section');
  block.getBoundingClientRect = () => ({left: 0, top: 200, right: 700, bottom: 250, width: 700, height: 50});
  const {window, body, documentElement, listeners} = loadArtifactCommentsScript(SHORTCUT_PATH, false, {
    bodyChildren: [block],
    innerWidth: 1400,
    fetch: async () => ({ok: true, status: 200, async json() { return {name: 'S'}; }}),
  });
  const fireResize = () => {
    for (const l of listeners) {
      if (l.target === 'window' && l.type === 'resize') l.handler();
    }
  };
  const dock = dockOf(body);
  const contentRight = block.getBoundingClientRect().right;
  const columnLeft = contentRight + window.__cbcGutterGap + 'px';
  const bodyStyleAttr = body.attributes.style;
  const assertBarAtHome = (width) => {
    assert.equal(dock.parentNode, body, 'the bar stays a direct child of body at ' + width + 'px');
    assert.equal(body.attributes.style, bodyStyleAttr, 'body style attribute identical at ' + width + 'px');
  };
  assert.ok(
    window.__cbcChooseWidth(documentElement.clientWidth, contentRight, window.__cbcGutterGap, 240, 300) >= 240,
    'the column engages at 1400px in this fake layout'
  );

  addBlockComment(body, listeners, block, 'cmt');
  await flushPromises();
  assertBarAtHome(1400);
  assert.equal(dock.style.left, columnLeft, 'bar inline left tracks the column left edge at 1400px');

  window.innerWidth = 1024;
  fireResize();
  assertBarAtHome(1024);
  assert.equal(dock.style.left, columnLeft, 'bar inline left still tracks the column left edge at 1024px');

  window.innerWidth = 800;
  fireResize();
  assertBarAtHome(800);
  assert.ok(!dock.style.left, 'bar inline left returns to its pre-entry state below the threshold');

  window.innerWidth = 1400;
  fireResize();
  assertBarAtHome(1400);
  assert.equal(dock.style.left, columnLeft, 'bar inline left tracks the column left edge again at 1400px');
});

// ---------------------------------------------------------------------------
// Re-anchor, hover, and click affordances (comment-gutter part 2b)
// ---------------------------------------------------------------------------

const REANCHOR_PATH = '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html';
const FETCH_OK = async () => ({ok: true, status: 200, json: async () => ({name: 'S'})});

// A 640px-wide article column at the default 1024px viewport keeps the one-band
// reserve clean (640 <= 1024 - 316) but not the two-band one (640 > 1024 - 624),
// so measureContent() puts the dock inside the card column in these tests.
function rectAt(top) {
  return () => ({left: 0, top, right: 640, bottom: top + 40, width: 640, height: 40});
}

test('re-anchor hit: restores el and marks the block when quote matches', () => {
  const block = makeBlock('Unique commentable text for reanchor hit');
  block.getBoundingClientRect = rectAt(200);
  const storage = seedDraft(REANCHOR_PATH, [
    {kind: 'block', quote: 'Unique commentable text for reanchor hit', context: '', comment: 'test comment'},
  ]);
  const {body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: [block],
    fetch: FETCH_OK,
  });
  assert.equal(block.classList.contains('__cbc-marked'), true, 'block carries __cbc-marked after re-anchor');
  const gutter = findChildByClass(body, '__cbc-gutter');
  assert.ok(gutter, 'gutter is active in gutter mode');
  assert.ok(gutter.children.length > 0, 'anchored card is placed in the gutter');
});

test('re-anchor miss: entry stays unanchored, no block is marked, nothing throws', () => {
  const block = makeBlock('some commentable text');
  const storage = seedDraft(REANCHOR_PATH, [
    {kind: 'block', quote: 'quote that matches no block', context: '', comment: 'no match'},
  ]);
  const {body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: [block],
    fetch: FETCH_OK,
  });
  assert.equal(block.classList.contains('__cbc-marked'), false, 'no block is marked on miss');
  const gutter = findChildByClass(body, '__cbc-gutter');
  assert.ok(!gutter, 'gutter is not created when no entry is anchored');
});

test('re-anchor never mis-anchors to a prefix-sharing block', () => {
  const blockLong = makeBlock('common prefix text with extra tail');
  blockLong.getBoundingClientRect = rectAt(200);
  const blockShort = makeBlock('common prefix text');
  blockShort.getBoundingClientRect = rectAt(400);
  const storage = seedDraft(REANCHOR_PATH, [
    {kind: 'block', quote: 'common prefix text', context: '', comment: 'should anchor to exact match'},
  ]);
  const {body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: [blockLong, blockShort],
    fetch: FETCH_OK,
  });
  assert.equal(blockShort.classList.contains('__cbc-marked'), true, 'exact-match block is marked');
  assert.equal(blockLong.classList.contains('__cbc-marked'), false, 'prefix-sharing block is NOT marked');
});

test('re-anchor is idempotent: running twice does not change anchors or marks', () => {
  const block = makeBlock('idempotent reanchor text');
  block.getBoundingClientRect = rectAt(200);
  const storage = seedDraft(REANCHOR_PATH, [
    {kind: 'block', quote: 'idempotent reanchor text', context: '', comment: 'test idempotence'},
  ]);
  const {window, body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: [block],
    fetch: FETCH_OK,
  });
  assert.equal(block.classList.contains('__cbc-marked'), true, 'block is marked after first re-anchor');
  const gutterBefore = findChildByClass(body, '__cbc-gutter');
  assert.ok(gutterBefore, 'gutter exists after first re-anchor');
  const cardsBefore = gutterBefore.children.length;
  window.__cbcReanchor();
  assert.equal(block.classList.contains('__cbc-marked'), true, 'block is still marked after second re-anchor');
  const gutterAfter = findChildByClass(body, '__cbc-gutter');
  assert.ok(gutterAfter, 'gutter still exists after second re-anchor');
  assert.equal(gutterAfter.children.length, cardsBefore, 'card count unchanged after second re-anchor');
});

test('gutter mode routes anchored entries to the gutter and unanchored entries to the corner list', () => {
  const blockA = makeBlock('anchored block A for routing');
  blockA.getBoundingClientRect = rectAt(200);
  const blockB = makeBlock('anchored block B for routing');
  blockB.getBoundingClientRect = rectAt(400);
  const storage = seedDraft(REANCHOR_PATH, [
    {kind: 'block', quote: 'anchored block A for routing', context: '', comment: 'matched A'},
    {kind: 'block', quote: 'nonexistent quote one matching nothing', context: '', comment: 'unmatched A'},
    {kind: 'block', quote: 'anchored block B for routing', context: '', comment: 'matched B'},
    {kind: 'block', quote: 'nonexistent quote two matching nothing', context: '', comment: 'unmatched B'},
  ]);
  const {body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: [blockA, blockB],
    fetch: FETCH_OK,
  });
  const gutter = findChildByClass(body, '__cbc-gutter');
  assert.ok(gutter, 'gutter is active in gutter mode');
  assert.equal(gutter.children.length, 2, 'gutter child count equals the anchored count');
  const trayList = body.querySelector('.__cbc-tray-list');
  assert.ok(trayList, 'tray list exists');
  assert.equal(trayList.children.length, 2, 'corner list card count equals the unanchored count');
});

test('gutter mode writes only stackCards tops to gutter children (single writer)', () => {
  // The rects give the fake layout a 640px-wide article column so that
  // measureContent() keeps the one-band reserve clean and gutter mode engages.
  const tops = [100, 110, 120];
  const blocks = tops.map((t) => {
    const b = makeBlock('single writer anchor ' + t);
    b.getBoundingClientRect = () => ({left: 0, top: t, right: 640, bottom: t + 50, width: 640, height: 50});
    return b;
  });
  const storage = seedDraft(REANCHOR_PATH, [
    ...tops.map((t) => ({kind: 'block', quote: 'single writer anchor ' + t, context: '', comment: 'anchored ' + t})),
    {kind: 'block', quote: 'single writer unanchored one', context: '', comment: 'unanchored one'},
    {kind: 'block', quote: 'single writer unanchored two', context: '', comment: 'unanchored two'},
  ]);
  const {window, body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: blocks,
    fetch: FETCH_OK,
  });
  const stackCards = window.__cbcStackCards;
  const gap = window.__cbcGutterGap;
  const gutter = findChildByClass(body, '__cbc-gutter');
  assert.ok(gutter, 'gutter is active');
  const cards = gutter.children.filter((c) => c.className === '__cbc-tray-item');
  assert.equal(cards.length, blocks.length, 'one gutter card per anchored entry');
  const trayList = body.querySelector('.__cbc-tray-list');
  assert.equal(trayList.children.length, 2, 'unanchored entries routed to the corner list');

  const anchors = blocks.map((b) => b.getBoundingClientRect().top + (window.scrollY || 0));
  const heights = cards.map((c) => c.offsetHeight);
  const expected = stackCards(anchors, heights, gap);
  const px = (s) => parseInt(String(s), 10);
  const actualTops = cards.map((c) => c.style.top);
  const expectedTops = expected.map((t) => t + 'px');
  assert.deepEqual(
    [...actualTops].sort((a, b) => px(a) - px(b)),
    [...expectedTops].sort((a, b) => px(a) - px(b)),
    'every gutter child top is a value produced by this render\'s stackCards output'
  );
});

test('hover affordance: hovering a card highlights its anchor block with a distinct class', () => {
  const block = makeBlock('hover affordance target block');
  block.getBoundingClientRect = rectAt(200);
  const storage = seedDraft(REANCHOR_PATH, [
    {kind: 'block', quote: 'hover affordance target block', context: '', comment: 'hover test'},
  ]);
  const {body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: [block],
    fetch: FETCH_OK,
  });
  const gutter = findChildByClass(body, '__cbc-gutter');
  assert.ok(gutter, 'gutter exists');
  const card = gutter.children[0];
  assert.ok(card, 'card exists in gutter');
  assert.equal(block.classList.contains('__cbc-marked'), true, 'block has persistent mark before hover');
  assert.equal(block.classList.contains('__cbc-card-hover'), false, 'block does not have hover class before hover');
  card._listeners.mouseenter[0]({});
  assert.equal(block.classList.contains('__cbc-card-hover'), true, 'block gains hover class on mouseenter');
  assert.equal(block.classList.contains('__cbc-marked'), true, 'persistent mark survives hover-in');
  card._listeners.mouseleave[0]({});
  assert.equal(block.classList.contains('__cbc-card-hover'), false, 'hover class removed on mouseleave');
  assert.equal(block.classList.contains('__cbc-marked'), true, 'persistent mark survives hover-out');
});

test('click affordance: clicking a card scrolls its anchor block into view', () => {
  const block = makeBlock('click affordance target block');
  block.getBoundingClientRect = rectAt(200);
  let scrollCalls = 0;
  block.scrollIntoView = () => { scrollCalls++; };
  const storage = seedDraft(REANCHOR_PATH, [
    {kind: 'block', quote: 'click affordance target block', context: '', comment: 'click test'},
  ]);
  const {body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: [block],
    fetch: FETCH_OK,
  });
  const gutter = findChildByClass(body, '__cbc-gutter');
  assert.ok(gutter, 'gutter exists');
  const card = gutter.children[0];
  assert.ok(card, 'card exists in gutter');
  assert.equal(scrollCalls, 0, 'no scroll before click');
  let stopPropCalled = false;
  let preventDefaultCalled = false;
  card._listeners.click[0]({
    target: card,
    stopPropagation() { stopPropCalled = true; },
    preventDefault() { preventDefaultCalled = true; },
  });
  assert.equal(scrollCalls, 1, 'scrollIntoView called once on card click');
  assert.equal(stopPropCalled, true, 'click stops propagation to prevent re-entering capture path');
  assert.equal(preventDefaultCalled, true, 'click prevents default');
});

test('click affordance: clicking an unanchored card is a no-op for scroll but still blocks capture', () => {
  const block = makeBlock('some block text');
  let scrollCalls = 0;
  block.scrollIntoView = () => { scrollCalls++; };
  const storage = seedDraft(REANCHOR_PATH, [
    {kind: 'block', quote: 'nonexistent quote', context: '', comment: 'unanchored'},
  ]);
  const {body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: [block],
    fetch: FETCH_OK,
  });
  const trayList = body.querySelector('.__cbc-tray-list');
  assert.ok(trayList, 'tray list exists in corner mode');
  const card = trayList.children[0];
  assert.ok(card, 'card exists in tray list');
  let stopPropCalled = false;
  let preventDefaultCalled = false;
  card._listeners.click[0]({
    target: card,
    stopPropagation() { stopPropCalled = true; },
    preventDefault() { preventDefaultCalled = true; },
  });
  assert.equal(scrollCalls, 0, 'no scroll for unanchored card');
  assert.equal(stopPropCalled, true, 'click still stops propagation');
  assert.equal(preventDefaultCalled, true, 'click still prevents default');
});

test('card click does not break the x button', () => {
  const block = makeBlock('block for remove test');
  block.getBoundingClientRect = rectAt(200);
  const storage = seedDraft(REANCHOR_PATH, [
    {kind: 'block', quote: 'block for remove test', context: '', comment: 'remove test'},
  ]);
  const {body} = loadArtifactCommentsScript(REANCHOR_PATH, false, {
    sessionStorage: storage,
    bodyChildren: [block],
    fetch: FETCH_OK,
  });
  const gutter = findChildByClass(body, '__cbc-gutter');
  assert.ok(gutter, 'gutter exists');
  const card = gutter.children[0];
  assert.ok(card, 'card exists');
  const removeBtn = card.querySelector('.__cbc-tray-remove');
  assert.ok(removeBtn, 'x button exists in card');
  assert.ok(removeBtn._listeners.click && removeBtn._listeners.click.length > 0, 'x button has click handler');
  clickElement(removeBtn);
  assert.equal(block.classList.contains('__cbc-marked'), false, 'block is unmarked after removing the only entry');
  assert.equal(gutter.children.length, 0, 'gutter is empty after removal');
});
