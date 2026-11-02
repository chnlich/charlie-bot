// ---------------------------------------------------------------------------
// Send-button lock: #send-btn's disabled state has one writer — the in-flight
// lock (file-upload.js refreshSendLock) driven by attachment uploads plus the
// input-work window voice opens for each transcription. Thinking state never
// writes the button, so a
// master run leaves it sendable; each optimistic send's server echo is skipped
// by request id. Driven through the real chat,
// sidebar, websocket, file-upload and voice-input modules on the
// session_context_stub harness, in the style of chat_attachments_render.test.js
// (send button as an attribute-map fake) and voice_input_run.test.js (fake XHR).
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic, readPackageStatic} = require('./read_static');
const {createElement, createClassList} = require('./dom_element_stub');
const {
  baseSessionContext,
  installSessionDocumentLookups,
  stubPageTimers,
  createChatSidebarContext,
} = require('./session_context_stub');

const FILE_UPLOAD_JS = readStatic('file-upload.js');
const WEBSOCKET_JS = readStatic('websocket.js');
const WEBSOCKET_HANDLERS_JS = readStatic('websocket-handlers.js');
const VOICE_INPUT_JS = readPackageStatic('features', 'voice', 'voice-input.js');

const tick = () => new Promise((resolve) => setImmediate(resolve));
const flush = async () => { await tick(); await tick(); };

// The send button as chat_attachments_render.test.js fakes it: an attribute map
// plus a real classList, so the lock's disabled write and greyed classes are
// observable exactly as the production code makes them.
class SendButton {
  constructor() {
    this.classList = createClassList();
    this.attributes = new Map();
  }

  setAttribute(name, value = '') { this.attributes.set(name, String(value)); }
  removeAttribute(name) { this.attributes.delete(name); }
  hasAttribute(name) { return this.attributes.has(name); }
}

class FakeVoiceXhr {
  constructor() {
    this.status = 0;
    this.response = null;
    this.upload = {};
    this.onload = this.onerror = this.onabort = null;
  }

  open() {}
  setRequestHeader() {}
  send() {}
  abort() { if (this.onabort) this.onabort(); }

  // Test-side stand-ins for the browser's async events.
  respond(status, response) { this.status = status; this.response = response; if (this.onload) this.onload(); }
  failNetwork() { if (this.onerror) this.onerror(); }
}

// The run object startRecording builds, from the module's own builder: the lock
// tests enter the transcription window directly through startUpload with the
// slot claimed below.
function makeVoiceRun(h, sessionId) {
  return h.context.newVoiceRun(sessionId, h.context.VOICE_BACKENDS[0]);
}

const sendLocked = (h) => h.sendButton.hasAttribute('disabled');
const sendGreyed = (h) => h.sendButton.classList.contains('opacity-50')
  && h.sendButton.classList.contains('cursor-not-allowed')
  && !h.sendButton.classList.contains('hover:bg-blue-500');
const sendPlain = (h) => !h.sendButton.classList.contains('opacity-50')
  && !h.sendButton.classList.contains('cursor-not-allowed')
  && h.sendButton.classList.contains('hover:bg-blue-500');

function buildLockHarness() {
  const elements = new Map();
  for (const id of ['messages', 'msg-input', 'thinking', 'thinking-time', 'file-chips', 'usage-text']) {
    elements.set(id, createElement({id}));
  }
  const sendButton = new SendButton();
  elements.set('send-btn', sendButton);

  const {context} = baseSessionContext({elements});
  context.console = {error() {}, warn() {}, log() {}};
  // Call-time refs the page loads from config.js, plus the event cursor the
  // inline bootstrap owns: none of them ride the modules.
  context.saveDraft = () => {};
  context.eventCursor = 0;
  context.window = Object.assign(context.window, {removeEventListener() {}});
  context.XMLHttpRequest = FakeVoiceXhr;
  let requestSequence = 0;
  context.crypto = {randomUUID: () => `request-${++requestSequence}`};
  context.FormData = class {
    append() {}
    get() { return null; }
  };
  context.Blob = require('node:buffer').Blob;
  context.accessTokenAuthorization = () => null;
  context.VOICE_BACKENDS = [{id: 'batch', label: 'Batch backend', livePartials: false, unavailableReason: null}];
  context.VOICE_DEFAULT_BACKEND = 'batch';

  const toasts = [];
  context.showToast = (message, isError) => toasts.push({message, isError: !!isError});

  const h = {elements, sendButton, toasts, fetchCalls: [], fetchImpl: null, context};
  installSessionDocumentLookups(context, elements, elements.get('messages'), []);
  context.fetch = async (url, opts) => {
    h.fetchCalls.push({url, opts});
    if (h.fetchImpl) return h.fetchImpl(url, opts);
    return {ok: true, json: async () => ({})};
  };
  stubPageTimers(context);
  createChatSidebarContext(context);
  // Page-order tail: the gate/lock file lands before voice-input.js, whose
  // transcription windows report into it; every cross-file read here happens
  // at call time, so the load order only mirrors the page's dependency.
  vm.runInContext(FILE_UPLOAD_JS, context, {filename: 'file-upload.js'});
  vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});
  vm.runInContext(WEBSOCKET_HANDLERS_JS, context, {filename: 'websocket-handlers.js'});
  vm.runInContext(VOICE_INPUT_JS, context, {filename: 'voice-input.js'});
  // The voice slot and the echo count are the modules' top-level lets: their
  // lexical bindings shadow any context property, so tests claim and read them
  // from inside the context.
  context.__claimVoiceRun = vm.runInContext('(run) => { activeVoiceRun = run; }', context);
  context.__pendingRequestIds = vm.runInContext('() => [...pendingUserRequestIds]', context);
  return h;
}

// A held-open /api upload response, the way an in-flight attachment looks.
function holdUploadOpen(h) {
  let resolveUpload;
  h.fetchImpl = (url) => {
    if (String(url).includes('/upload')) {
      return new Promise((resolve) => { resolveUpload = resolve; });
    }
    return {ok: true, json: async () => ({})};
  };
  return (data) => resolveUpload({ok: true, json: async () => data});
}

// ---------------------------------------------------------------------------
// 1. Initiation paths never lock the button.
// ---------------------------------------------------------------------------
test('sending a message starts a thinking round but never locks the send button', async () => {
  const h = buildLockHarness();
  let resolveSend;
  h.fetchImpl = () => new Promise((resolve) => { resolveSend = resolve; });
  const input = h.elements.get('msg-input');
  input.value = 'hello there';

  h.context.sendMessage();
  await flush();

  assert.equal(h.context.masterThinking, true, 'the thinking indicator is up');
  assert.equal(sendLocked(h), false, 'a master round does not lock the button');
  assert.equal(sendGreyed(h), false);
  assert.equal(h.context.__pendingRequestIds().length, 1);

  resolveSend({ok: true, status: 202, json: async () => ({})});
  await flush();
  assert.equal(sendLocked(h), false);
});


// ---------------------------------------------------------------------------
// 2. Each in-flight kind locks; the combined count unlocks only at zero.
// ---------------------------------------------------------------------------
test('attachment and voice windows each lock, and the combined count unlocks only at zero', async () => {
  const h = buildLockHarness();
  const completeUpload = holdUploadOpen(h);

  const uploadPromise = h.context.uploadFile({name: 'a.txt', size: 3});
  await flush();
  assert.equal(sendLocked(h), true, 'an in-flight attachment locks the button');
  assert.equal(sendGreyed(h), true);

  const run = makeVoiceRun(h, 'session-a');
  h.context.__claimVoiceRun(run);
  h.context.startUpload(run);
  await flush(); // startUpload awaits the recording's settled devices first
  assert.equal(sendLocked(h), true, 'a voice transcription window keeps it locked');

  completeUpload({path: '/tmp/a.txt', filename: 'a.txt', size: 3});
  await uploadPromise;
  assert.equal(sendLocked(h), true, 'the settled upload leaves the voice window holding the lock');
  assert.equal(sendGreyed(h), true);

  run.xhr.respond(200, {text: 'hello world'});
  await flush();
  assert.equal(sendLocked(h), false, 'the decode landed: the count is zero and the button unlocks');
  assert.equal(sendPlain(h), true);
  assert.equal(h.elements.get('msg-input').value, 'hello world');
});

test('a failed voice upload settles its window; a retry re-locks; cancel settles again', async () => {
  const h = buildLockHarness();
  const run = makeVoiceRun(h, 'session-a');
  h.context.__claimVoiceRun(run);

  h.context.startUpload(run);
  await flush(); // startUpload awaits the recording's settled devices first
  assert.equal(sendLocked(h), true);

  run.xhr.failNetwork();
  await flush();
  assert.equal(sendLocked(h), false, 'the failed window settled');
  assert.equal(run.phase, 'retry', 'the buffer waits in the retry state');

  h.context.startUpload(run);
  await flush(); // the retry re-enters through the same devices await
  assert.equal(sendLocked(h), true, 'the retry re-enters the in-flight window');

  h.context.cancelVoiceUpload(run);
  await flush();
  assert.equal(sendLocked(h), false, 'the canceled window settled');
});

test('a session switch tears down an in-flight transcription window and settles its count', async () => {
  const h = buildLockHarness();
  const run = makeVoiceRun(h, 'session-a');
  h.context.__claimVoiceRun(run);

  h.context.startUpload(run);
  await flush(); // startUpload awaits the recording's settled devices first
  assert.equal(sendLocked(h), true);

  h.context.resetVoiceState();
  await flush();
  assert.equal(sendLocked(h), false, 'the teardown settled the window');
  assert.equal(run.phase, 'uploading', 'the aborted continuation died on the ownership guard, not in failVoiceUpload');
});

// ---------------------------------------------------------------------------
// 3. master_done inside an in-flight window does not unlock the button.
// ---------------------------------------------------------------------------
test('master_done inside an in-flight window does not unlock the button', async () => {
  const h = buildLockHarness();
  const completeUpload = holdUploadOpen(h);

  h.context.startThinking({keepSendEnabled: true});
  const uploadPromise = h.context.uploadFile({name: 'a.txt', size: 3});
  await flush();
  assert.equal(sendLocked(h), true, 'the upload holds the lock while the round runs');

  h.context.handleWSEvent({type: 'master_done'}, 'session-a', 0);
  assert.equal(h.context.masterThinking, false, 'the round-end cleared the thinking state');
  assert.equal(sendLocked(h), true, 'stopThinking no longer writes the button');

  completeUpload({path: '/tmp/a.txt', filename: 'a.txt', size: 3});
  await uploadPromise;
  assert.equal(sendLocked(h), false, 'the count returning to zero unlocks');
  assert.equal(sendPlain(h), true);
});

// ---------------------------------------------------------------------------
// 4. Consecutive sends: each echo is skipped exactly once.
// ---------------------------------------------------------------------------
test('two sends fired back to back skip their matching echoes and draw every other echo', async () => {
  const h = buildLockHarness();
  const resolvers = [];
  h.fetchImpl = () => new Promise((resolve) => resolvers.push(resolve));
  const input = h.elements.get('msg-input');
  const messages = h.elements.get('messages');

  input.value = 'first message';
  h.context.sendMessage();
  input.value = 'second message';
  h.context.sendMessage();
  await flush();
  const requestIds = h.fetchCalls.map((call) => JSON.parse(call.opts.body).request_id);
  assert.equal(new Set(requestIds).size, 2, 'each send has a separate request id');
  assert.deepEqual([...h.context.__pendingRequestIds()], requestIds, 'both request ids await their echoes');
  assert.equal(messages.children.length, 2, 'two local bubbles, one per send');
  assert.equal(messages.children.filter((el) => el.innerHTML.includes('data-message-role="user"')
    && el.innerHTML.includes('first message')).length, 1);
  assert.equal(messages.children.filter((el) => el.innerHTML.includes('data-message-role="user"')
    && el.innerHTML.includes('second message')).length, 1);

  h.context.handleWSEvent({
    type: 'message',
    message: {role: 'user', content: 'first message', request_id: requestIds[0], id: 'srv-1'},
  }, 'session-a', 0);
  h.context.handleWSEvent({
    type: 'message',
    message: {role: 'user', content: 'second message', request_id: requestIds[1], id: 'srv-2'},
  }, 'session-a', 0);
  assert.deepEqual([...h.context.__pendingRequestIds()], [], 'each matching echo removes its own request id');
  assert.equal(messages.children.length, 2, 'neither echo painted a second bubble');

  h.context.handleWSEvent({
    type: 'message',
    message: {role: 'user', content: 'from another tab', request_id: 'other-request', id: 'srv-3'},
  }, 'session-a', 0);
  h.context.handleWSEvent({
    type: 'message', message: {role: 'user', content: 'legacy echo', id: 'srv-4'},
  }, 'session-a', 0);
  assert.equal(messages.children.length, 4, 'unmatched and missing request ids render');
  assert.match(messages.children[2].innerHTML, /from another tab/);
  assert.match(messages.children[3].innerHTML, /legacy echo/);

  resolvers.forEach((resolve) => resolve({ok: true, status: 202, json: async () => ({})}));
  await flush();
});

function captureSendMessages(h) {
  const messages = [];
  const statusUpdates = [];
  h.context.appendMessage = (role, content, _inputMode, _timestamp, _files, requestId) => {
    const message = {role, content, request_id: requestId, delivery_status: requestId ? 'Sending…' : null};
    messages.push(message);
    return message;
  };
  h.context.setMessageDeliveryStatus = (message, status) => {
    message.delivery_status = status;
    statusUpdates.push(status);
  };
  return {messages, statusUpdates};
}

test('a network error retries with the same request id and keeps one optimistic bubble', async () => {
  const h = buildLockHarness();
  const {messages, statusUpdates} = captureSendMessages(h);
  const retryDelays = [];
  let releaseRetry;
  h.context.setTimeout = (callback, delay) => {
    retryDelays.push(delay);
    releaseRetry = callback;
    return 1;
  };
  let attempt = 0;
  h.fetchImpl = async () => {
    attempt += 1;
    if (attempt === 1) throw new TypeError('offline');
    return {ok: true, status: 202, json: async () => ({})};
  };
  h.elements.get('msg-input').value = 'network retry';

  const sendPromise = h.context.sendMessage();
  await flush();
  assert.equal(messages[0].delivery_status, 'Sending…', 'the bubble stays marked while the retry waits');
  assert.equal(h.fetchCalls.length, 1);
  releaseRetry();
  await sendPromise;

  const requestIds = h.fetchCalls.map((call) => JSON.parse(call.opts.body).request_id);
  assert.equal(requestIds.length, 2);
  assert.equal(requestIds[0], requestIds[1]);
  assert.deepEqual(retryDelays, [1000]);
  assert.equal(messages.filter((message) => message.role === 'user').length, 1);
  assert.deepEqual(statusUpdates, ['Sent']);
  assert.equal(messages.some((message) => message.content === 'Failed to send message'), false);
});

test('a retry lands in the session the message was typed in after the view switches away', async () => {
  const h = buildLockHarness();
  captureSendMessages(h);
  let releaseRetry;
  h.context.setTimeout = (callback) => {
    releaseRetry = callback;
    return 1;
  };
  let attempt = 0;
  h.fetchImpl = async () => {
    attempt += 1;
    if (attempt === 1) throw new TypeError('offline');
    return {ok: true, status: 202, json: async () => ({})};
  };
  h.elements.get('msg-input').value = 'typed in session a';

  const sendPromise = h.context.sendMessage();
  await flush();
  h.context.SESSION_ID = 'session-b';
  releaseRetry();
  await sendPromise;

  assert.deepEqual(h.fetchCalls.map((call) => call.url), ['/api/chat/session-a/message', '/api/chat/session-a/message']);
});

test('a 5xx retries with the same request id', async () => {
  const h = buildLockHarness();
  const {messages} = captureSendMessages(h);
  const retryDelays = [];
  h.context.setTimeout = (callback, delay) => {
    retryDelays.push(delay);
    callback();
    return 1;
  };
  h.fetchImpl = async (url) => {
    if (!String(url).includes('/message')) return {ok: true, status: 200, json: async () => ({})};
    if (h.fetchCalls.filter((call) => String(call.url).includes('/message')).length === 1) {
      return {ok: false, status: 503};
    }
    return {ok: true, status: 202, json: async () => ({})};
  };
  h.elements.get('msg-input').value = 'server retry';

  await h.context.sendMessage();

  const requestIds = h.fetchCalls
    .filter((call) => String(call.url).includes('/message'))
    .map((call) => JSON.parse(call.opts.body).request_id);
  assert.equal(requestIds.length, 2);
  assert.equal(requestIds[0], requestIds[1]);
  assert.deepEqual(retryDelays, [1000]);
  assert.equal(messages.filter((message) => message.role === 'user').length, 1);
});

test('a 4xx stops retries, marks the bubble failed, and shows the existing failure line', async () => {
  const h = buildLockHarness();
  const {messages, statusUpdates} = captureSendMessages(h);
  let messageAttempts = 0;
  h.fetchImpl = async (url) => {
    if (String(url).includes('/message')) {
      messageAttempts += 1;
      return {ok: false, status: 409, json: async () => ({})};
    }
    return {ok: true, status: 200, json: async () => ({})};
  };
  let stopped = 0;
  h.context.stopThinking = () => {
    stopped += 1;
  };
  h.elements.get('msg-input').value = 'rejected message';

  await h.context.sendMessage();

  assert.equal(messageAttempts, 1);
  assert.equal(stopped, 1, 'the send that started the thinking round ends it on refusal');
  assert.deepEqual(statusUpdates, ['Failed']);
  assert.equal(messages.filter((message) => message.role === 'user')[0].delivery_status, 'Failed');
  assert.equal(messages.filter((message) => message.content === 'Failed to send message').length, 1);
  assert.deepEqual([...h.context.__pendingRequestIds()], []);
});

test('a 202 dispatch_error keeps the bubble sent and adds one system line without retry', async () => {
  const h = buildLockHarness();
  const {messages, statusUpdates} = captureSendMessages(h);
  let attempts = 0;
  h.fetchImpl = async () => {
    attempts += 1;
    return {ok: true, status: 202, json: async () => ({dispatch_error: 'run already active'})};
  };
  h.elements.get('msg-input').value = 'accepted message';

  await h.context.sendMessage();

  assert.equal(attempts, 1);
  assert.deepEqual(statusUpdates, ['Sent']);
  assert.equal(messages.filter((message) => message.role === 'user')[0].delivery_status, 'Sent');
  assert.equal(messages.filter((message) => message.role === 'system' && message.content === 'run already active').length, 1);
});

// ---------------------------------------------------------------------------
// 5. The send gate names the in-flight category; /compact shares the gate.
// ---------------------------------------------------------------------------
test('the send gate names the in-flight category and /compact passes the same gate', async () => {
  const h = buildLockHarness();
  const completeUpload = holdUploadOpen(h);

  const uploadPromise = h.context.uploadFile({name: 'a.txt', size: 3});
  await flush();
  h.toasts.length = 0;
  assert.equal(h.context.blockIfUploadsInFlight(), true);
  assert.deepEqual(h.toasts, [{message: 'Please wait for the attachment upload to finish', isError: true}]);

  completeUpload({path: '/tmp/a.txt', filename: 'a.txt', size: 3});
  await uploadPromise;

  const run = makeVoiceRun(h, 'session-a');
  h.context.__claimVoiceRun(run);
  h.context.startUpload(run);
  await flush(); // startUpload awaits the recording's settled devices first
  h.toasts.length = 0;
  assert.equal(h.context.blockIfUploadsInFlight(), true);
  assert.deepEqual(h.toasts, [{message: 'Please wait for the voice transcription to finish', isError: true}]);

  let confirms = 0;
  h.context.confirm = () => { confirms += 1; return true; };
  h.fetchCalls.length = 0;
  await h.context.compactContext();
  assert.equal(confirms, 0, 'the gate answered before the confirm dialog');
  assert.deepEqual(h.fetchCalls.filter((call) => String(call.url).includes('/message')), [], 'no compact request left the page');
  assert.deepEqual([...h.context.__pendingRequestIds()], []);

  run.xhr.respond(200, {text: 'done'});
  await flush();
  assert.equal(sendLocked(h), false);

  h.toasts.length = 0;
  h.fetchCalls.length = 0;
  await h.context.compactContext();
  assert.equal(confirms, 1, 'with nothing in flight the dialog runs');
  assert.equal(h.fetchCalls.filter((call) => String(call.url).includes('/message')).length, 1);
  assert.equal(h.context.__pendingRequestIds().length, 1);
  assert.deepEqual(h.toasts, []);
});
