// ---------------------------------------------------------------------------
// Status block paragraphs: the master's **Goal** / **Now** / **Waiting on
// you** opener written on single-newline lines must render as one <p> with a
// <br> between consecutive lines, while every other paragraph — the list
// form, the blank-line form, ordinary prose — keeps marked's default bytes.
// These tests run the page's real renderer and marked build (see
// marked_renderer_harness.js) and pin the mechanism: detection on the three
// labels with the first line required to be **Goal**, the fallback for
// paragraphs the rule must not touch (including one whose inline tokens do
// not split cleanly on the line boundaries), and the streaming paint
// agreeing with the completed-message parse.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadStockMarked, loadRendererContext, paint } = require('./marked_renderer_harness');

const BLOCK = '**Goal**: g\n**Now**: n\n**Waiting on you**: w';

function brCount(html) {
  return (html.match(/<br>/g) || []).length;
}

test('single-newline block renders one <p> with two <br> and strong labels', async () => {
  const { marked } = await loadRendererContext();
  assert.equal(
    marked.parse(BLOCK),
    '<p><strong>Goal</strong>: g<br><strong>Now</strong>: n'
      + '<br><strong>Waiting on you</strong>: w</p>\n');
});

test('full-width colons render the same structure', async () => {
  const { marked } = await loadRendererContext();
  const html = marked.parse('**Goal**：g\n**Now**：n\n**Waiting on you**：w');
  assert.equal(
    html,
    '<p><strong>Goal</strong>：g<br><strong>Now</strong>：n'
      + '<br><strong>Waiting on you</strong>：w</p>\n');
});

test('list form keeps one <ul> with three <li> and no <br>', async () => {
  const { marked } = await loadRendererContext();
  const html = marked.parse('- **Goal**: g\n- **Now**: n\n- **Waiting on you**: w');
  assert.equal((html.match(/<ul>/g) || []).length, 1);
  assert.equal((html.match(/<li>/g) || []).length, 3);
  assert.equal(brCount(html), 0);
});

test('blank-line form keeps three <p> and no <br>', async () => {
  const { marked } = await loadRendererContext();
  const html = marked.parse('**Goal**: g\n\n**Now**: n\n\n**Waiting on you**: w');
  assert.equal((html.match(/<p>/g) || []).length, 3);
  assert.equal(brCount(html), 0);
});

test('ordinary multi-line paragraph stays byte-identical to stock marked', async () => {
  const [{ marked }, stock] = await Promise.all([loadRendererContext(), loadStockMarked()]);
  const src = 'first line\nsecond line';
  assert.equal(marked.parse(src), stock.parse(src));
  assert.equal(brCount(marked.parse(src)), 0);
});

test('a paragraph whose first line is **Now** stays byte-identical to stock', async () => {
  const [{ marked }, stock] = await Promise.all([loadRendererContext(), loadStockMarked()]);
  const src = '**Now**: n\n**Waiting on you**: w';
  assert.equal(marked.parse(src), stock.parse(src));
  assert.equal(brCount(marked.parse(src)), 0);
});

test('a body paragraph after the block takes no <br>', async () => {
  const { marked } = await loadRendererContext();
  const html = marked.parse(BLOCK + '\n\nbody line');
  assert.equal(brCount(html), 2, 'only the block\'s two line gaps may break');
  assert.match(html, /^<p><strong>Goal<\/strong>: g<br>/);
  assert.match(html, /<p>body line<\/p>\n$/);
});

test('a non-label line after the block falls back to stock bytes', async () => {
  const [{ marked }, stock] = await Promise.all([loadRendererContext(), loadStockMarked()]);
  const src = BLOCK + '\ntrailing prose';
  assert.equal(marked.parse(src), stock.parse(src));
  assert.equal(brCount(marked.parse(src)), 0);
});

test('a code span crossing a line boundary falls back to stock bytes', async () => {
  const [{ marked }, stock] = await Promise.all([loadRendererContext(), loadStockMarked()]);
  const src = '**Goal**: x `code\n**Now**: y`';
  assert.equal(marked.parse(src), stock.parse(src));
});

test('the streaming paint of the block matches the completed-message parse', async () => {
  const context = await loadRendererContext();
  assert.equal(paint(context, BLOCK), context.marked.parse(BLOCK));
});
