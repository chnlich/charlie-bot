'use strict';
const assert = require('node:assert/strict');
const test = require('node:test');
const { loadRenderer } = require('./marked_renderer_harness');

// The chat math extension (web/static/js/markdown-renderer.js) passes the four
// KaTeX delimiter classes through marked as whole tokens so the formula bytes
// survive to the DOM text node, where renderChatMath's auto-render walk does
// the actual rendering. Cases 01-11 are ported from the session probe
// (~/scripts/20260911_math_ext_probe/probe_math_extension.js); the rest pin the
// bracket classes and the dollar/bracket skip guards. The same case list runs
// against the wrap pre-render driver in tests/core/test_artifact_wrap.py, so
// the chat and wrap scanners stay behavior-identical.

// The harness loads the page's real marked build from the CDN (the same URL the
// index page loads) with markdown-renderer.js registered on it, so these parses
// exercise the served marked plus the repo's renderer and math extension.
async function parse() {
  const marked = await loadRenderer();
  return (src) => marked.parse(src, { async: false }).trim();
}

test('the session failure formulas survive marked verbatim', async () => {
  const md = await parse();
  assert.equal(
      md('$$\\text{Output} = \\text{out\\_routed} + \\text{out\\_shared} + x$$'),
      '<p>$$\\text{Output} = \\text{out\\_routed} + \\text{out\\_shared} + x$$</p>');
  assert.equal(
      md('$$\\text{logits}_{\\text{token}} = x \\cdot W_{\\text{token}}^T \\in [N, 32]$$'),
      '<p>$$\\text{logits}_{\\text{token}} = x \\cdot W_{\\text{token}}^T \\in [N, 32]$$</p>');
});

test('inline math spans survive with subscripts and CJK prose', async () => {
  const md = await parse();
  assert.equal(md('$S_{\\text{local}} = 27,334$ and $B=2$'),
      '<p>$S_{\\text{local}} = 27,334$ and $B=2$</p>');
  assert.equal(md('**注意 $B=2$ 的边界**'), '<p><strong>注意 $B=2$ 的边界</strong></p>');
});

test('dollar amounts stay literal', async () => {
  const md = await parse();
  assert.equal(md('costs $5 and $10 today'), '<p>costs $5 and $10 today</p>');
  assert.equal(md('price $5 later'), '<p>price $5 later</p>');
  assert.equal(md('between $5 and$10 total'), '<p>between $5 and$10 total</p>');
});

test('escaped dollars never open or close a span', async () => {
  const md = await parse();
  assert.equal(md('price \\$5 and \\$10'), '<p>price $5 and $10</p>');
  assert.equal(md('$5 + \\$3$ total'), '<p>$5 + \\$3$ total</p>');
});

test('inline code spans and code fences keep priority', async () => {
  const md = await parse();
  assert.equal(
      md('the op `$S_{\\text{local}}$` stays code'),
      '<p>the op <code>$S_{\\text{local}}$</code> stays code</p>');
  const fenced = md('before\n```\n$x < y$\n```\nafter');
  assert.ok(fenced.includes('<pre><code'), fenced);
  assert.ok(fenced.includes('$x < y$'), fenced);
  // The math token never starts inside the fence, and the fence opener's
  // backtick is consumed by the code tokenizer, not the extension.
  assert.ok(fenced.startsWith('<p>before</p>'), fenced);
});

test('link, bold, and list constructs keep rendering around math spans', async () => {
  const md = await parse();
  assert.equal(
      md('see [the $x$ variant](https://example.com)'),
      '<p>see <a href="https://example.com" target="_blank" rel="noopener noreferrer">the $x$ variant</a></p>');
  assert.equal(md('**bold** then $x^2$ end'), '<p><strong>bold</strong> then $x^2$ end</p>');
  assert.equal(
      md('- 求 $x^2$ 的最小值\n- 然后 $$y = \\sum_{k=1}^8 w_k x_k$$ 结束'),
      '<ul>\n<li>求 $x^2$ 的最小值</li>\n<li>然后 $$y = \\sum_{k=1}^8 w_k x_k$$ 结束</li>\n</ul>');
});

test('angle and ampersand inside math are HTML-escaped, not dropped', async () => {
  const md = await parse();
  assert.equal(md('$x < y$ and $a & b$'), '<p>$x &lt; y$ and $a &amp; b$</p>');
});

test('bracket delimiter classes pass through whole', async () => {
  const md = await parse();
  assert.equal(
      md('\\[\\text{logits}_{\\text{token}} = x \\cdot W_{\\text{token}}^T \\in [N, 32]\\]'),
      '<p>\\[\\text{logits}_{\\text{token}} = x \\cdot W_{\\text{token}}^T \\in [N, 32]\\]</p>');
  assert.equal(md('value \\(x^2\\) here'), '<p>value \\(x^2\\) here</p>');
  assert.equal(md('\\[\n\\text{logits} = Wx\n\\]'), '<p>\\[\n\\text{logits} = Wx\n\\]</p>');
});

test('an unterminated delimiter yields no token and keeps marked core behavior', async () => {
  const md = await parse();
  assert.equal(md('open \\[x\\ never closed'), '<p>open [x\\ never closed</p>');
  assert.equal(md('open \\(x\\ never closed'), '<p>open (x\\ never closed</p>');
});

test('a close delimiter followed by a like character does not close early', async () => {
  const md = await parse();
  // A \] immediately followed by ']' is not a close (display math with [N, 32]
  // style trailing brackets): the span declines and the prose stays literal.
  assert.equal(md('\\[\\text{x}\\]] trailing'), '<p>[\\text{x}]] trailing</p>');
});
