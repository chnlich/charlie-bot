// ---------------------------------------------------------------------------
// A chat URL is a maximal printable-ASCII run [\x21-\x7E]+; any glued-on CJK
// character, full-width mark, curly quote, ellipsis or emoji is prose. These
// tests assert the mechanism, not the incident: over a glue corpus (the
// full-width punctuation family, a glued ideograph, curly quote U+201D, CJK
// ellipsis U+2026, an astral emoji, and the screenshot sentence as one worked
// example) (a) every rendered href is plain printable ASCII with the cut-off
// tail following the anchor as text, (b) the pathname the markdown render
// produces equals the pathname the artifacts.js scanner extracts from the same
// raw text, and (c) the ASCII control battery tokenizes byte-identically to
// stock marked. See chat/artifacts.js FILE_SERVER_LINK_SOURCE for the scanner
// side of the same boundary, and chat_file_link_prefixes.test.js for probe
// cleanliness against it.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadRenderer, loadStockMarked } = require('./marked_renderer_harness');

const {
  FakeText,
  makeMessage,
  loadArtifactsScript,
  render,
  statusResponder,
} = require('./file_link_dom_stub');

const SCHEME_URL = 'https://host.example/absolute_filepath/tmp/run-17/debug_x_v1.html';
const SCHEME_PATHNAME = '/absolute_filepath/tmp/run-17/debug_x_v1.html';

// One tail per glue family; the last is the screenshot sentence as one worked
// example. ASCII trails (a sentence-final period, an unbalanced paren) are the
// stock backpedal's business and belong to the ASCII battery below.
const GLUE_TAILS = [
  { name: 'full-width period', tail: '。' },
  { name: 'ideographic comma', tail: '、' },
  { name: 'full-width comma', tail: '，' },
  { name: 'full-width semicolon', tail: '；' },
  { name: 'full-width colon', tail: '：' },
  { name: 'full-width parens with prose', tail: '（括注）' },
  { name: 'corner brackets', tail: '「引用」' },
  { name: 'bare ideograph', tail: '是' },
  { name: 'curly quote U+201D', tail: '”' },
  { name: 'CJK ellipsis U+2026', tail: '…' },
  { name: 'astral emoji (surrogate pair)', tail: '\uD83D\uDE80' },
  { name: 'the screenshot sentence', tail: '（第 4 节新增）。' },
];

function messageWith(tail) {
  return '产物见 ' + SCHEME_URL + tail + '之后还有别的句子。';
}

// Token trees for comparison with stock: drop the `escaped` bookkeeping field
// (stock inlineText sets it, the del override's lone-`~` text token omits it)
// and merge adjacent text-token runs (the same del override splits a lone `~`
// off into its own text token where stock swallows it into one run; the merged
// tree renders identically and carries the tokens this fix is about).
function normalizeToken(token) {
  const clone = {};
  for (const key of Object.keys(token)) {
    const value = token[key];
    if (key === 'escaped') continue;
    clone[key] = Array.isArray(value) ? normalizeInline(value) : value;
  }
  return clone;
}

function normalizeInline(tokens) {
  const out = [];
  for (const raw of tokens) {
    const token = normalizeToken(raw);
    const prev = out[out.length - 1];
    if (prev && prev.type === 'text' && token.type === 'text') {
      prev.raw += token.raw;
      prev.text += token.text;
    } else {
      out.push(token);
    }
  }
  return out;
}

test('a glued CJK tail cuts the bare URL at the first non-ASCII character', async () => {
  const marked = await loadRenderer();
  for (const { name, tail } of GLUE_TAILS) {
    const input = messageWith(tail);
    const html = marked.parse(input);
    const anchors = html.match(/<a href="[^"]*"/g) || [];
    assert.equal(anchors.length, 1, `expected one link for ${name}: ${html}`);
    const href = anchors[0].slice('<a href="'.length, -1);
    assert.match(href, /^[\x21-\x7E]*$/, `the href carries glued prose for ${name}`);
    assert.equal(href, SCHEME_URL, `the cut does not land exactly at the glue for ${name}`);
    assert.ok(
      html.indexOf('>' + SCHEME_URL + '</a>') !== -1,
      `the link text is not the cut URL for ${name}: ${html}`);
    const after = html.slice(html.indexOf('</a>') + '</a>'.length);
    assert.ok(after.startsWith(tail), `the cut-off tail does not follow the anchor for ${name}: ${html}`);
  }
});

test('the markdown render and the artifacts scanner extract the same pathname', async () => {
  const marked = await loadRenderer();
  for (const { name, tail } of GLUE_TAILS) {
    const input = messageWith(tail);
    const html = marked.parse(input);
    const hrefMatch = html.match(/<a href="([^"]*)"/);
    assert.ok(hrefMatch, `the render produced no link for ${name}: ${html}`);
    const renderPathname = new URL(hrefMatch[1]).pathname;

    const { context, requests } = loadArtifactsScript({
      respond: statusResponder({ [SCHEME_PATHNAME]: 200 }),
    });
    const { root } = makeMessage([new FakeText(input)]);
    await render(context, root);

    assert.equal(requests.length, 1, `expected one probe for ${name}`);
    assert.match(requests[0].url, /^[\x21-\x7E]*$/, `the probe URL carries glued prose for ${name}`);
    const scanPathname = new URL(requests[0].url).pathname;
    assert.equal(scanPathname, renderPathname, `the two layers disagree on the boundary for ${name}`);
  }
});

test('ASCII message shapes tokenize byte-identically to stock marked', async () => {
  const patched = await loadRenderer();
  const stock = await loadStockMarked();
  const cases = [
    'Balanced parens stay whole: https://en.wikipedia.org/wiki/Perl_(language) explains them.',
    'A sentence-final period stays prose: it renders at https://example.com/a/path.html.',
    'Email autolinks: write to some.user+label@example-mail.com for access.',
    'A www. prefix autolinks: www.example.com/nested/path?q=1 is the original.',
    'Explicit [debug page](https://example.com/debug_x_v1.html) next to bare https://example.com/other.',
    'A lone tilde stays literal: roughly ~89 ms today, no pair.',
  ];
  for (const input of cases) {
    assert.deepEqual(
      normalizeInline(patched.lexer(input)),
      normalizeInline(stock.lexer(input)),
      `tokenizer divergence from stock marked on: ${input}`);
  }
  // The control is not vacuous: where the del override intentionally changes
  // behavior (two lone tildes cross-pair into <del> in stock), the trees differ.
  const divergence = '耗时~89ms…共~19个';
  assert.notDeepEqual(
    normalizeInline(patched.lexer(divergence)),
    normalizeInline(stock.lexer(divergence)),
    'control: the patched tokenizer must differ where the overrides intentionally differ');
});
