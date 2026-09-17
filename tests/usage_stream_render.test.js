const assert = require('node:assert/strict');
const test = require('node:test');
const { buildStreamHarness, FAKE_MARKED_SRC } = require('./stream_render_harness');

function loadUsage() {
  const h = buildStreamHarness(FAKE_MARKED_SRC);
  return { ...h, frames: h.stats().frames, lastHtml: () => h.stats().frames.at(-1) || '' };
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
  // A line-cutting fake marked: lexer emits a space token per newline so
  // streamSafeCut freezes cuts at line ends and the incremental path engages
  // (the default fake's single paragraph token never cuts).
  const cuttingMarked =
    'globalThis.marked = { Renderer: function() { return {}; }, use() {}, ' +
    'parse: (s) => s.split("\\n").filter(Boolean).map((l) => `<p>${l}</p>`).join(""), ' +
    'lexer: (s) => s.split("\\n").flatMap((l) => l ? ' +
    '[{ type: "paragraph", raw: l + "\\n", text: l }, { type: "space", raw: "\\n" }] : []), ' +
    'parser: (tokens) => tokens.map((t) => t.type === "space" ? "" : `<p>${t.text}</p>`).join("") };';
  const h0 = buildStreamHarness(cuttingMarked);
  const h = { ...h0, frames: h0.stats().frames, lastHtml: () => h0.stats().frames.at(-1) || '' };
  let parses = 0;
  const realParse = h.context.marked.parse;
  h.context.marked.parse = (s) => { parses += 1; return realParse(s); };
  h.showStreaming({ content: 'one\ntwo\nthree\n' });
  h.advance(250);
  const parsesAfterTurn = parses;
  // The switch shape: teardown hides the stream, the render re-shows the same
  // pending draft past the coalesce window (the synchronous-paint branch).
  h.context.hideStreaming();
  h.advance(250);
  h.showStreaming({ content: 'one\ntwo\nthree\n' });
  h.context.hideStreaming();
  h.advance(250);
  h.showStreaming({ content: 'one\ntwo\nthree\nfour\n' });
  assert.equal(parses, parsesAfterTurn,
      'the re-shows re-parsed the draft instead of serving the frozen prefix');
  assert.match(h.lastHtml(), /four/);
});

test('a hide followed by a different draft parses fresh', () => {
  const h = loadUsage();
  h.showStreaming({ content: 'first session draft' });
  h.advance(250);
  h.context.hideStreaming();
  h.showStreaming({ content: 'other session draft' });
  assert.match(h.lastHtml(), /other session draft/);
  assert.ok(!h.lastHtml().includes('first session'), 'stale draft content leaked into the new stream');
});
