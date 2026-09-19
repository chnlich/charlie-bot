const assert = require('node:assert/strict');
const test = require('node:test');
const { buildStreamHarness, FAKE_MARKED_SRC, CUTTING_MARKED_SRC } = require('./stream_render_harness');

function adapt(h0) {
  return { ...h0, frames: h0.stats().frames, lastHtml: () => h0.stats().frames.at(-1) || '' };
}

function loadUsage() {
  return adapt(buildStreamHarness(FAKE_MARKED_SRC));
}

// Both reuse-path tests assert which strings the lexer re-ran across the
// switch-shaped hide+re-show, so the harness records every lexer input.
function loadCuttingUsage() {
  const h = adapt(buildStreamHarness(CUTTING_MARKED_SRC));
  const lexerInputs = [];
  const realLexer = h.context.marked.lexer;
  h.context.marked.lexer = (s) => { lexerInputs.push(s); return realLexer(s); };
  return { h, lexerInputs };
}

test('first delta paints synchronously at the leading edge', () => {
  const h = loadUsage();
  h.showStreaming({ content: 'hello' });
  assert.equal(h.frames.length, 1);
  assert.match(h.lastHtml(), /hello/);
});

test('a same-window burst coalesces and the trailing paint carries the last draft', () => {
  const h = loadUsage();
  for (let i = 1; i <= 10; i++) h.showStreaming({ content: `d${i}` });
  assert.equal(h.frames.length, 1);
  h.advance(200);
  assert.equal(h.frames.length, 2);
  assert.equal(h.stats().timerCount, 0);
  assert.match(h.frames[0], /d1/);
  assert.match(h.lastHtml(), /d10/);
  assert.ok(h.frames.every((p) => !p.includes('d5')), 'a mid-burst draft painted');
});

test('hideStreaming cancels a pending trailing paint', () => {
  const h = loadUsage();
  h.showStreaming({ content: 'first' });
  h.showStreaming({ content: 'pending' });
  h.context.hideStreaming();
  h.advance(1000);
  assert.equal(h.frames.length, 1);
  assert.equal(h.context.document.getElementById('streaming-content').innerHTML, '');
});

test('deltas spaced past the cadence each paint at the leading edge', () => {
  const h = loadUsage();
  for (let i = 0; i < 4; i++) {
    h.showStreaming({ content: `draft ${i}` });
    h.advance(250);
  }
  assert.equal(h.frames.length, 4);
  assert.match(h.lastHtml(), /draft 3/);
});

test('a switch-shaped hide+re-show of the same draft reuses the parse state', () => {
  const { h, lexerInputs } = loadCuttingUsage();
  h.showStreaming({ content: 'one\ntwo\nthree\n' });
  h.advance(250);
  const lexesAfterTurn = lexerInputs.length;
  // The switch shape: teardown hides the stream, the render re-shows the same
  // pending draft past the coalesce window (the synchronous-paint branch).
  h.context.hideStreaming();
  h.advance(250);
  h.showStreaming({ content: 'one\ntwo\nthree\n' });
  h.context.hideStreaming();
  h.advance(250);
  h.showStreaming({ content: 'one\ntwo\nthree\nfour\n' });
  assert.ok(!lexerInputs.slice(lexesAfterTurn).some((s) => s.startsWith('one')),
      'the re-shows re-lexed the draft instead of the appended tail');
  assert.match(h.lastHtml(), /four/);
});

test('a hide followed by a different draft parses fresh', () => {
  const { h, lexerInputs } = loadCuttingUsage();
  h.showStreaming({ content: 'first\nsession\ndraft\n' });
  h.advance(250);
  h.context.hideStreaming();
  h.advance(250);
  h.showStreaming({ content: 'other\nsession\ndraft\n' });
  assert.ok(lexerInputs.some((s) => s.startsWith('other')),
      'the different draft did not parse fresh');
  assert.match(h.lastHtml(), /<p>other<\/p>/);
  assert.ok(!h.lastHtml().includes('first'), 'stale draft content leaked into the new stream');
});
