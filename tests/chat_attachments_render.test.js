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
    console: {error: () => {}},
    document: {
      addEventListener() {},
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
  };

  vm.createContext(context);
  vm.runInContext(CHAT_JS, context, {filename: 'chat.js'});
  return context;
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
