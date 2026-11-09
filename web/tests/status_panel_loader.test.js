'use strict';
// Session status panel loader (templates/js/chat/status-panel.js): the document carries this loader in the
// module's place. A web session's open starts the download only when its status has a waiting item to count on
// the view toggle, and the Status view's selection is the other first need. The open is replayed when the module
// lands, with the web_status messages and the descendant refresh that arrived in the meantime. A session that
// shows no panel never asks for the module. A failed download is reported and the next need retries.
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const test = require('node:test');
const assert = require('node:assert/strict');

const ROOT = path.join(__dirname, '..');
const readLoader = () => fs.readFileSync(path.join(ROOT, 'templates/js/chat/status-panel.js'), 'utf8');
const flush = () => new Promise((resolve) => setImmediate(resolve));

function loadPage({statusViewShown = false} = {}) {
  const downloadCalls = [];
  const replayed = [];
  const errors = [];
  let resolveLoad = null;
  let rejectLoad = null;
  const panelClasses = new Set(statusViewShown ? [] : ['hidden']);
  const panel = {classList: {contains: (name) => panelClasses.has(name)}};
  const context = {
    document: {getElementById: (id) => (id === 'status-panel' ? panel : null)},
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
    ForYou: {isWebSession: (session) => session.profile === 'manager' && !session.thread_session},
    wire(globals, chatOnly) {
      Object.assign(this, globals, chatOnly || {});
      Object.assign(context, globals);
    },
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(readLoader(), context, {filename: 'templates/js/chat/status-panel.js'});
  // The module's own definitions take these names when it lands; each replay call is recorded in order.
  const land = () => {
    context.openSessionStatusPanel = (session, messages) => {
      replayed.push({call: 'open', sessionId: session.id, messages});
    };
    context.observeWebStatusMessages = (messages) => replayed.push({call: 'observe', messages});
    context.refreshSessionStatusPanel = () => replayed.push({call: 'refresh'});
    context.Chat.openSessionStatusPanel = context.openSessionStatusPanel;
  };
  return {
    context, downloadCalls, replayed, errors, land, panel,
    resolveLoad: () => resolveLoad(),
    rejectLoad: (error) => rejectLoad(error),
  };
}

const manager = {id: 'sess-1', profile: 'manager'};
const worker = {id: 'sess-2', profile: 'worker'};
const waitingStatus = {updated_at: '2026-01-01T00:00:00Z', waiting: [{tier: 'wait', title: 'Review'}]};
const noWaitingStatus = {updated_at: '2026-01-01T00:00:00Z', waiting: []};

test('a web session whose status has a waiting item starts the module download', () => {
  const rig = loadPage();
  assert.deepEqual(Array.from(rig.downloadCalls), [], 'the document loads no panel code');
  rig.context.openSessionStatusPanel({...manager, web_status: waitingStatus}, []);
  assert.equal(rig.downloadCalls.length, 1);
  assert.equal(rig.downloadCalls[0].key, 'chat-status-panel');
  assert.deepEqual(Array.from(rig.downloadCalls[0].paths), ['/static/js/chat/status-panel.js']);
});

test('a web session with nothing to count and a hidden panel asks for nothing', () => {
  const rig = loadPage();
  rig.context.openSessionStatusPanel({...manager, web_status: noWaitingStatus}, []);
  rig.context.observeWebStatusMessages([{role: 'web_status', web_status: noWaitingStatus}]);
  assert.deepEqual(Array.from(rig.downloadCalls), [], 'no badge and no open panel need no panel code');
});

test('a status whose only waiting items are the reader\'s own asks for nothing', () => {
  const rig = loadPage();
  const userActOnly = {updated_at: '2026-01-01T00:00:00Z', waiting: [{tier: 'user-act', title: 'Approve'}]};
  rig.context.openSessionStatusPanel({...manager, web_status: userActOnly}, []);
  rig.context.observeWebStatusMessages([{role: 'web_status', web_status: userActOnly}]);
  assert.deepEqual(Array.from(rig.downloadCalls), [], 'the badge counts no item of the reader\'s own act');
});

test('a live status that adds a waiting item starts the download', () => {
  const rig = loadPage();
  rig.context.openSessionStatusPanel({...manager, web_status: noWaitingStatus}, []);
  rig.context.observeWebStatusMessages([{role: 'web_status', web_status: waitingStatus}]);
  assert.equal(rig.downloadCalls.length, 1);
});

test('a committed message that is not a web_status is not kept', async () => {
  const rig = loadPage();
  rig.context.openSessionStatusPanel({...manager, web_status: waitingStatus}, []);
  rig.context.observeWebStatusMessages([{role: 'assistant', content: 'done'}]);
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.deepEqual(Array.from(rig.replayed).map((call) => call.call), ['open'],
    'the panel replays no chat history');
});

test('the remembered Status view starts the download on open', () => {
  const rig = loadPage({statusViewShown: true});
  rig.context.openSessionStatusPanel({...manager, web_status: noWaitingStatus}, []);
  assert.equal(rig.downloadCalls.length, 1);
});

test('selecting the Status view starts the download', () => {
  const rig = loadPage();
  rig.context.openSessionStatusPanel({...manager, web_status: noWaitingStatus}, []);
  assert.deepEqual(Array.from(rig.downloadCalls), []);
  rig.context.Chat.loadStatusPanel();
  assert.equal(rig.downloadCalls.length, 1);
});

test('a session that shows no panel never asks for the module', () => {
  const rig = loadPage();
  rig.context.openSessionStatusPanel({...worker, web_status: waitingStatus}, []);
  rig.context.observeWebStatusMessages([{role: 'web_status', web_status: waitingStatus}]);
  rig.context.refreshSessionStatusPanel();
  rig.context.Chat.loadStatusPanel();
  assert.deepEqual(Array.from(rig.downloadCalls), []);
});

test('the open and the facts that arrived after it are replayed when the module lands', async () => {
  const rig = loadPage();
  const messages = [{role: 'user', content: 'hello'}];
  const live = [{role: 'web_status', web_status: waitingStatus}];
  rig.context.openSessionStatusPanel({...manager, web_status: waitingStatus}, messages);
  rig.context.observeWebStatusMessages(live);
  rig.context.refreshSessionStatusPanel();
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.equal(rig.replayed.length, 3);
  assert.equal(rig.replayed[0].call, 'open');
  assert.equal(rig.replayed[0].sessionId, 'sess-1');
  assert.equal(rig.replayed[0].messages, messages);
  assert.equal(rig.replayed[1].call, 'observe');
  assert.equal(rig.replayed[1].messages.length, 1, 'the live message is kept until the module lands');
  assert.equal(rig.replayed[1].messages[0].role, 'web_status');
  assert.equal(rig.replayed[2].call, 'refresh');
});

test('a cleared panel replays nothing when the module lands', async () => {
  const rig = loadPage();
  rig.context.openSessionStatusPanel({...manager, web_status: waitingStatus}, []);
  rig.context.observeWebStatusMessages([{role: 'web_status', web_status: waitingStatus}]);
  rig.context.clearSessionStatusPanel();
  rig.land();
  rig.resolveLoad();
  await flush();
  assert.deepEqual(Array.from(rig.replayed), []);
});

test('a failed download is reported and the next need retries', async () => {
  const rig = loadPage();
  rig.context.openSessionStatusPanel({...manager, web_status: waitingStatus}, []);
  rig.rejectLoad(new Error('offline'));
  await flush();
  assert.match(rig.errors[0], /did not load/);
  rig.context.Chat.loadStatusPanel();
  assert.equal(rig.downloadCalls.length, 2, 'the next need retries the download');
});
