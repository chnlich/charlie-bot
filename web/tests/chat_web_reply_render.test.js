const assert = require('node:assert/strict');
const test = require('node:test');

const {loadChatRendering} = require('./chat_rendering_context_stub');

// ---------------------------------------------------------------------------
// The message roles the web reply channel adds to the chat: a web_reply is the
// agent's message to the user (an agent-side bubble holding its markdown). A
// web_reply_notice is a one-line centered notice of plain text. A
// web_reply_check is the audit marker of one reply's text and draws nothing.
// The error row a failed turn writes (a system message whose text starts with
// "Error: ") follows. A handler result row carries its kind, and the row draws the
// check or the cross from that kind.
// ---------------------------------------------------------------------------

test('a web reply renders as an agent-side bubble around its markdown body', () => {
  const context = loadChatRendering();
  const html = context.renderMessage(
      {role: 'web_reply', id: 'r1', content: '**done**, see the page', timestamp: null}, 'sess-1');
  assert.ok(html.includes('data-message-role="web_reply"'));
  assert.ok(html.includes('rounded-bl-md'), 'the agent side of the chat');
  assert.ok(html.includes('class="prose-msg" data-raw="**done**, see the page"'), 'the markdown body');
});

test('a web reply in a thread session names its private destination beside the Web badge', () => {
  const context = loadChatRendering();
  context.getDisplayedSessionPlatform = () => 'slack';
  const html = context.renderMessage(
      {role: 'web_reply', id: 'r2', content: 'Private update.', timestamp: null}, 'sess-1');
  assert.match(html, />Web<\/span>/);
  assert.match(html, /Web reply · private, not posted to Slack/);
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

test('a web reply check draws nothing and does not reach the unknown-role error', () => {
  const context = loadChatRendering();
  const html = context.renderMessage(
      {role: 'web_reply_check', id: 'c1', content: 'web reply check', timestamp: null}, 'sess-1');
  assert.equal(html, '');
});

test('a reply cold-read check draws nothing, under its own role and the legacy role', () => {
  const context = loadChatRendering();
  for (const message of [
    {role: 'cold_read_check', name: 'reply', id: 'c1', content: 'web reply check', timestamp: null},
    {role: 'web_reply_check', id: 'c2', content: 'web reply check', timestamp: null},
  ]) {
    assert.equal(context.renderMessage(message, 'sess-1'), '', message.role);
  }
});

test('an item cold-read check renders as the centered pill, under its own role and the legacy role', () => {
  const context = loadChatRendering();
  for (const message of [
    {role: 'cold_read_check', name: 'ask', id: 'c1', content: 'Cold read of a Waiting on you item', timestamp: null},
    {role: 'cold_read_check', name: 'settle', id: 'c2', content: 'Cold read of the kept Waiting on you items', timestamp: null},
    {role: 'web_ask_check', id: 'c3', content: 'Cold read of a Waiting on you item', timestamp: null},
  ]) {
    const html = context.renderMessage(message, 'sess-1');
    assert.ok(html.includes('data-message-role="' + message.role + '"'), message.role);
    assert.ok(html.includes('justify-center'), message.role);
    assert.ok(html.includes('rounded-full'), message.role);
    assert.ok(html.includes(context.escapeHtml(message.content)), message.role);
  }
});

test('the For you view lists the cold-read checks under no role', () => {
  const {FOR_YOU_RULE} = require('./read_static');
  for (const role of ['cold_read_check', 'web_ask_check', 'web_reply_check']) {
    assert.ok(!FOR_YOU_RULE.roles.includes(role), role);
  }
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

test('a handler_ok row draws the check ahead of its task and message', () => {
  const context = loadChatRendering();
  const html = context.renderMessage(
      {role: 'system', kind: 'handler_ok', id: 'h1', content: 'lint: clean', timestamp: null}, 'sess-1');
  assert.ok(html.includes(context.cbIcon('done', 12) + ' lint: clean'), 'the check leads the text');
  assert.ok(html.includes('justify-center'), 'the row stays a centered pill');
  // A row stored before the kind field has no mark: its text keeps the check it was stored with.
  const legacy = context.renderMessage(
      {role: 'system', id: 'h0', content: '\u2713 lint: clean', timestamp: null}, 'sess-1');
  assert.ok(legacy.includes('\u2713 lint: clean'), 'the stored text shows as it was');
  assert.ok(!legacy.includes('class="cb-icon'), 'a row without a kind draws no icon');
});

test('a handler_error row draws the cross ahead of its task and message', () => {
  const context = loadChatRendering();
  const html = context.renderMessage(
      {role: 'system', kind: 'handler_error', id: 'h2', content: 'test: 2 failed', timestamp: null}, 'sess-1');
  assert.ok(html.includes(context.cbIcon('failed', 12) + ' test: 2 failed'), 'the cross leads the text');
  assert.ok(html.includes('justify-center'), 'the row stays a centered pill');
});
