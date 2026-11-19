const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const COMMENT_POST_JS = readStatic('comment_post.js');

function loadCommentPost(fetchImpl) {
  let requestSequence = 0;
  const context = {
    fetch: fetchImpl,
    crypto: {randomUUID: () => `comment-request-${++requestSequence}`},
    setTimeout: (callback) => { callback(); return 1; },
  };
  vm.createContext(context);
  vm.runInContext(COMMENT_POST_JS, context, { filename: 'comment_post.js' });
  return context;
}

// Minimal textarea/old-node double for swapInInlineEditor: records the
// addEventListener registrations so the test can fire keydown and blur.
function editorDom(initialValue) {
  const listeners = {};
  const textarea = {
    value: initialValue,
    addEventListener(name, fn) {
      listeners[name] = fn;
    },
    focus() {},
    select() {},
  };
  const oldNode = {
    parentNode: {
      replaceChild(node, ref) {
        listeners.swapped = node === textarea && ref === oldNode;
      },
    },
  };
  return {
    textarea,
    oldNode,
    listeners,
    fireKey(event) {
      listeners.keydown({...event, preventDefault() {}});
    },
    fireBlur() {
      listeners.blur();
    },
  };
}

test('swapInInlineEditor swaps the node in and treats an empty edit as a cancel', () => {
  const context = loadCommentPost();
  const events = [];
  const finished = [];
  const editor = editorDom('   ');
  context.swapInInlineEditor(editor.textarea, editor.oldNode, (value) => events.push(['commit', value]), () => finished.push(1));

  assert.equal(editor.listeners.swapped, true);
  editor.fireBlur();

  assert.deepEqual(events, [], 'an all-whitespace edit must not commit');
  assert.equal(finished.length, 1, 'cancel still runs finish once');
});

test('swapInInlineEditor commits the raw text then finishes exactly once', () => {
  const context = loadCommentPost();
  const events = [];
  const finished = [];
  const editor = editorDom(' hello ');
  context.swapInInlineEditor(editor.textarea, editor.oldNode, (value) => events.push(['commit', value]), () => finished.push(1));

  editor.fireKey({key: 'Escape'});
  assert.deepEqual(events, [], 'Escape is a cancel: no commit');
  assert.equal(finished.length, 1);

  editor.fireBlur();
  assert.equal(finished.length, 1, 'the done-once flag keeps the second end inert');

  const retry = editorDom(' hello ');
  context.swapInInlineEditor(retry.textarea, retry.oldNode, (value) => events.push(['commit', value]), () => finished.push(1));
  retry.fireKey({key: 'Enter', ctrlKey: true});

  assert.deepEqual(events, [['commit', ' hello ']], 'commit receives the raw text; the tray applies its own trim');
  assert.equal(finished.length, 2);
});

test('postCommentMessage sends the comment-tray request shape', async () => {
  const calls = [];
  const context = loadCommentPost(async (url, options = {}) => {
    calls.push({ url, options });
    return {ok: true, status: 202, json: async () => ({})};
  });

  await context.postCommentMessage('session 7', 'two lines\nof comment');

  assert.equal(calls.length, 1);
  const call = calls[0];
  assert.equal(call.url, '/api/chat/session%207/message');
  assert.equal(call.options.method, 'POST');
  assert.equal(call.options.credentials, 'same-origin');
  assert.equal(call.options.headers['Content-Type'], 'application/json');
  const body = JSON.parse(call.options.body);
  assert.equal(body.content, 'two lines\nof comment');
  assert.deepEqual(body.uploaded_files, []);
  assert.equal(body.request_id, 'comment-request-1');
});

test('postCommentMessage retries a network error and a 5xx with one request id', async () => {
  const calls = [];
  const context = loadCommentPost(async (url, options = {}) => {
    calls.push({url, options});
    if (calls.length === 1) throw new TypeError('offline');
    if (calls.length === 2) return {ok: false, status: 503};
    return {ok: true, status: 202, json: async () => ({})};
  });
  const delays = [];
  context.setTimeout = (callback, delay) => { delays.push(delay); callback(); return 1; };

  await context.postCommentMessage('s', 'c');

  assert.deepEqual(delays, [1000, 2000]);
  assert.equal(calls.length, 3);
  assert.deepEqual(calls.map((call) => JSON.parse(call.options.body).request_id), [
    'comment-request-1', 'comment-request-1', 'comment-request-1',
  ]);
});

test('postCommentMessage treats a 202 dispatch_error as accepted without retry', async () => {
  let attempts = 0;
  const context = loadCommentPost(async () => {
    attempts += 1;
    return {ok: true, status: 202, json: async () => ({dispatch_error: 'run already active'})};
  });

  const result = await context.postCommentMessage('s', 'c');

  assert.equal(attempts, 1);
  assert.deepEqual(result, {dispatch_error: 'run already active'});
});

test('postCommentMessage stops on a 4xx and preserves the auth and HTTP messages', async () => {
  const unauthorized = loadCommentPost(async () => ({ ok: false, status: 401 }));
  await assert.rejects(unauthorized.postCommentMessage('s', 'c'), { message: 'log in to comment' });

  let attempts = 0;
  const badRequest = loadCommentPost(async () => {
    attempts += 1;
    return {ok: false, status: 409};
  });
  await assert.rejects(badRequest.postCommentMessage('s', 'c'), {message: 'Comment post failed: HTTP 409'});
  assert.equal(attempts, 1);
});

// The undo window runs on a clock the test owns: each setTimeout call queues its step, and the test runs them.
function loadCommentPostWithClock(fetchImpl) {
  const timers = [];
  const cleared = [];
  let requestSequence = 0;
  const context = {
    fetch: fetchImpl,
    crypto: {randomUUID: () => `comment-request-${++requestSequence}`},
    setTimeout: (callback) => {
      timers.push(callback);
      return timers.length;
    },
    clearTimeout: (id) => { cleared.push(id); },
  };
  vm.createContext(context);
  vm.runInContext(COMMENT_POST_JS, context, {filename: 'comment_post.js'});
  return {context, timers, cleared};
}

const ACCEPTED = async () => ({ok: true, status: 202, json: async () => ({input_event_id: 'event-1'})});

test('startUndoableCommentSend counts five seconds down, then posts once and settles with the result', async () => {
  const calls = [];
  const {context, timers} = loadCommentPostWithClock(async (url, options) => {
    calls.push({url, body: JSON.parse(options.body)});
    return ACCEPTED();
  });
  const ticks = [];
  const handle = context.startUndoableCommentSend('session 7', 'the picks', (left) => ticks.push(left));
  while (timers.length > 0) timers.shift()();

  assert.deepEqual(ticks, [5, 4, 3, 2, 1, 0]);
  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, '/api/chat/session%207/message');
  assert.equal(calls[0].body.content, 'the picks');
  assert.deepEqual(await handle.sent, {input_event_id: 'event-1'});
});

test('cancel inside the window posts nothing and settles null', async () => {
  const calls = [];
  const {context, timers, cleared} = loadCommentPostWithClock(async (url) => {
    calls.push(url);
    return ACCEPTED();
  });
  const ticks = [];
  const handle = context.startUndoableCommentSend('s', 'c', (left) => ticks.push(left));
  assert.equal(handle.cancel(), true);

  assert.deepEqual(cleared, [1], 'the pending tick is cleared');
  assert.equal(await handle.sent, null);
  assert.deepEqual(ticks, [5]);
  assert.deepEqual(calls, []);
  assert.equal(timers.length, 1, 'only the first tick was ever queued');
});

test('cancel after the post has begun returns false and the message still lands', async () => {
  const calls = [];
  const {context, timers} = loadCommentPostWithClock(async (url) => {
    calls.push(url);
    return ACCEPTED();
  });
  const handle = context.startUndoableCommentSend('s', 'c', () => {});
  while (timers.length > 0) timers.shift()();

  assert.equal(handle.cancel(), false);
  assert.deepEqual(await handle.sent, {input_event_id: 'event-1'});
  assert.equal(calls.length, 1);
});

test('a 4xx answer rejects the undo window with the post error', async () => {
  const {context, timers} = loadCommentPostWithClock(async () => ({ok: false, status: 409}));
  const handle = context.startUndoableCommentSend('s', 'c', () => {});
  while (timers.length > 0) timers.shift()();

  await assert.rejects(handle.sent, {message: 'Comment post failed: HTTP 409'});
});
