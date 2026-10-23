const assert = require('node:assert/strict');
const test = require('node:test');

const {loadChatRendering} = require('./chat_rendering_context_stub');

// ---------------------------------------------------------------------------
// The two message roles the web reply channel adds to the chat: a web_reply is
// the agent's message to the user (an agent-side bubble holding its markdown),
// and a web_reply_notice is a one-line centered notice of plain text. The error
// row a failed turn writes (a system message whose text starts with "Error: ")
// closes the file.
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

// The usage lines and the reason that a failed charlie-code run writes to its error row.
const ERROR_TEXT = "Error: Agent error: Usage: charlie-code [OPTIONS]\n"
    + "Try 'charlie-code --help' for help.\n\n"
    + "Error: Invalid value: --image: '/tmp/example.jpg' is 5000000 bytes; the limit is 4194304 bytes (4 MiB)";

test('an error row renders left-aligned in the red palette with every line of its text', () => {
  const context = loadChatRendering();
  const timestamp = '2026-04-02T10:07:00.000Z';
  const html = context.renderMessage(
      {role: 'system', id: 'e1', content: ERROR_TEXT, timestamp}, 'sess-1');
  for (const line of ERROR_TEXT.split('\n').filter(Boolean)) {
    assert.ok(html.includes(context.escapeHtml(line)), `the row shows "${line}"`);
  }
  assert.ok(html.includes('whitespace-pre-wrap'), 'the line breaks show');
  assert.ok(html.includes('max-h-48 overflow-y-auto'), 'a long text scrolls inside the row');
  assert.ok(html.includes('justify-start'), 'the row sits on the agent side');
  assert.ok(html.includes(`title="${context.Chat.formatBubbleTime(timestamp)}"`), 'the row keeps its timestamp');
  assert.ok(!html.includes('truncate'), 'nothing cuts the text to one line');
});

test('a system row without the error prefix stays a centered one-line pill', () => {
  const context = loadChatRendering();
  const html = context.renderMessage(
      {role: 'system', id: 's1', content: 'Backend switched: one to two', timestamp: null}, 'sess-1');
  assert.ok(html.includes('justify-center'));
  assert.ok(html.includes('truncate'), 'the pill truncates its one line');
});
