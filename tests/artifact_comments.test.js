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
    style: {},
    children: [],
    childNodes: [],
    nodeType: 1,
    tagName: 'DIV',
    parentElement: null,
    display: 'block',
    get textContent() {
      return this.childNodes.map((node) => node.textContent || '').join('');
    },
    get innerText() {
      return this.textContent;
    },
    classList: {add() {}, remove() {}},
    appendChild(child) {
      this.children.push(child);
    },
    addEventListener() {},
    setAttribute() {},
    focus() {},
    querySelector() {
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

function loadArtifactCommentsScript(pathname, framed = false) {
  const listeners = [];
  const window = {
    location: {pathname},
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
  window.parent = framed ? {} : window;

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
    console,
    Node: {DOCUMENT_POSITION_FOLLOWING: 4},
    fetch() {
      throw new Error('fetch should not run while loading artifact-comments.js');
    },
  };
  vm.createContext(context);
  vm.runInContext(ARTIFACT_COMMENTS_JS, context, {filename: 'artifact-comments.js'});
  return {window, head, body, listeners};
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

test('artifact comments script stays inert inside frames', () => {
  const {window, head, listeners} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html',
    true
  );

  assert.equal(window.__cbcExtractSessionIdFromPath, undefined);
  assert.equal(window.__cbcBuildBatchMessage, undefined);
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

test('resolveEntryDraft returns draftText lines when override exists', () => {
  const {window} = loadArtifactCommentsScript(
    '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html'
  );

  const resolveEntryDraft = window.__cbcResolveEntryDraft;
  const entry = {
    quote: 'quoted text',
    context: 'Context',
    comment: 'comment text',
    draftText: 'Custom draft\nsecond line',
  };
  const lines = resolveEntryDraft(entry);
  assert.equal(lines.length, 2);
  assert.equal(lines[0], 'Custom draft');
  assert.equal(lines[1], 'second line');
});

test('buildBatchMessage uses draftText override instead of default formatting', () => {
  const artifactPath = '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(artifactPath);
  const buildBatchMessage = window.__cbcBuildBatchMessage;

  const entries = [
    {quote: 'First', context: 'Ctx', comment: 'default comment'},
    {quote: 'Second', context: 'Ctx2', comment: 'also default', draftText: 'Overridden text'},
  ];

  const message = buildBatchMessage(entries);

  assert.ok(message.includes('1. ▸ Ctx › "First"'), 'first entry uses default format');
  assert.ok(message.includes('↳ default comment'), 'first entry comment rendered');
  assert.ok(message.includes('2. Overridden text'), 'second entry uses draftText');
  assert.ok(!message.includes('↳ also default'), 'second default comment is not rendered');
});

test('buildBatchMessage indents multiline draftText correctly', () => {
  const artifactPath = '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(artifactPath);
  const buildBatchMessage = window.__cbcBuildBatchMessage;

  const entries = [{quote: 'Q', context: 'C', comment: 'c', draftText: 'Line one\nLine two'}];

  const message = buildBatchMessage(entries);

  assert.equal(
    message,
    [
      '[Artifact comments · ' + artifactPath + '] (1)',
      '',
      '1. Line one',
      '   Line two',
    ].join('\n')
  );
});

test('buildBatchMessage handles mixed default and edited entries', () => {
  const artifactPath = '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(artifactPath);
  const buildBatchMessage = window.__cbcBuildBatchMessage;

  const entries = [
    {quote: 'Q1', context: 'C1', comment: 'first', draftText: 'Edited first'},
    {quote: 'Q2', context: 'C2', comment: 'second'},
  ];

  const message = buildBatchMessage(entries);

  assert.equal(
    message,
    [
      '[Artifact comments · ' + artifactPath + '] (2)',
      '',
      '1. Edited first',
      '',
      '2. ▸ C2 › "Q2"',
      '   ↳ second',
    ].join('\n')
  );
});

test('buildBatchMessage uses draftText for improve shortcut entries', () => {
  const artifactPath = '/files/data/home/chaoli/.charliebot/sessions/session-270/artifacts/plan.html';
  const {window} = loadArtifactCommentsScript(artifactPath);
  const buildBatchMessage = window.__cbcBuildBatchMessage;

  const entries = [
    {
      kind: 'improve',
      quote: '',
      context: 'Improve',
      comment: 'Think from scratch, how to improve this?',
      draftText: 'Custom improve prompt',
    },
  ];

  const message = buildBatchMessage(entries);

  assert.equal(
    message,
    [
      '[Artifact comments · ' + artifactPath + '] (1)',
      '',
      '1. Custom improve prompt',
    ].join('\n')
  );
});
