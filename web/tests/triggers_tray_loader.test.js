'use strict';
// Pending triggers tray loader (templates/js/chat/triggers-tray.js): the document carries this loader in the
// module's place. The page opens the tray on every session, but only a nonzero pending count in the status
// poll starts the download, so a session with no pending trigger never asks for the module. The open session,
// the poll's handoff and the bell's expand request are replayed when the module lands. A failed download is
// reported and the next count or click retries.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

const ROOT = path.join(__dirname, '..');
const readLoader = () => fs.readFileSync(path.join(ROOT, 'templates/js/chat/triggers-tray.js'), 'utf8');
const flush = () => new Promise((resolve) => setImmediate(resolve));

function loadPage() {
  const downloadCalls = [];
  const styleCalls = [];
  const replayed = [];
  const errors = [];
  let resolveLoad = null;
  let rejectLoad = null;
  const context = {
    SESSION_ID: 'sess-1',
    loadPageStyles(key, paths) {
      styleCalls.push({key, paths});
      return Promise.resolve();
    },
    loadPageScripts(key, paths) {
      downloadCalls.push({key, paths});
      return new Promise((resolve, reject) => {
        resolveLoad = resolve;
        rejectLoad = reject;
      });
    },
    console: {error: (message) => errors.push(message)},
  };
  context.Chat = {
    wire(globals) {
      Object.assign(this, globals);
      Object.assign(context, globals);
    },
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(readLoader(), context, {filename: 'templates/js/chat/triggers-tray.js'});
  // The module's own definitions take these names when it lands; each replay call is recorded in order.
  const land = () => {
    context.renderPendingTriggersTray = (sessionId) => replayed.push({call: 'render', sessionId});
    context.setSessionPendingTriggerTrayStatus = (sid, status) => replayed.push({call: 'status', sid, status});
    context.expandPendingTriggersTray = (sid) => {
      replayed.push({call: 'expand', sid});
      return sid === context.SESSION_ID;
    };
    context.Chat.renderPendingTriggersTray = context.renderPendingTriggersTray;
    context.Chat.setSessionPendingTriggerTrayStatus = context.setSessionPendingTriggerTrayStatus;
    context.Chat.expandPendingTriggersTray = context.expandPendingTriggersTray;
  };
  return {
    context, downloadCalls, styleCalls, replayed, errors, land,
    resolveLoad: () => resolveLoad(),
    rejectLoad: (error) => rejectLoad(error),
  };
}

const status = (count) => ({pending_trigger_count: count, next_trigger_at: null});

test('an open and a zero pending count ask for nothing', () => {
  const rig = loadPage();
  rig.context.renderPendingTriggersTray('sess-1');
  rig.context.setSessionPendingTriggerTrayStatus('sess-1', status(0));
  assert.deepEqual(Array.from(rig.downloadCalls), [], 'a session with no pending trigger loads no tray code');
});

test('another session\'s pending count asks for nothing until it opens', () => {
  const rig = loadPage();
  rig.context.renderPendingTriggersTray('sess-1');
  rig.context.setSessionPendingTriggerTrayStatus('sess-2', status(2));
  assert.deepEqual(Array.from(rig.downloadCalls), []);
});

test('the first nonzero count for the open session starts one download', async () => {
  const rig = loadPage();
  rig.context.renderPendingTriggersTray('sess-1');
  rig.context.setSessionPendingTriggerTrayStatus('sess-1', status(2));
  rig.context.setSessionPendingTriggerTrayStatus('sess-1', status(2));
  await flush();
  assert.equal(rig.styleCalls.length, 1, 'the tray stylesheet comes with the module');
  assert.equal(rig.styleCalls[0].key, 'chat-triggers-tray');
  assert.deepEqual(Array.from(rig.styleCalls[0].paths), ['/static/css/triggers-tray.css']);
  assert.equal(rig.downloadCalls.length, 1);
  assert.equal(rig.downloadCalls[0].key, 'chat-triggers-tray');
  assert.deepEqual(Array.from(rig.downloadCalls[0].paths), ['/static/js/chat/triggers-tray.js']);
});

test('the open session is rendered when the module lands', async () => {
  const rig = loadPage();
  rig.context.renderPendingTriggersTray('sess-1');
  rig.context.setSessionPendingTriggerTrayStatus('sess-1', status(2));
  await flush();
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.deepEqual(Array.from(rig.replayed), [{call: 'render', sessionId: 'sess-1'}]);
});

test('a bell for the open session expands in place after the render', async () => {
  const rig = loadPage();
  rig.context.renderPendingTriggersTray('sess-1');
  assert.equal(rig.context.expandPendingTriggersTray('sess-1'), true, 'the open session answers in place');
  await flush();
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.deepEqual(Array.from(rig.replayed), [
    {call: 'render', sessionId: 'sess-1'},
    {call: 'expand', sid: 'sess-1'},
  ]);
});

test('a bell for another session switches there and expands on that open', async () => {
  const rig = loadPage();
  rig.context.renderPendingTriggersTray('sess-1');
  assert.equal(rig.context.expandPendingTriggersTray('sess-2'), false, 'the caller switches session first');
  rig.context.renderPendingTriggersTray('sess-2');
  await flush();
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.deepEqual(Array.from(rig.replayed), [
    {call: 'render', sessionId: 'sess-2'},
    {call: 'expand', sid: 'sess-2'},
  ]);
});

test('a failed download is reported and the next count retries', async () => {
  const rig = loadPage();
  rig.context.renderPendingTriggersTray('sess-1');
  rig.context.setSessionPendingTriggerTrayStatus('sess-1', status(1));
  await flush();
  rig.rejectLoad(new Error('offline'));
  await flush();
  assert.match(rig.errors[0], /did not load/);
  rig.context.setSessionPendingTriggerTrayStatus('sess-1', status(2));
  await flush();
  assert.equal(rig.downloadCalls.length, 2, 'the next nonzero count retries the download');
});
