const assert = require('node:assert/strict');
const test = require('node:test');
const { loadRendererContext, MARKED_URL } = require('./marked_renderer_harness');
const { fetchUrl, largestAssistantDraft } = require('./stream_collector_common');

// The streaming paint path, exactly as usage.js's paintStreamDraft drives it.
function paint(context, draft) {
  context.streamPaintCodeTokens = [];
  const html = context.parseStreamDraft(context.fixNestedFences(draft));
  context.streamPaintCodeTokens = null;
  return html;
}

// The pre-incremental streaming render: lex the whole draft, record the code
// tokens, render — the behavior the incremental parse must reproduce byte for
// byte on every paint. (marked.parse differs for a draft ending inside an
// unterminated fence: the streaming paint deliberately renders that block
// escaped-plain, the M54 skip.)
function referenceStreamRender(context, draft) {
  context.streamPaintCodeTokens = [];
  const tokens = context.marked.lexer(context.fixNestedFences(draft));
  context.recordCodeTokens(tokens);
  const html = context.marked.parser(tokens);
  context.streamPaintCodeTokens = null;
  return html;
}

// One streamed draft grown by random appends; every painted frame must equal
// the whole-draft streaming render of that draft. Returns the paints exercised.
function streamedPaints(context, text, rng) {
  let paints = 0;
  let pos = 0;
  while (pos < text.length) {
    pos = Math.min(text.length, pos + 1 + Math.floor(rng() * 160));
    const draft = text.slice(0, pos);
    const html = paint(context, draft);
    const reference = referenceStreamRender(context, draft);
    assert.equal(html, reference, `frame mismatch at ${pos} bytes of a ${text.length} byte draft`);
    paints += 1;
  }
  return paints;
}

// A deterministic PRNG so a failure reproduces from the seed in the message.
function mulberry32(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const BLOCKS = [
  'Prose paragraph with **bold**, `inline code` and a [link](https://example.com/x).\n',
  '## A heading\n',
  '- item one\n- item two\n- item three\n',
  '1. ordered item\n2. second item\n\n3. after a blank\n',
  '```js\nconst a = 1;\n```\n',
  '```\nunterminated fence content\n',
  '> quoted line\n> more quote\n',
  '| a | b |\n| --- | --- |\n| 1 | 2 |\n',
  '---\n',
  '<div class="x">\n<p>raw html block</p>\n</div>\n',
  'Setext heading\n===\n',
  'CJK prose 引数 with — em dash and ✅ emoji.\n',
  'Trailing [ref] use with no definition yet.\n',
  '[ref]: https://example.com/definition\n',
  'Indented continuation\n\n    code-ish indent\n',
  '~~~~\nfence with tildes\n~~~~\n',
];

function randomDraft(rng, blocks) {
  const n = 3 + Math.floor(rng() * 14);
  let out = '';
  for (let i = 0; i < n; i++) out += blocks[Math.floor(rng() * blocks.length)];
  return out;
}

test('random streamed drafts match the full parse on every paint', async () => {
  const context = await loadRendererContext({
    getLanguage: () => null,
    highlightAuto: (code) => ({ value: code }),
    highlight: (code) => ({ value: code }),
  });
  const rng = mulberry32(0x5f3759df);
  let paints = 0;
  for (let i = 0; i < 120; i++) {
    paints += streamedPaints(context, randomDraft(rng, BLOCKS), rng);
  }
  assert.ok(paints > 400, `fuzz exercised only ${paints} paints`);
});

test('a reference definition arriving in the tail re-parses whole from then on', async () => {
  const context = await loadRendererContext({
    getLanguage: () => null,
    highlightAuto: (code) => ({ value: code }),
    highlight: (code) => ({ value: code }),
  });
  // The prefix renders the [ref] use literally; once the definition lands the
  // full parse resolves it — the frames must follow the full parse both ways.
  const steps = ['prefix with [ref] use\n\nmore prose\n\n', '[ref]: https://example.com/d\n', 'tail after definition\n'];
  let draft = '';
  for (const step of steps) {
    draft += step;
    const html = paint(context, draft);
    assert.equal(html, referenceStreamRender(context, draft), `mismatch after adding ${JSON.stringify(step)}`);
  }
});

test('a list continued across a blank line never freezes a mid-list cut', async () => {
  const context = await loadRendererContext({
    getLanguage: () => null,
    highlightAuto: (code) => ({ value: code }),
    highlight: (code) => ({ value: code }),
  });
  // The blank line inside the list cannot become a cut (the space's preceding
  // token is a list), so the tail keeps re-rendering until the list ends.
  const steps = ['- a\n- b\n', '\n  continued across blank\n', '- c\n', '\nprose after the list\n'];
  let draft = '';
  for (const step of steps) {
    draft += step;
    const html = paint(context, draft);
    assert.equal(html, referenceStreamRender(context, draft));
  }
});

test('the largest on-disk draft paints identically on every step of its replay', async () => {
  const context = await loadRendererContext({
    getLanguage: () => null,
    highlightAuto: (code) => ({ value: code }),
    highlight: (code) => ({ value: code }),
  });
  const text = largestAssistantDraft(() => true);
  const paints = streamedPaints(context, text, mulberry32(1234));
  assert.ok(paints > 100, `real-corpus replay painted only ${paints} frames`);
});

test('the frozen prefix actually advances: the incremental path re-lexes a shrinking tail', async () => {
  const context = await loadRendererContext({
    getLanguage: () => null,
    highlightAuto: (code) => ({ value: code }),
    highlight: (code) => ({ value: code }),
  });
  // Drive a draft whose blocks close one after another; after the paints the
  // state's cut must sit far from 0 — otherwise every paint re-lexed whole
  // and this suite would pass vacuously.
  const text = Array.from({ length: 12 }, (_, i) => `Paragraph ${i} with prose.\n`).join('\n');
  for (let pos = 40; pos <= text.length; pos += 40) {
    paint(context, text.slice(0, pos));
  }
  assert.ok(context.streamParseState !== null, 'the stream state was dropped');
  assert.ok(context.streamParseState.cut > text.length / 2, `cut frozen at ${context.streamParseState.cut}`);
});

test('a draft ending on a closed block matches marked.parse on every paint', async () => {
  // The collector's final-frame parity shape: no unterminated fence at the
  // end, so the streaming render and the full parse agree byte for byte.
  const context = await loadRendererContext({
    getLanguage: () => null,
    highlightAuto: (code) => ({ value: code }),
    highlight: (code) => ({ value: code }),
  });
  const text = Array.from({ length: 10 }, (_, i) => `Paragraph ${i} prose.\n\n`).join('') + '```js\nconst a = 1;\n```\n';
  let pos = 30;
  while (pos <= text.length) {
    const draft = text.slice(0, pos);
    const html = paint(context, draft);
    assert.equal(html, context.marked.parse(context.fixNestedFences(draft)), `parity loss at ${pos}`);
    pos += 30;
  }
});

test('a shrinking or rewritten draft resets to a full parse', async () => {
  const context = await loadRendererContext({
    getLanguage: () => null,
    highlightAuto: (code) => ({ value: code }),
    highlight: (code) => ({ value: code }),
  });
  paint(context, 'first draft\n\nsecond paragraph\n');
  assert.ok(context.streamParseState !== null);
  const html = paint(context, 'wholly different\n');
  assert.equal(html, referenceStreamRender(context, 'wholly different\n'));
});
