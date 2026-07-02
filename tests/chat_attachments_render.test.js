const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const CHAT_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'chat.js'),
  'utf8'
);
const FILE_UPLOAD_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'file-upload.js'),
  'utf8'
);
const SLASH_COMMANDS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'slash-commands.js'),
  'utf8'
);

function escapeHtml(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

class FakeClassList {
  constructor() {
    this._classes = new Set();
  }

  add(...classes) {
    for (const className of classes) this._classes.add(className);
  }

  remove(...classes) {
    for (const className of classes) this._classes.delete(className);
  }

  contains(className) {
    return this._classes.has(className);
  }
}

class FakeElement {
  constructor() {
    this.innerHTML = '';
    this.classList = new FakeClassList();
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
    Node: {ELEMENT_NODE: 1},
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
        let text = '';
        return {
          set textContent(value) {
            text = String(value);
          },
          get innerHTML() {
            return escapeHtml(text);
          },
        };
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
  vm.runInContext(CHAT_JS, context, {filename: 'chat.js'});
  return context;
}

function makeAnchor(href) {
  return {
    dataset: {},
    isConnected: true,
    href,
    getAttribute(name) {
      return name === 'href' ? this.href : null;
    },
    setAttribute(name, value) {
      if (name === 'href') this.href = value;
    },
    closest(selector) {
      return selector === '.prose-msg' ? this.prose : null;
    },
  };
}

function makeProseRoot(anchors) {
  const parent = {
    inserted: [],
    insertBefore(child) {
      this.inserted.push(child);
      child.parentNode = this;
      return child;
    },
  };
  const prose = {
    id: '',
    parentNode: parent,
    nextSibling: null,
    nextElementSibling: null,
    closest() {
      return null;
    },
    querySelectorAll(selector) {
      if (selector === 'a[href]') return anchors;
      if (selector === 'code') return [];
      return [];
    },
  };
  for (const anchor of anchors) anchor.prose = prose;
  return {
    parent,
    root: {
      querySelectorAll(selector) {
        return selector === '.prose-msg' ? [prose] : [];
      },
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

function loadSlashCommandsScript(fetchImpl, overrides = {}) {
  const messages = [];
  const toasts = [];
  const clearedIds = [];
  const input = {
    value: '/help',
    style: {height: '42px'},
  };
  const localStorage = {
    removed: [],
    removeItem(key) {
      this.removed.push(key);
    },
  };
  const context = {
    SESSION_ID: 'session-a',
    DRAFT_KEY: 'draft-session-a',
    uploadsInFlight: 0,
    pendingUserMsg: false,
    console: {error: () => {}},
    fetch: fetchImpl,
    localStorage,
    showToast: (message, isError) => {
      toasts.push({message, isError: !!isError});
    },
    getUploadedFilesForPayload: () => [],
    clearSentUploadedFiles: (ids) => {
      clearedIds.push(ids);
    },
    appendMessage: (role, content, isVoice, timestamp, uploadedFiles) => {
      messages.push({role, content, isVoice: !!isVoice, timestamp, uploadedFiles});
    },
    bumpCurrentSessionToTop: () => {},
    startThinking: () => {},
    escapeHtml,
    document: {
      getElementById(id) {
        if (id === 'msg-input') return input;
        return null;
      },
      createElement() {
        let text = '';
        return {
          set textContent(value) {
            text = String(value);
          },
          get innerHTML() {
            return escapeHtml(text);
          },
        };
      },
    },
    ...overrides,
  };

  vm.createContext(context);
  vm.runInContext(SLASH_COMMANDS_JS, context, {filename: 'slash-commands.js'});
  return {context, messages, toasts, clearedIds, input, localStorage};
}

test('normalizeUserMessage strips legacy attachment footers and keeps file names for rendering', () => {
  const context = loadChatScript();

  const normalized = context.normalizeUserMessage(
    'Check this file\n\n[Attached files]\n- /tmp/report.pdf',
    null
  );

  assert.equal(normalized.content, 'Check this file');
  assert.equal(JSON.stringify(normalized.uploadedFiles), JSON.stringify([
    {filename: 'report.pdf', path: '/tmp/report.pdf'},
  ]));

  const html = context.renderUserMessageBubble('', false, null, normalized.uploadedFiles);
  assert.match(html, /message-attachment/);
  assert.match(html, /report\.pdf/);
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

test('executeSlashCommand marks a pending user message before the request resolves', async () => {
  let resolveFetch;
  const fetchPromise = new Promise((resolve) => {
    resolveFetch = resolve;
  });
  const {context, messages, clearedIds} = loadSlashCommandsScript(() => fetchPromise);

  const uploadedFiles = [{id: 7, filename: 'report.pdf', path: '/tmp/report.pdf', size: 12}];
  const commandPromise = context.executeSlashCommand('help', '', {displayText: '/help', uploadedFiles});

  assert.equal(context.pendingUserMsg, true);

  resolveFetch({
    async json() {
      return {type: 'help', commands: []};
    },
  });
  await commandPromise;

  assert.equal(messages[0].role, 'user');
  assert.equal(JSON.stringify(messages[0].uploadedFiles), JSON.stringify([
    {filename: 'report.pdf', path: '/tmp/report.pdf', size: 12},
  ]));
  assert.deepEqual(clearedIds, [[7]]);
});

test('executeSlashCommand clears pendingUserMsg when the server returns an error', async () => {
  const {context, messages, toasts} = loadSlashCommandsScript(async () => ({
    async json() {
      return {error: 'bad command'};
    },
  }));

  await context.executeSlashCommand('bad', '');

  assert.equal(context.pendingUserMsg, false);
  assert.equal(messages.length, 0);
  assert.deepEqual(toasts, [{message: 'bad command', isError: true}]);
});

test('executeSlashCommand blocks submission while uploads are still in flight', async () => {
  let fetchCalls = 0;
  const {context, toasts} = loadSlashCommandsScript(async () => {
    fetchCalls += 1;
    return {async json() { return {type: 'help', commands: []}; }};
  }, {uploadsInFlight: 1});

  await context.executeSlashCommand('help', '');

  assert.equal(fetchCalls, 0);
  assert.equal(context.pendingUserMsg, false);
  assert.deepEqual(toasts, [{message: 'Please wait for uploads to finish', isError: true}]);
});

test('resolveHtmlArtifactLink accepts raw URL strings and anchor elements', () => {
  const context = loadChatScript();

  const pathHref = '/files/%2Ftmp%2Freport/artifacts/plot.html';
  const fullHref = 'https://example.com/files/%2Ftmp%2Freport/artifacts/plot.html';

  const pathResult = context.resolveHtmlArtifactLink(pathHref);
  assert.equal(pathResult.absPath, '//tmp/report/artifacts/plot.html');
  assert.equal(pathResult.fetchUrl, '/files/%2Ftmp%2Freport/artifacts/plot.html');

  const fullResult = context.resolveHtmlArtifactLink(fullHref);
  assert.equal(fullResult.absPath, '//tmp/report/artifacts/plot.html');
  assert.equal(fullResult.fetchUrl, '/files/%2Ftmp%2Freport/artifacts/plot.html');

  const anchor = {
    getAttribute(name) {
      return name === 'href' ? pathHref : null;
    },
  };
  const anchorResult = context.resolveHtmlArtifactLink(anchor);
  assert.equal(anchorResult.absPath, '//tmp/report/artifacts/plot.html');

  assert.equal(context.resolveHtmlArtifactLink('/files/report/artifacts/plot.txt'), null);
  assert.equal(context.resolveHtmlArtifactLink('/other/path/artifacts/plot.html'), null);
  assert.equal(context.resolveHtmlArtifactLink('not a url'), null);
});

test('embedLinkedHtmlArtifacts stamps artifact prose links and rendered card open URLs with session fragment', async () => {
  const context = loadChatScript();
  context.SESSION_ID = 'view-session';
  const artifactAnchor = makeAnchor('/files/%2Ftmp%2Freport/artifacts/plot.html#old');
  const plainAnchor = makeAnchor('/files/%2Ftmp%2Freport/readme.txt#keep');
  const {root, parent} = makeProseRoot([artifactAnchor, plainAnchor]);

  context.Chat.embedLinkedHtmlArtifacts(root);
  assert.equal(
    artifactAnchor.getAttribute('href'),
    '/files/%2Ftmp%2Freport/artifacts/plot.html#cbsession=view-session'
  );
  assert.equal(plainAnchor.getAttribute('href'), '/files/%2Ftmp%2Freport/readme.txt#keep');

  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(parent.inserted.length, 1);
  assert.match(
    parent.inserted[0].renderedHtml,
    /href="\/files\/\/tmp\/report\/artifacts\/plot\.html#cbsession=view-session"/
  );
});

test('findArtifactLinkInCode extracts artifact URLs from inline code text', () => {
  const context = loadChatScript();
  function code(text) {
    return {textContent: text};
  }

  const pathResult = context.findArtifactLinkInCode(code('/files/%2Ftmp%2Freport/artifacts/plot.html'));
  assert.equal(pathResult.absPath, '//tmp/report/artifacts/plot.html');

  const fullResult = context.findArtifactLinkInCode(code('See https://example.com/files/%2Ftmp%2Freport/artifacts/plot.html here'));
  assert.equal(fullResult.absPath, '//tmp/report/artifacts/plot.html');

  assert.equal(context.findArtifactLinkInCode(code('just some code')), null);
  assert.equal(context.findArtifactLinkInCode(code('https://example.com/files/report/artifacts/plot.txt')), null);
});
