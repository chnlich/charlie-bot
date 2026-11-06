const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const { createClassList } = require('./dom_element_stub');

const { escapeHtml } = require('./escape_html_stub');

const FILE_UPLOAD_JS = readStatic('file-upload.js');
const DISPLAYED_SESSION_JS = readStatic('chat/displayed-session.js');

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

function loadFileUploadScript(fetchImpl) {
  const fileChips = new FakeElement();
  const sendButton = new FakeElement();
  const context = {
    SESSION_ID: 'session-a',
    // index.html's host fact; the displayed-session module restates it in the tab title.
    PAGE_HOSTNAME: 'localhost:8000',
    SESSION_NAME: 'Session session-a',
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
  // The displayed-session module rides the page ahead of file-upload.js; its
  // bind answers the upload's composer target.
  vm.runInContext(DISPLAYED_SESSION_JS, context, {filename: 'chat/displayed-session.js'});
  vm.runInContext(FILE_UPLOAD_JS, context, {filename: 'file-upload.js'});
  return {context, fileChips, sendButton};
}

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

test('a file status outside the known three throws instead of rendering a success mark', () => {
  const {context} = loadFileUploadScript(async () => ({ok: true, async json() { return {}; }}));
  const unknown = {status: 'quarantined', id: 1};

  assert.throws(() => context.renderFileChipIcon(unknown), /Unknown file status/);
  assert.throws(() => context.renderFileChipRemoveButton(unknown), /Unknown file status/);
  assert.throws(() => context.renderFileChipStatusLabel(unknown), /Unknown file status/);
});




