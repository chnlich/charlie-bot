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
