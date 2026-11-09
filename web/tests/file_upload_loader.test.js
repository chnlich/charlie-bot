'use strict';
// File upload loader (web/templates/js/file-upload.js): the document carries this loader in the module's
// place, so the page can paint before the upload code arrives. Until it lands, the composer's gate reads an
// empty attachment list and no upload in flight, and a picked file waits for the download. These tests hold
// the loader's defaults, its download, and the module's own definitions taking over the same names.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

const ROOT = path.join(__dirname, '..');
const readStatic = (name) => fs.readFileSync(path.join(ROOT, 'static/js', name), 'utf8');
const readLoader = () => fs.readFileSync(path.join(ROOT, 'templates/js/file-upload.js'), 'utf8');

function loadPage() {
  const calls = [];
  const input = {
    listeners: new Map(),
    addEventListener(name, listener) { this.listeners.set(name, listener); },
  };
  const errors = [];
  const context = {
    document: {
      readyState: 'complete',
      getElementById: (id) => (id === 'msg-input' ? input : null),
      createElement: () => ({classList: {add() {}, remove() {}}}),
      addEventListener() {},
    },
    loadPageScripts(key, paths) {
      calls.push({key, paths});
      return new Promise(() => {});
    },
    console: {error: (message) => errors.push(message), warn: () => {}},
    showToast() {},
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(readLoader(), context, {filename: 'templates/js/file-upload.js'});
  return {context, calls, input, errors};
}

test('the first attachment action starts the module download', () => {
  const {context, calls} = loadPage();
  assert.deepEqual(Array.from(calls), [], 'the document loads no upload code');
  context.handleFiles({files: []});
  assert.equal(calls.length, 1);
  assert.equal(calls[0].key, 'file-upload');
  assert.equal(calls[0].paths.length, 1);
  assert.equal(calls[0].paths[0], '/static/js/file-upload.js');
});

test('before the module lands the gate reads no attachment and no upload in flight', () => {
  const {context, input} = loadPage();
  assert.equal(context.blockIfUploadsInFlight(), false);
  assert.deepEqual(Array.from(context.getUploadedFilesForPayload()), []);
  assert.deepEqual(Array.from(context.toPayloadFiles([{filename: 'a.txt'}])), []);
  assert.equal(context.handleFiles({files: []}), undefined);
  assert.equal(context.renderFileChips(), undefined);
  assert.equal(typeof input.listeners.get('paste'), 'function', 'a screenshot pasted before the module lands is kept');
});

test('the module defines the same names when it lands', () => {
  const {context} = loadPage();
  vm.runInContext(readStatic('file-upload.js'), context, {filename: 'file-upload.js'});
  const file = {id: 1, filename: 'a.txt', path: '/tmp/a.txt', size: 2, status: 'uploaded'};
  assert.equal(
    JSON.stringify(Array.from(context.toPayloadFiles([file]))),
    JSON.stringify([{filename: 'a.txt', path: '/tmp/a.txt', size: 2}]),
    'the wire projection is the module\'s, not the loader\'s empty answer',
  );
  context.inputWorkStarted('busy');
  assert.equal(context.blockIfUploadsInFlight(), true, 'an input-work window now holds the send gate');
  context.inputWorkSettled('busy');
  assert.equal(context.blockIfUploadsInFlight(), false);
});
