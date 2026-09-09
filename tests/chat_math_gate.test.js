'use strict';
const assert = require('node:assert/strict');
const test = require('node:test');
const { buildStreamHarness } = require('./stream_render_harness');

// Fake marked keeps the parse deterministic; the gate under test wraps the
// renderMathInElement call, not the parse.
const FAKE_MARKED_SRC =
  'globalThis.marked = { Renderer: function() { return {}; }, use() {}, parse: (s) => `<p>${s}</p>`, ' +
  'lexer: (s) => [{ type: "paragraph", raw: s, text: s }], ' +
  'parser: (tokens) => tokens.map((t) => `<p>${t.text}</p>`).join("") };';

function loadRenderer() {
  const h = buildStreamHarness(FAKE_MARKED_SRC);
  const calls = [];
  h.context.renderMathInElement = (el, opts) => calls.push({ el, opts });
  return { h, calls };
}

function elWithRaw(raw) {
  return { dataset: raw === null ? {} : { raw } };
}

test('hasMathDelimiter matches every configured delimiter family', () => {
  const { h } = loadRenderer();
  assert.equal(h.context.hasMathDelimiter('plain text'), false);
  assert.equal(h.context.hasMathDelimiter('inline $x$ / display $$y$$'), true);
  assert.equal(h.context.hasMathDelimiter('bracket \\[x\\] / paren \\(y\\)'), true);
});

test('renderChatMath skips the walk when the source carries no math delimiter', () => {
  const { h, calls } = loadRenderer();
  h.context.renderChatMath(elWithRaw('a *b* c\n\n```js\nvar x = 1;\n```'));
  h.context.renderChatMath(elWithRaw(null), 'plain draft text');
  h.context.renderChatMath({}, '');
  assert.equal(calls.length, 0);
});

test('renderChatMath walks when the source carries a delimiter, or none at all', () => {
  const { h, calls } = loadRenderer();
  h.context.renderChatMath(elWithRaw('costs $5 and $10'));
  h.context.renderChatMath(elWithRaw(null), 'bounds \\(a\\) and \\[b\\]');
  h.context.renderChatMath({});
  h.context.renderChatMath({ dataset: undefined });
  assert.equal(calls.length, 4);
});

function replayPaints(drafts) {
  const { h, calls } = loadRenderer();
  for (const draft of drafts) h.showStreaming(draft);
  h.advance(200);
  return { frames: h.stats().frames.length, calls: calls.length };
}

test('a math-free streamed paint never calls the walk', () => {
  const r = replayPaints([
    { content: 'plain draft text\n\n- item one\n- item two' },
    { content: 'plain draft text\n\n- item one\n- item two, more' },
  ]);
  assert.deepEqual(r, { frames: 2, calls: 0 });
});

test('a math-bearing streamed paint walks once per paint', () => {
  assert.deepEqual(replayPaints([
    { content: 'draft with $x^2$ inline' },
    { content: 'draft with $x^2$ inline, more' },
  ]), { frames: 2, calls: 2 });
});

test('thinking text alone gates the paint walk', () => {
  assert.deepEqual(replayPaints([
    { content: 'plain', thinking: 'step $\\int x$' },
    { content: 'plain', thinking: 'plain thinking' },
  ]), { frames: 2, calls: 1 });
});
