const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');
const {TextEncoder} = require('node:util');

const { readStatic } = require('./read_static');

const COMMENT_POST_JS = readStatic('comment_post.js');

function loadCommentPost(fetchImpl) {
  let requestSequence = 0;
  const context = {
    fetch: fetchImpl,
    crypto: {randomUUID: () => `comment-request-${++requestSequence}`},
    TextEncoder,
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

test('postCommentMessage reports an oversized keepalive body and allows the same message while the page stays open',
  async () => {
    const calls = [];
    const context = loadCommentPost(async (url, options = {}) => {
      calls.push({url, options});
      return {ok: true, status: 202, json: async () => ({})};
    });
    const content = 'é'.repeat(32768);

    await assert.rejects(context.postCommentMessage('s', content, {keepalive: true}), /64 KiB/);
    assert.equal(calls.length, 0, 'the over-limit request is rejected before fetch');

    await context.postCommentMessage('s', content);
    assert.equal(calls.length, 1, 'the normal send is not subject to the keepalive limit');
    assert.ok(Buffer.byteLength(calls[0].options.body, 'utf8') > 64 * 1024);
  });

test('buildClickMessage owns the shared numbered reference and answer format', () => {
  const context = loadCommentPost();
  const message = context.buildClickMessage('[Status · Waiting on you]', [{
    reference: 'Waiting on you · session session-a · list 0123456789abcdef · since 2026-11-06T16:50:57Z · #2',
    answer: 'Done',
  }]);

  assert.equal(message, [
    '[Status · Waiting on you]',
    '',
    '1. ▸ Waiting on you · session session-a · list 0123456789abcdef · since 2026-11-06T16:50:57Z · #2',
    '   ↳ Done',
  ].join('\n'));

  const twoQuestions = context.buildClickMessage('[Status · Waiting on you]', [{
    reference: 'Waiting on you · session session-a · list 0123456789abcdef · since 2026-11-06T16:50:57Z · #2',
    answer: ['Q1 「keep」: keep the file.', 'Q2 「now」: act now.'],
  }]);
  assert.equal(twoQuestions, [
    '[Status · Waiting on you]',
    '',
    '1. ▸ Waiting on you · session session-a · list 0123456789abcdef · since 2026-11-06T16:50:57Z · #2',
    '   ↳ Q1 「keep」: keep the file.',
    '   ↳ Q2 「now」: act now.',
  ].join('\n'));
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

test('postCommentMessage surfaces a rejected keepalive request instead of retrying after page leave', async () => {
  let attempts = 0;
  const context = loadCommentPost(async () => {
    attempts += 1;
    throw new TypeError('keepalive quota exhausted');
  });

  await assert.rejects(context.postCommentMessage('s', 'c', {keepalive: true}), /keepalive quota exhausted/);
  assert.equal(attempts, 1, 'the caller gets the failure so the tray can show its next action');
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
  const makeEventTarget = () => {
    const listeners = new Map();
    return {
      addEventListener(name, listener) {
        const registered = listeners.get(name) || [];
        registered.push(listener);
        listeners.set(name, registered);
      },
      removeEventListener(name, listener) {
        const registered = listeners.get(name) || [];
        listeners.set(name, registered.filter((current) => current !== listener));
      },
      dispatch(name) {
        for (const listener of listeners.get(name) || []) listener();
      },
    };
  };
  const context = {
    fetch: fetchImpl,
    crypto: {randomUUID: () => `comment-request-${++requestSequence}`},
    TextEncoder,
    window: makeEventTarget(),
    document: makeEventTarget(),
    setTimeout: (callback, delay) => {
      timers.push({callback, delay});
      return timers.length;
    },
    clearTimeout: (id) => { cleared.push(id); },
  };
  vm.createContext(context);
  vm.runInContext(COMMENT_POST_JS, context, {filename: 'comment_post.js'});
  return {context, timers, cleared};
}

const ACCEPTED = async () => ({ok: true, status: 202, json: async () => ({input_event_id: 'event-1'})});

test('startUndoableSend waits five seconds, then runs post once inside the timer and settles with its result', async () => {
  const {context, timers} = loadCommentPostWithClock(ACCEPTED);
  const calls = [];
  const handle = context.startUndoableSend(async (options) => {
    calls.push(options);
    return {input_event_id: 'event-1'};
  });
  assert.deepEqual(timers.map((timer) => timer.delay), [5000]);
  assert.equal(calls.length, 0);

  timers.shift().callback();

  assert.deepEqual(calls.map((options) => options.keepalive), [false],
    'post runs when the window closes, with no extra tick, and not keepalive');
  assert.equal(handle.sending, true);
  assert.equal(handle.cancel(), false, 'cancel after the post has begun returns false');
  assert.deepEqual(await handle.sent, {input_event_id: 'event-1'});
});

test('cancel inside the window posts nothing and settles null', async () => {
  const {context, timers, cleared} = loadCommentPostWithClock(ACCEPTED);
  let posts = 0;
  const handle = context.startUndoableSend(async () => {
    posts += 1;
  });
  assert.equal(handle.cancel(), true);

  assert.deepEqual(cleared, [1], 'the pending timer is cleared');
  assert.equal(await handle.sent, null);
  assert.equal(handle.cancel(), false, 'a second cancel finds the window already closed');
  assert.equal(posts, 0);
  assert.equal(timers.length, 1, 'only the window timer was ever queued');
});

test('pagehide flushes the window at once, posting once with keepalive so the post outlives the page', async () => {
  const {context, cleared} = loadCommentPostWithClock(ACCEPTED);
  const calls = [];
  const handle = context.startUndoableSend(async (options) => {
    calls.push(options);
    return {input_event_id: 'event-1'};
  });
  context.window.dispatch('pagehide');

  assert.deepEqual(await handle.sent, {input_event_id: 'event-1'}, 'the flush posts and settles with its result');
  assert.deepEqual(cleared, [1], 'the pending window timer is cleared');
  assert.deepEqual(calls.map((options) => options.keepalive), [true], 'exactly one post, keepalive');
  assert.equal(handle.cancel(), false, 'a flush closes the undo window, so Undo finds nothing to cancel');
});

test('session-view teardown flushes the window at once, posting once with keepalive', async () => {
  const {context, cleared} = loadCommentPostWithClock(ACCEPTED);
  const calls = [];
  const handle = context.startUndoableSend(async (options) => {
    calls.push(options);
    return {input_event_id: 'event-1'};
  });
  context.document.dispatch('charliebot:session-view-teardown');

  assert.deepEqual(await handle.sent, {input_event_id: 'event-1'});
  assert.deepEqual(cleared, [1]);
  assert.deepEqual(calls.map((options) => options.keepalive), [true], 'exactly one post, keepalive');
  context.window.dispatch('pagehide');
  assert.equal(calls.length, 1, 'one leave event does not post a second time');
});

test('a failed post rejects the undo window with the post error', async () => {
  const {context, timers} = loadCommentPostWithClock(async () => ({ok: false, status: 409}));
  const handle = context.startUndoableSend(() => context.postCommentMessage('s', 'c'));
  timers.shift().callback();

  await assert.rejects(handle.sent, {message: 'Comment post failed: HTTP 409'});
});
