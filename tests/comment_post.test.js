const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { readStatic } = require('./read_static');

const COMMENT_POST_JS = readStatic('comment_post.js');

function loadCommentPost(fetchImpl) {
  const context = { fetch: fetchImpl };
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
    return { ok: true, status: 200 };
  });

  await context.postCommentMessage('session 7', 'two lines\nof comment');

  assert.equal(calls.length, 1);
  const call = calls[0];
  assert.equal(call.url, '/api/chat/session%207/message');
  assert.equal(call.options.method, 'POST');
  assert.equal(call.options.credentials, 'same-origin');
  assert.equal(call.options.headers['Content-Type'], 'application/json');
  assert.equal(call.options.body, JSON.stringify({ content: 'two lines\nof comment', uploaded_files: [] }));
});

test('postCommentMessage throws the auth message on 401 and the HTTP message otherwise', async () => {
  const unauthorized = loadCommentPost(async () => ({ ok: false, status: 401 }));
  await assert.rejects(unauthorized.postCommentMessage('s', 'c'), { message: 'log in to comment' });

  const serverError = loadCommentPost(async () => ({ ok: false, status: 502 }));
  await assert.rejects(serverError.postCommentMessage('s', 'c'), { message: 'Comment post failed: HTTP 502' });
});
