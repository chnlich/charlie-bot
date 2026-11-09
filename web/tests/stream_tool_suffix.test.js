'use strict';
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {loadPageTimersOnce} = require('./websocket_context_stub');
const {readStatic} = require('./read_static');
const {baseSessionContext, createChatSidebarContext, installSessionDocumentLookups, stubPageTimers}
  = require('./session_context_stub');
const {createElement} = require('./dom_element_stub');

// A page whose draft accumulator and WebSocket handlers are loaded: the frame
// path a live page runs, with the paint and append hooks recording instead.
function buildPage() {
  const errors = [];
  const toasts = [];
  const appended = [];
  const painted = [];
  const context = {
    SESSION_ID: 'session-a',
    eventCursor: 0,
    STATIC_ASSET_VERSION: 'stub-asset-version',
    document: {getElementById: () => null, dispatchEvent: () => true, addEventListener() {}},
    CustomEvent,
    TextEncoder,
    location: {protocol: 'http:', host: 'localhost'},
    localStorage: {getItem: () => null},
    console: {log() {}, error(message) { errors.push(String(message)); }},
    showToast(message, isError) { toasts.push({message, isError}); },
    hideStreaming() {},
    showStreaming(draft) { painted.push(draft); },
    observeWebStatusMessages() {},
    observeTaskRecordMessages() {},
    appendMessageObject(msg) { appended.push(msg); },
    appendMessage() {},
    refreshSessionStatusPanel() {},
    startThinking() {},
    stopThinking() {},
    pollActiveSessionView() {},
    refreshSessionStatusNow() {},
    applyRunningChanged() {},
    setSessionPendingTriggerIndicator() {},
    updateSidebarSessionName() {},
    handleSidebarSearch() {},
    switchSidebarFilter() {},
    currentFilter: 'all',
  };
  context.globalThis = context;
  vm.createContext(context);
  loadPageTimersOnce(context);
  vm.runInContext(readStatic('websocket.js'), context, {filename: 'websocket.js'});
  vm.runInContext(readStatic('chat/namespace.js'), context, {filename: 'chat/namespace.js'});
  context.Chat.activeTurnEngineFor = () => null;
  vm.runInContext(readStatic('stream-draft.js'), context, {filename: 'stream-draft.js'});
  vm.runInContext(readStatic('websocket-handlers.js'), context, {filename: 'websocket-handlers.js'});
  return {context, errors, toasts, appended, painted};
}

function bashRow(command, output) {
  const row = {name: 'Bash', input: {command}, output: output || '', is_error: false};
  return JSON.parse(JSON.stringify(row));
}

// The whole-list wire: every frame carries the draft's full row list, the form
// the page consumed before the suffix frames.
function wholeFrames(snapshots) {
  return snapshots.map((rows) => ({type: 'stream', tools: rows}));
}

// The suffix wire: every frame carries the rows from the first one that differs
// from the previous snapshot, with that row's index.
function suffixFrames(snapshots) {
  const frames = [];
  let previous = [];
  for (const rows of snapshots) {
    let from = 0;
    while (from < previous.length && from < rows.length
        && JSON.stringify(previous[from]) === JSON.stringify(rows[from])) {
      from++;
    }
    frames.push({type: 'stream', tools: rows.slice(from), tools_from: from});
    previous = rows;
  }
  return frames;
}

function deliver(context, frames) {
  for (const frame of frames) {
    context.handleWSEvent(frame, 'session-a', 0);
  }
}

function heldRows(context) {
  // The accumulator builds its arrays inside the vm realm, whose prototypes a
  // strict deepEqual compares; the JSON round trip reads the row values only.
  return JSON.parse(JSON.stringify(context.takeStreamedDraft().tools ?? null));
}

// Each row-history renders the same rows through the suffix wire as through the
// whole-list wire.
function assertSameRows(snapshots) {
  const whole = buildPage();
  deliver(whole.context, wholeFrames(snapshots));
  const suffix = buildPage();
  deliver(suffix.context, suffixFrames(snapshots));
  assert.deepEqual(heldRows(suffix.context), heldRows(whole.context));
  assert.deepEqual(heldRows(suffix.context), snapshots[snapshots.length - 1]);
  return {whole, suffix};
}

test('suffix frames render the same rows as whole lists as rows are added', () => {
  assertSameRows([[bashRow('one')], [bashRow('one'), bashRow('two')], [bashRow('one'), bashRow('two'), bashRow('three')]]);
});

test('a result on the last row renders the same rows through both wires', () => {
  const withResult = bashRow('one', 'file1\nfile2');
  assertSameRows([[bashRow('one')], [withResult]]);
});

test('a run end that stops a row renders the same rows through both wires', () => {
  const agentRunning = {name: 'Agent', input: {description: 'Check'}, output: '', is_error: false,
    id: 'toolu_1', subagent: {status: 'running', tools: []}};
  const agentStopped = {...agentRunning, subagent: {status: 'stopped', tools: []}};
  const {suffix} = assertSameRows([[agentRunning], [agentStopped]]);
  // The suffix of the stop starts at the stopped row.
  assert.deepEqual(heldRows(suffix.context), [agentStopped]);
});

test('a page entering a session whose draft holds only tool rows renders the same rows through both wires', () => {
  const bootstrapRows = [bashRow('one'), bashRow('two')];
  const afterResult = [bashRow('one'), bashRow('two', 'read'), bashRow('three')];
  const whole = buildPage();
  whole.context.seedStreamDraft({content: '', thinking: '', tools: bootstrapRows});
  deliver(whole.context, wholeFrames([afterResult]));
  const suffix = buildPage();
  suffix.context.seedStreamDraft({content: '', thinking: '', tools: bootstrapRows});
  deliver(suffix.context, suffixFrames([afterResult]));
  assert.deepEqual(heldRows(suffix.context), heldRows(whole.context));
  assert.deepEqual(heldRows(suffix.context), afterResult);
});

test('a reconnect mid-draft renders the same rows through both wires', () => {
  const first = [bashRow('one')];
  const second = [bashRow('one', 'done')];
  const third = [bashRow('one', 'done'), bashRow('two')];
  const whole = buildPage();
  deliver(whole.context, wholeFrames([first, second, third]));
  // The reconnect resends the whole draft as a reset frame, then the live frames resume.
  const suffix = buildPage();
  deliver(suffix.context, [...suffixFrames([first, second]), {type: 'stream', reset: true, tools: third}]);
  assert.deepEqual(heldRows(suffix.context), heldRows(whole.context));
});

test('a transcript seed older than the socket frames cannot roll the rows back', () => {
  const first = [bashRow('one')];
  const second = [bashRow('one', 'done')];
  const suffix = buildPage();
  deliver(suffix.context, suffixFrames([first, second]));
  // The stale seed still holds the result-less row.
  suffix.context.seedStreamDraft({content: 'draft text', thinking: '', tools: first});
  assert.deepEqual(heldRows(suffix.context), second, 'the socket-owned rows survived the older seed');
  // The same page fed whole lists renders the same rows when its seed is current.
  const whole = buildPage();
  deliver(whole.context, wholeFrames([first, second]));
  whole.context.seedStreamDraft({content: 'draft text', thinking: '', tools: second});
  assert.deepEqual(heldRows(whole.context), second);
  assert.deepEqual(heldRows(suffix.context), heldRows(whole.context));
});

test('a seed still fills the rows of a draft the socket has never touched', () => {
  const rows = [bashRow('one')];
  const page = buildPage();
  page.context.seedStreamDraft({content: 'draft text', thinking: '', tools: rows});
  assert.deepEqual(heldRows(page.context), rows);
});

test('a commit carries the rows the page streamed, through both wires', () => {
  const snapshots = [[bashRow('one')], [bashRow('one', 'done')]];
  const pages = [buildPage(), buildPage()];
  deliver(pages[0].context, wholeFrames(snapshots));
  deliver(pages[1].context, suffixFrames(snapshots));
  for (const page of pages) {
    page.context.handleWSEvent(
        {type: 'message', message: {role: 'assistant', id: 'm1', content_streamed: true}}, 'session-a', 0);
  }
  const committed = pages.map((page) => JSON.parse(JSON.stringify(page.appended[0])));
  assert.equal(committed[0].id, 'm1');
  assert.deepEqual(committed[1].tools, committed[0].tools);
  assert.deepEqual(committed[1].tools, snapshots[1]);
  // The commit closed the draft, so the next suffix starts from an empty list again.
  assert.equal(heldRows(pages[1].context), null);
});

test('a tools_from past the held rows is a protocol error, reported and not applied', () => {
  const page = buildPage();
  deliver(page.context, suffixFrames([[bashRow('one')]]));
  page.context.handleWSEvent({type: 'stream', tools: [bashRow('two')], tools_from: 5}, 'session-a', 0);
  assert.deepEqual(heldRows(page.context), [bashRow('one')], 'the bad frame changed no rows');
  assert.equal(page.errors.length, 1);
  assert.match(page.errors[0], /tools_from 5/);
  assert.equal(page.toasts.length, 1);
  assert.equal(page.toasts[0].isError, true);
});

test('a manager bootstrap whose draft holds only tool rows seeds the rows and paints nothing', () => {
  const rows = [bashRow('one'), bashRow('two')];
  const elements = new Map([['messages', createElement({id: 'messages'})]]);
  const painted = [];
  const hidden = [];
  const {context} = baseSessionContext({elements});
  context.showStreaming = (draft) => painted.push(draft);
  context.hideStreaming = () => hidden.push(true);
  installSessionDocumentLookups(context, elements, elements.get('messages'), []);
  stubPageTimers(context);
  createChatSidebarContext(context);
  vm.runInContext(readStatic('stream-draft.js'), context, {filename: 'stream-draft.js'});

  context.renderSessionView({
    session: {id: 'session-a', backend: 'claude-opus-4.6', round_ratings: {}},
    messages: [],
    pending_draft: {content: '', thinking: '', tools: rows},
    event_count: 6,
    oldest_message_ordinal: 4,
    active_backend: 'claude-opus-4.6',
    active_backend_type: '',
    has_more: false,
  });
  assert.deepEqual(context.takeStreamedDraft().tools, rows, 'the bootstrap rows are the held base');
  assert.deepEqual(painted, [], 'a manager draft with no text paints no preview');
  assert.deepEqual(hidden, [], 'and no hide undid the seed');
});
