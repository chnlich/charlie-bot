// ---------------------------------------------------------------------------
// Send-button lock: #send-btn's disabled state has one writer — the in-flight
// count lock (file-upload.js refreshSendLock) driven by attachment uploads plus
// voice transcription windows. Thinking state never writes the button, so a
// master run leaves it sendable; each optimistic send's server echo is skipped
// exactly once via the pendingUserEchoes count. Driven through the real chat,
// sidebar, websocket, file-upload and voice-input modules on the
// session_context_stub harness, in the style of chat_attachments_render.test.js
// (send button as an attribute-map fake) and voice_input_run.test.js (fake XHR).
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic, chatModules, sidebarModules} = require('./read_static');
const {createElement, createClassList} = require('./dom_element_stub');
const {
  baseSessionContext,
  installSessionDocumentLookups,
  stubPageTimers,
  createChatSidebarContext,
} = require('./session_context_stub');

const SLASH_COMMANDS_JS = readStatic('slash-commands.js');
const FILE_UPLOAD_JS = readStatic('file-upload.js');
const WEBSOCKET_JS = readStatic('websocket.js');
const VOICE_INPUT_JS = readStatic('voice-input.js');

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

// The run shape startRecording builds; the lock tests enter the transcription
// window directly through startUpload with the slot claimed below.
function makeVoiceRun(sessionId) {
  return {
    sessionId,
    stream: null,
    audioContext: null,
    sourceNode: null,
    workletNode: null,
    recording: false,
    stopping: false,
    phase: 'idle',
    pcmChunks: [],
    totalSamples: 0,
    level: 0,
    confirmFired: false,
    confirmedText: '',
    confirmedSpan: null,
    xhr: null,
    requestInFlight: false,
    uploadTimedOut: false,
    uploadTimer: null,
    flushId: 0,
    flushResolvers: new Map(),
    ui: null,
    backend: {id: 'local', label: 'Local (sherpa)', livePartials: false},
    devicesPromise: null,
    relaySocket: null,
    relayFinalResolve: null,
    relayWaitTimer: null,
    transcript: null,
  };
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
  // Call-time refs the page loads from config.js / context-panel.js, plus the
  // event cursor the inline bootstrap owns: none of them ride the modules.
  context.saveDraft = () => {};
  context.applyWorkingContext = (content) => content;
  context.eventCursor = 0;
  context.window = Object.assign(context.window, {removeEventListener() {}});
  context.XMLHttpRequest = FakeVoiceXhr;
  context.FormData = class {
    append() {}
    get() { return null; }
  };
  context.Blob = require('node:buffer').Blob;
  context.accessTokenAuthorization = () => null;
  context.VOICE_BACKENDS = [{id: 'local', label: 'Local (sherpa)', livePartials: false, unavailableReason: null}];
  context.VOICE_DEFAULT_BACKEND = 'local';

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
  vm.runInContext(SLASH_COMMANDS_JS, context, {filename: 'slash-commands.js'});
  vm.runInContext(FILE_UPLOAD_JS, context, {filename: 'file-upload.js'});
  vm.runInContext(WEBSOCKET_JS, context, {filename: 'websocket.js'});
  vm.runInContext(VOICE_INPUT_JS, context, {filename: 'voice-input.js'});
  // The voice slot and the echo count are the modules' top-level lets: their
  // lexical bindings shadow any context property, so tests claim and read them
  // from inside the context.
  context.__claimVoiceRun = vm.runInContext('(run) => { activeVoiceRun = run; }', context);
  context.__echoCount = vm.runInContext('() => pendingUserEchoes', context);
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
  assert.equal(h.context.__echoCount(), 1);

  resolveSend({ok: true});
  await flush();
  assert.equal(sendLocked(h), false);
});

test('a slash dispatch starts thinking without locking the send button', async () => {
  const h = buildLockHarness();
  h.fetchImpl = async () => ({ok: true, json: async () => ({type: 'prompt_dispatched'})});

  await h.context.executeSlashCommand('prompt', 'write a haiku');

  assert.equal(h.context.masterThinking, true, 'the dispatched prompt is thinking');
  assert.equal(sendLocked(h), false);
  assert.equal(sendGreyed(h), false);
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

  const run = makeVoiceRun('session-a');
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
  const run = makeVoiceRun('session-a');
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
  const run = makeVoiceRun('session-a');
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
test('two sends fired back to back each skip exactly their own echo', async () => {
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
  assert.equal(h.context.__echoCount(), 2, 'two sends, two pending echoes');
  assert.equal(messages.children.length, 2, 'two local bubbles, one per send');

  h.context.handleWSEvent({type: 'message', message: {role: 'user', content: 'first message', id: 'srv-1'}}, 'session-a', 0);
  h.context.handleWSEvent({type: 'message', message: {role: 'user', content: 'second message', id: 'srv-2'}}, 'session-a', 0);
  assert.equal(h.context.__echoCount(), 0, 'each echo consumed one pending count');
  assert.equal(messages.children.length, 2, 'neither echo painted a second bubble');

  // An echo with no pending send — another tab's message — still renders.
  h.context.handleWSEvent({type: 'message', message: {role: 'user', content: 'from another tab', id: 'srv-3'}}, 'session-a', 0);
  assert.equal(messages.children.length, 3, 'the skip is per-send consumption, not a latch');
  assert.match(messages.children[2].innerHTML, /from another tab/);

  resolvers.forEach((resolve) => resolve({ok: true}));
  await flush();
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

  const run = makeVoiceRun('session-a');
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
  assert.equal(h.context.__echoCount(), 0);

  run.xhr.respond(200, {text: 'done'});
  await flush();
  assert.equal(sendLocked(h), false);

  h.toasts.length = 0;
  h.fetchCalls.length = 0;
  await h.context.compactContext();
  assert.equal(confirms, 1, 'with nothing in flight the dialog runs');
  assert.equal(h.fetchCalls.filter((call) => String(call.url).includes('/message')).length, 1);
  assert.equal(h.context.__echoCount(), 1);
  assert.deepEqual(h.toasts, []);
});
