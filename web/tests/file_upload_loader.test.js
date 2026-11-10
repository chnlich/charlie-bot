'use strict';
// File upload loader (web/templates/js/file-upload.js): the document carries this loader in the module's
// place, so the page can paint before the upload code arrives. Until it lands, the work a trigger started
// waits here behind the send gate: a picked file or a pasted image holds the gate while it downloads, so a
// message cannot leave without its attachment, and an input-work window another feature opened holds it
// too. A paste that carries no image neither downloads the module nor loses the paste. These tests hold
// that gate, the download trigger, and the module's own definitions taking over the same names.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

const ROOT = path.join(__dirname, '..');
const readStatic = (name) => fs.readFileSync(path.join(ROOT, 'static/js', name), 'utf8');
const readLoader = () => fs.readFileSync(path.join(ROOT, 'templates/js/file-upload.js'), 'utf8');
const flush = () => new Promise((resolve) => setImmediate(resolve));

function loadPage() {
  const calls = [];
  const toasts = [];
  const errors = [];
  let resolveLoad = null;
  let rejectLoad = null;
  const attributes = new Set();
  const classes = new Set();
  const button = {
    setAttribute(name) { attributes.add(name); },
    removeAttribute(name) { attributes.delete(name); },
    classList: {
      add(...names) { names.forEach((name) => classes.add(name)); },
      remove(...names) { names.forEach((name) => classes.delete(name)); },
    },
  };
  const listeners = new Map();
  const input = {
    listeners,
    addEventListener(name, listener) { listeners.set(name, listener); },
  };
  const context = {
    document: {
      readyState: 'complete',
      title: '',
      getElementById(id) {
        if (id === 'msg-input') return input;
        if (id === 'send-btn') return button;
        return null;
      },
      createElement: () => ({classList: {add() {}, remove() {}}}),
      addEventListener() {},
    },
    SESSION_ID: 'sess-1',
    SESSION_NAME: 'worker',
    PAGE_HOSTNAME: 'host',
    FormData: class { append() {} },
    fetch: () => new Promise(() => {}),
    loadPageStyles(key, paths) {
      return Promise.resolve();
    },
    loadPageScripts(key, paths) {
      calls.push({key, paths});
      return new Promise((resolve, reject) => {
        resolveLoad = resolve;
        rejectLoad = reject;
      });
    },
    console: {error: (message) => errors.push(message), warn() {}},
    showToast: (message, warning) => toasts.push({message, warning}),
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(readLoader(), context, {filename: 'templates/js/file-upload.js'});
  // The module reads the session the page displays; the page has that module before the upload one.
  const land = () => {
    vm.runInContext(readStatic('chat/displayed-session.js'), context, {filename: 'displayed-session.js'});
    vm.runInContext(readStatic('file-upload.js'), context, {filename: 'file-upload.js'});
  };
  return {
    context, calls, toasts, errors, input, attributes, classes, land,
    resolveLoad: () => resolveLoad(),
    rejectLoad: (error) => rejectLoad(error),
  };
}

function pasteEvent(items) {
  const event = {
    prevented: false,
    preventDefault() { this.prevented = true; },
    clipboardData: {items},
  };
  return event;
}

test('the first attachment action starts the module download', async () => {
  const rig = loadPage();
  assert.deepEqual(Array.from(rig.calls), [], 'the document loads no upload code');
  rig.context.handleFiles({files: []});
  await flush();
  assert.equal(rig.calls.length, 1);
  assert.equal(rig.calls[0].key, 'file-upload');
  assert.equal(rig.calls[0].paths.length, 1);
  assert.equal(rig.calls[0].paths[0], '/static/js/file-upload.js');
});

test('before the module lands the gate reads no attachment and no upload in flight', () => {
  const rig = loadPage();
  assert.equal(rig.context.blockIfUploadsInFlight(), false);
  assert.deepEqual(Array.from(rig.context.getUploadedFilesForPayload()), []);
  assert.deepEqual(Array.from(rig.context.toPayloadFiles([{filename: 'a.txt'}])), []);
  assert.equal(rig.context.handleFiles({files: []}), undefined);
  assert.equal(rig.context.renderFileChips(), undefined);
  assert.deepEqual(Array.from(rig.toasts), []);
  assert.equal(typeof rig.input.listeners.get('paste'), 'function', 'a screenshot pasted before the module lands is kept');
});

test('a picked file holds the send gate until the module lands and its upload starts', async () => {
  const rig = loadPage();
  rig.context.handleFiles({files: [{name: 'a.txt'}]});
  assert.equal(rig.context.blockIfUploadsInFlight(), true, 'the message waits for the file it carries');
  assert.match(rig.toasts[0].message, /attachment upload/);
  assert.equal(rig.attributes.has('disabled'), true, 'the send button shows the wait');

  await flush();
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.equal(rig.context.blockIfUploadsInFlight(), true, 'the module\'s own upload window holds the gate now');
  assert.equal(rig.attributes.has('disabled'), true);
});

test('an input-work window holds the gate from before the module lands, and a settle before it cancels the start', async () => {
  const rig = loadPage();
  rig.context.inputWorkStarted('Transcribing the recording');
  assert.equal(rig.context.blockIfUploadsInFlight(), true);
  assert.equal(rig.toasts[0].message, 'Transcribing the recording');
  rig.context.inputWorkSettled('Transcribing the recording');
  assert.equal(rig.context.blockIfUploadsInFlight(), false);

  await flush();
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.equal(rig.context.blockIfUploadsInFlight(), false, 'the settled window forwards no open start');
});

test('an input-work window still open when the module lands moves into the module', async () => {
  const rig = loadPage();
  rig.context.inputWorkStarted('Transcribing the recording');
  await flush();
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.equal(rig.context.blockIfUploadsInFlight(), true, 'the module holds the window the loader opened');
  rig.context.inputWorkSettled('Transcribing the recording');
  assert.equal(rig.context.blockIfUploadsInFlight(), false);
});

test('a plain-text paste neither downloads the module nor cancels the paste', () => {
  const rig = loadPage();
  const event = pasteEvent([{kind: 'string', type: 'text/plain'}]);
  rig.input.listeners.get('paste')(event);
  assert.equal(event.prevented, false, 'the text paste keeps its default action');
  assert.deepEqual(Array.from(rig.calls), [], 'text alone is no attachment action');
});

test('an image paste cancels the paste and starts the download while it travels', async () => {
  const rig = loadPage();
  const event = pasteEvent([{kind: 'file', type: 'image/png', getAsFile: () => ({})}]);
  rig.input.listeners.get('paste')(event);
  assert.equal(event.prevented, true, 'the cancel runs while the event dispatches');
  await flush();
  assert.equal(rig.calls.length, 1);
  assert.equal(rig.context.blockIfUploadsInFlight(), true, 'the screenshot waits behind the gate');
});

test('a failed download releases the held work and the next trigger retries', async () => {
  const rig = loadPage();
  rig.context.handleFiles({files: [{name: 'a.txt'}]});
  assert.equal(rig.context.blockIfUploadsInFlight(), true);
  await flush();
  rig.rejectLoad(new Error('offline'));
  await flush();
  assert.equal(rig.context.blockIfUploadsInFlight(), false, 'a module that will not arrive must not lock the composer');
  assert.match(rig.errors[0], /did not load/);
  rig.context.handleFiles({files: []});
  await flush();
  assert.equal(rig.calls.length, 2, 'the next action retries the download');
});

test('the module defines the same names when it lands', () => {
  const rig = loadPage();
  rig.land();
  const file = {id: 1, filename: 'a.txt', path: '/tmp/a.txt', size: 2, status: 'uploaded'};
  assert.equal(
    JSON.stringify(Array.from(rig.context.toPayloadFiles([file]))),
    JSON.stringify([{filename: 'a.txt', path: '/tmp/a.txt', size: 2}]),
    'the wire projection is the module\'s, not the loader\'s empty answer',
  );
  rig.context.inputWorkStarted('busy');
  assert.equal(rig.context.blockIfUploadsInFlight(), true, 'an input-work window now holds the send gate');
  rig.context.inputWorkSettled('busy');
  assert.equal(rig.context.blockIfUploadsInFlight(), false);
});
