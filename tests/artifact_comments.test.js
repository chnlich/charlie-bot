const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ARTIFACT_COMMENTS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'artifact-comments.js'),
  'utf8'
);

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
    innerWidth: 1024,
    innerHeight: 768,
    addEventListener(type, handler, options) {
      listeners.push({target: 'window', type, handler, options});
    },
    setTimeout() {},
    getComputedStyle(el) {
      return {display: el.display || 'block'};
    },
  };
  window.self = window;
  window.parent = framed ? (opts.parent || {}) : window;

  const head = makeElement();
  const body = makeElement();
  const document = {
    head,
    body,
    createElement() {
      return makeElement();
    },
    addEventListener(type, handler, options) {
      listeners.push({target: 'document', type, handler, options});
    },
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
    console: opts.console || console,
    Node: {DOCUMENT_POSITION_FOLLOWING: 4},
    fetch: opts.fetch || function() {
      throw new Error('fetch should not run while loading artifact-comments.js');
    },
  };
  vm.createContext(context);
  vm.runInContext(ARTIFACT_COMMENTS_JS, context, {filename: 'artifact-comments.js'});
  return {window, head, body, listeners};
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

function rectsIntersect(a, b) {
  return a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top;
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

  const shortcuts = findChildByClass(body, '__cbc-shortcuts');
  const shortcutBtn = findChildByClass(shortcuts, '__cbc-shortcut');
  clickElement(shortcutBtn);
  await flushPromises();

  const tray = findChildByClass(body, '__cbc-tray');
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

  const shortcuts = findChildByClass(body, '__cbc-shortcuts');
  const shortcutBtn = findChildByClass(shortcuts, '__cbc-shortcut');
  clickElement(shortcutBtn);
  await flushPromises(5);

  const tray = findChildByClass(body, '__cbc-tray');
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
  const shortcuts = body.children.find((child) => child.className === '__cbc-shortcuts');
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
  assert.match(cssRule(styleText, '.__cbc-tray-item-comment'), /-webkit-line-clamp:2/);
  assert.match(cssRule(styleText, '.__cbc-tray-item-comment'), /overflow-wrap:anywhere/);
  assert.doesNotMatch(cssRule(styleText, '.__cbc-tray-edit-btn'), /position:absolute/);
  assert.doesNotMatch(cssRule(styleText, '.__cbc-tray-remove'), /position:absolute/);
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
  const shortcuts = findChildByClass(body, '__cbc-shortcuts');
  const shortcutBtn = findChildByClass(shortcuts, '__cbc-shortcut');
  clickElement(shortcutBtn);
  const tray = findChildByClass(body, '__cbc-tray');
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

function loadWithShortcuts() {
  return loadArtifactCommentsScript(SHORTCUT_PATH, false, {
    console: {warn() {}, error() {}},
    fetch: async () => ({ok: true, status: 200, async json() { return {name: 'S'}; }}),
  });
}

function shortcutButtons(body) {
  const shortcuts = findChildByClass(body, '__cbc-shortcuts');
  return shortcuts.children.filter((child) => child.className === '__cbc-shortcut');
}

function trayItemCount(body) {
  const tray = findChildByClass(body, '__cbc-tray');
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
  const {body} = loadWithShortcuts();
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
