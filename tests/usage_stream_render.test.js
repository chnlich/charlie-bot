const assert = require('node:assert/strict');
const test = require('node:test');
const { buildStreamHarness } = require('./stream_render_harness');
// Fake marked keeps the behavior tests deterministic: parse marks its input; the
// Renderer/use surface is what markdown-renderer.js touches at load.
const FAKE_MARKED_SRC =
  'globalThis.marked = { Renderer: function() { return {}; }, use() {}, parse: (s) => `<p>${s}</p>` };';

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
