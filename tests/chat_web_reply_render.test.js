const assert = require('node:assert/strict');
const test = require('node:test');

const {loadChatRendering} = require('./chat_rendering_context_stub');

// ---------------------------------------------------------------------------
// The two message roles the web reply channel adds to the chat: a web_reply is
// the agent's message to the user (an agent-side bubble holding its markdown),
// and a web_reply_notice is a one-line centered notice of plain text.
// ---------------------------------------------------------------------------

test('a web reply renders as an agent-side bubble around its markdown body', () => {
  const context = loadChatRendering();
  const html = context.renderMessage(
      {role: 'web_reply', id: 'r1', content: '**done**, see the page', timestamp: null}, 'sess-1');
  assert.ok(html.includes('data-message-role="web_reply"'));
  assert.ok(html.includes('rounded-bl-md'), 'the agent side of the chat');
  assert.ok(html.includes('class="prose-msg" data-raw="**done**, see the page"'), 'the markdown body');
});

test('a web reply notice renders centered, with its text escaped as plain text', () => {
  const context = loadChatRendering();
  const html = context.renderMessage(
      {role: 'web_reply_notice', id: 'n1', content: '<b>no reply</b>', timestamp: null}, 'sess-1');
  assert.ok(html.includes('data-message-role="web_reply_notice"'));
  assert.ok(html.includes('justify-center'));
  assert.ok(!html.includes('<b>'), 'the notice text is not markup');
  assert.ok(html.includes('&lt;b&gt;no reply&lt;/b&gt;'));
});
