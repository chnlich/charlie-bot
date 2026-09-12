// The switch bootstrap's tool rows: the server-side preview cap marks
// input_truncated/output_truncated, and renderToolActivity surfaces each
// marker as a "full text in the session's raw events" note. A capped output
// (500 chars, the renderer's own split) renders plain — no dead reveal toggle.
const assert = require('node:assert/strict');
const test = require('node:test');

const {loadChatRendering} = require('./chat_rendering_context_stub');

test('input_truncated and output_truncated markers render their raw-events notes', () => {
  const context = loadChatRendering();
  const html = context.renderMessage({
    role: 'assistant',
    content: 'x',
    tools: [
      {name: 'Bash', input: {command: 'git ' + 'x'.repeat(500)}, input_truncated: true,
       output: 'o'.repeat(500), output_truncated: true},
    ],
  }, 'session-a');

  assert.match(html, /output truncated/);
  assert.match(html, /input truncated/);
  assert.equal((html.match(/truncated &mdash; full text in the session/g) || []).length, 2);
});

test('an output capped at exactly the renderer split renders plain, still noted', () => {
  const context = loadChatRendering();
  const html = context.renderMessage({
    role: 'assistant',
    content: 'x',
    tools: [
      {name: 'Read', input: {file_path: '/tmp/a.txt'}, output: 'o'.repeat(500), output_truncated: true},
    ],
  }, 'session-a');

  assert.match(html, /output truncated/);
  assert.ok(!html.includes('showMoreToggle'), 'no reveal toggle at the 500-char split');
});

test('an unmarked tool row renders neither note', () => {
  const context = loadChatRendering();
  const html = context.renderMessage({
    role: 'assistant',
    content: 'x',
    tools: [{name: 'Bash', input: {command: 'pwd'}, output: 'ok'}],
  }, 'session-a');

  assert.ok(!html.includes('output truncated'));
  assert.ok(!html.includes('input truncated'));
});
