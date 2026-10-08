const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic, chatModules, runStaticModules} = require('./read_static');
const { createClassList, createEscapingElement } = require('./dom_element_stub');

const { escapeHtml } = require('./escape_html_stub');

const { makeAnchor, makeProseRoot } = require('./chat_prose_stub');

const FILE_UPLOAD_JS = readStatic('file-upload.js');

class FakeElement {
  constructor() {
    this.innerHTML = '';
    this.classList = createClassList();
    this.attributes = new Map();
    this.textContent = '';
  }

  setAttribute(name, value = '') {
    this.attributes.set(name, value);
  }

  removeAttribute(name) {
    this.attributes.delete(name);
  }

  hasAttribute(name) {
    return this.attributes.has(name);
  }
}

function loadChatScript() {
  const context = {
    SESSION_ID: 'test-session',
    console: {error: () => {}},
    fetch: async () => ({
      ok: true,
      async text() {
        return '<main>Artifact</main>';
      },
    }),
    hljs: {highlight: (value) => ({value: escapeHtml(value)})},
    localStorage: {getItem: () => null, setItem: () => {}},
    marked: {parse: (txt) => txt},
    fixNestedFences: (txt) => txt,
    window: {
      addEventListener() {},
      location: {href: 'https://example.com/sessions/test-session'},
    },
    URL: globalThis.URL,
    Node: {ELEMENT_NODE: 1, TEXT_NODE: 3},
    document: {
      addEventListener() {},
      createElement(tagName) {
        if (tagName === 'template') {
          return {
            content: {firstElementChild: null},
            set innerHTML(value) {
              this.content.firstElementChild = {
                renderedHtml: String(value),
                nodeType: 1,
                dataset: {},
                classList: {contains: (className) => className === 'html-artifact'},
              };
            },
          };
        }
        return createEscapingElement(tagName);
      },
      getElementById() {
        return null;
      },
      querySelector() {
        return null;
      },
    },
  };

  vm.createContext(context);
  runStaticModules(context, chatModules());
  return context;
}

function makeText(value) {
  return {nodeType: 3, nodeValue: value};
}

function makeCodeEl(value) {
  return {
    nodeType: 1,
    tagName: 'CODE',
    childNodes: [makeText(value)],
    textContent: value,
    dataset: {},
    isConnected: true,
    closest(selector) {
      return selector === '.prose-msg' ? this.prose : null;
    },
  };
}

function loadFileUploadScript(fetchImpl) {
  const fileChips = new FakeElement();
  const sendButton = new FakeElement();
  const context = {
    SESSION_ID: 'session-a',
    console: {error: () => {}},
    FormData: class {
      constructor() {
        this.entries = [];
      }

      append(name, value) {
        this.entries.push([name, value]);
      }
    },
    fetch: fetchImpl,
    showToast: () => {},
    escapeHtml,
    document: {
      getElementById(id) {
        if (id === 'file-chips') return fileChips;
        if (id === 'send-btn') return sendButton;
        return null;
      },
    },
  };

  vm.createContext(context);
  vm.runInContext(FILE_UPLOAD_JS, context, {filename: 'file-upload.js'});
  return {context, fileChips, sendButton};
}

test('normalizeUserMessage strips legacy attachment footers and keeps file names for rendering', () => {
  const context = loadChatScript();

  const normalized = context.Chat.normalizeUserMessage(
    'Check this file\n\n[Attached files]\n- /tmp/report.pdf',
    null
  );

  assert.equal(normalized.content, 'Check this file');
  assert.equal(JSON.stringify(normalized.uploadedFiles), JSON.stringify([
    {filename: 'report.pdf', path: '/tmp/report.pdf'},
  ]));

  const html = context.renderUserMessageBubble('', null, null, normalized.uploadedFiles);
  assert.match(html, /message-attachment/);
  assert.match(html, /report\.pdf/);
});

test('renderUserMessageBubble shows the voice marker for a dictated message only', () => {
  const context = loadChatScript();

  assert.match(context.renderUserMessageBubble('hello', 'voice', null, null), /Voice/);
  assert.doesNotMatch(context.renderUserMessageBubble('hello', null, null, null), /Voice/);
});

test('uploadFile marks failed uploads visibly and excludes them from payload', async () => {
  const {context, fileChips, sendButton} = loadFileUploadScript(async () => ({
    ok: false,
    async json() {
      return {detail: 'disk full'};
    },
  }));

  await context.uploadFile({name: 'broken.txt', size: 4});

  assert.match(fileChips.innerHTML, /file-chip--failed/);
  assert.match(fileChips.innerHTML, /Failed/);
  assert.equal(context.getUploadedFilesForPayload().length, 0);
  assert.equal(sendButton.hasAttribute('disabled'), false);
});

test('uploadFile marks successful uploads as sendable', async () => {
  const {context, fileChips, sendButton} = loadFileUploadScript(async () => ({
    ok: true,
    async json() {
      return {
        filename: 'ready.txt',
        path: '/tmp/ready.txt',
        size: 21,
      };
    },
  }));

  await context.uploadFile({name: 'ready.txt', size: 21});

  assert.match(fileChips.innerHTML, /file-chip--uploaded/);
  assert.equal(context.getUploadedFilesForPayload().length, 1);
  assert.equal(context.getUploadedFilesForPayload()[0].path, '/tmp/ready.txt');
  assert.equal(sendButton.hasAttribute('disabled'), false);
});




test('resolveHtmlArtifactLink accepts raw URL strings and anchor elements', () => {
  const context = loadChatScript();

  const pathHref = '/absolute_filepath/%2Ftmp%2Freport/artifacts/plot.html';
  const fullHref = 'https://example.com/absolute_filepath/%2Ftmp%2Freport/artifacts/plot.html';

  const pathResult = context.Chat.resolveHtmlArtifactLink(pathHref);
  assert.equal(pathResult.absPath, '//tmp/report/artifacts/plot.html');
  assert.equal(pathResult.fetchUrl, '/absolute_filepath/%2Ftmp%2Freport/artifacts/plot.html');

  const fullResult = context.Chat.resolveHtmlArtifactLink(fullHref);
  assert.equal(fullResult.absPath, '//tmp/report/artifacts/plot.html');
  assert.equal(fullResult.fetchUrl, '/absolute_filepath/%2Ftmp%2Freport/artifacts/plot.html');

  const anchor = {
    getAttribute(name) {
      return name === 'href' ? pathHref : null;
    },
  };
  const anchorResult = context.Chat.resolveHtmlArtifactLink(anchor);
  assert.equal(anchorResult.absPath, '//tmp/report/artifacts/plot.html');

  assert.equal(context.Chat.resolveHtmlArtifactLink('/absolute_filepath/report/artifacts/plot.txt'), null);
  assert.equal(context.Chat.resolveHtmlArtifactLink('/other/path/artifacts/plot.html'), null);
  assert.equal(context.Chat.resolveHtmlArtifactLink('not a url'), null);
});

test('embedLinkedHtmlArtifacts stamps artifact prose links and rendered card open URLs with session fragment', async () => {
  const context = loadChatScript();
  context.SESSION_ID = 'view-session';
  const artifactAnchor = makeAnchor('/absolute_filepath/%2Ftmp%2Freport/artifacts/plot.html#old');
  const plainAnchor = makeAnchor('/absolute_filepath/%2Ftmp%2Freport/readme.txt#keep');
  const {root, parent} = makeProseRoot({anchors: [artifactAnchor, plainAnchor]});

  context.Chat.embedLinkedHtmlArtifacts(root);
  assert.equal(
    artifactAnchor.getAttribute('href'),
    '/absolute_filepath/%2Ftmp%2Freport/artifacts/plot.html#cbsession=view-session'
  );
  assert.equal(plainAnchor.getAttribute('href'), '/absolute_filepath/%2Ftmp%2Freport/readme.txt#keep');

  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(parent.inserted.length, 1);
  assert.match(
    parent.inserted[0].renderedHtml,
    /href="\/absolute_filepath\/\/tmp\/report\/artifacts\/plot\.html#cbsession=view-session"/
  );
});

test('findArtifactLinkInCode extracts artifact URLs from inline code text', () => {
  const context = loadChatScript();
  function code(text) {
    return {textContent: text};
  }

  const pathResult = context.Chat.findArtifactLinkInCode(code('/absolute_filepath/%2Ftmp%2Freport/artifacts/plot.html'));
  assert.equal(pathResult.absPath, '//tmp/report/artifacts/plot.html');

  const fullResult = context.Chat.findArtifactLinkInCode(code('See https://example.com/absolute_filepath/%2Ftmp%2Freport/artifacts/plot.html here'));
  assert.equal(fullResult.absPath, '//tmp/report/artifacts/plot.html');

  assert.equal(context.Chat.findArtifactLinkInCode(code('just some code')), null);
  assert.equal(context.Chat.findArtifactLinkInCode(code('https://example.com/absolute_filepath/report/artifacts/plot.txt')), null);
});

test('embedLinkedHtmlArtifacts embeds a bare artifact path in plain prose text', async () => {
  const context = loadChatScript();
  const barePath = '/absolute_filepath/home/x/.charliebot/sessions/abc/artifacts/report.html';
  const {root, parent} = makeProseRoot({childNodes: [makeText(barePath)]});

  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(parent.inserted.length, 1);
  assert.match(parent.inserted[0].renderedHtml, /html-artifact/);
  assert.match(
    parent.inserted[0].renderedHtml,
    /artifacts\/report\.html#cbsession=test-session/
  );
});

test('embedLinkedHtmlArtifacts embeds an artifact path inside a <code> span', async () => {
  const context = loadChatScript();
  const path = '/absolute_filepath/home/x/.charliebot/sessions/abc/artifacts/report.html';
  const codeSpan = makeCodeEl(path);
  const {root, parent} = makeProseRoot({childNodes: [codeSpan], codes: [codeSpan]});

  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(parent.inserted.length, 1);
});

test('embedLinkedHtmlArtifacts does not double-embed a path in both plain text and a <code> span', async () => {
  const context = loadChatScript();
  const path = '/absolute_filepath/home/x/.charliebot/sessions/abc/artifacts/report.html';
  const codeSpan = makeCodeEl(path);
  const {root, parent} = makeProseRoot({
    childNodes: [makeText(path), codeSpan],
    codes: [codeSpan],
  });

  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(parent.inserted.length, 1);
});

test('embedLinkedHtmlArtifacts ignores plain text that does not match the artifact pattern', async () => {
  const context = loadChatScript();
  const {root, parent} = makeProseRoot({
    childNodes: [makeText(
        'See /absolute_filepath/home/x/readme.txt and arbitrary /absolute_filepath/some/random/path')],
  });

  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(parent.inserted.length, 0);
});

test('embedLinkedHtmlArtifacts does not double-embed a path in both plain text and an <a> link', async () => {
  const context = loadChatScript();
  const path = '/absolute_filepath/home/x/.charliebot/sessions/abc/artifacts/report.html';
  const anchor = makeAnchor(path);
  const {root, parent} = makeProseRoot({anchors: [anchor], childNodes: [makeText(path)]});

  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(parent.inserted.length, 1);
});

test('embedLinkedHtmlArtifacts does not embed bare paths in the streaming message', async () => {
  const context = loadChatScript();
  const barePath = '/absolute_filepath/home/x/.charliebot/sessions/abc/artifacts/report.html';
  const {root, parent} = makeProseRoot({id: 'streaming-msg', childNodes: [makeText(barePath)]});

  context.Chat.embedLinkedHtmlArtifacts(root);
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(parent.inserted.length, 0);
});
