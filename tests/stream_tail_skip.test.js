const assert = require('node:assert/strict');
const test = require('node:test');
const { loadRendererContext, MARKED_URL } = require('./marked_renderer_harness');

// hljs stand-in whose output visibly marks the path taken: HL[AUTO]/HL[LANG]
// prefix the input, so a skipped block (escaped-plain) and a highlighted block
// are distinguishable in the emitted HTML, and the counters pin how often each
// entry point ran.
function countingHljs() {
  const calls = { auto: 0, lang: 0 };
  return {
    calls,
    hljs: {
      getLanguage: (lang) => (lang === 'js' || lang === 'py' ? { name: lang } : null),
      highlightAuto: (code) => { calls.auto += 1; return { value: 'HL[AUTO]' + code }; },
      highlight: (code) => { calls.lang += 1; return { value: 'HL[LANG]' + code }; },
    },
  };
}

// One streaming paint against the real marked + renderer, exactly the shape
// usage.js's paintStreamDraft drives: the recorder filled by parseStreamDraft's
// own token walk, cleared after.
function paint(context, draft) {
  context.streamPaintCodeTokens = [];
  const html = context.parseStreamDraft(context.fixNestedFences(draft));
  context.streamPaintCodeTokens = null;
  return html;
}

test('the growing tail paints escaped-plain and highlights once the fence closes', async () => {
  const { calls, hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  const growing = paint(context, '```js\nconst a = 1;\n');
  assert.equal(calls.lang, 0, 'the growing block re-ran highlight');
  assert.match(growing, /const a = 1;/);
  assert.doesNotMatch(growing, /HL\[LANG\]/);

  const closed = paint(context, '```js\nconst a = 1;\n```\n');
  assert.equal(calls.lang, 1, 'the closed block did not highlight exactly once');
  assert.match(closed, /HL\[LANG\]const a = 1;/);

  paint(context, '```js\nconst a = 1;\n```\n');
  assert.equal(calls.lang, 1, 'the closed block missed the cache');
});

test('a completed last block keeps its highlight: the closing-fence raw test gates the skip', async () => {
  const { calls, hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  // The draft ends on a completed block, the shape the final-frame parity
  // contract pins: the last paint must carry today's highlighted bytes.
  const html = paint(context, '```js\nconst a = 1;\n```\nprose tail');
  assert.equal(calls.lang, 1);
  assert.match(html, /HL\[LANG\]/);
});

test('the committed path never skips', async () => {
  const { calls, hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  context.streamPaintCodeTokens = null;
  context.marked.parse(context.fixNestedFences('```js\nconst a = 1;\n'));
  assert.equal(calls.lang, 1);
});

test('a completed block keeps its highlight while a later tail grows', async () => {
  const { calls, hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  // Paint 1: the js block closes inside this draft, so it is not the last
  // code token — it highlights while the bare tail after it skips.
  paint(context, '```js\nSHARED\n```\n\nprose\n\n```\nSHARED');
  assert.equal(calls.lang, 1);
  assert.equal(calls.auto, 0);
  // Paint 2: the tail grew; the js block hits the cache and the tail keeps
  // skipping — no entry point re-runs.
  paint(context, '```js\nSHARED\n```\n\nprose\n\n```\nSHARED\nmore');
  assert.equal(calls.lang, 1);
  assert.equal(calls.auto, 0);
  // Paint 3: the tail closed; the js block hits the cache, the bare tail
  // highlights once via highlightAuto.
  const final = paint(context, '```js\nSHARED\n```\n\nprose\n\n```\nSHARED\nmore\n```\n');
  assert.equal(calls.lang, 1);
  assert.equal(calls.auto, 1);
  assert.match(final, /HL\[LANG\]SHARED/);
  assert.match(final, /HL\[AUTO\]SHARED\nmore/);
});

test('the last code token skips by identity whatever its shape: indented, upgraded, list-nested', async () => {
  const { calls, hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  // An indented fence inside a list item: marked strips the list indent
  // before the fence rule sees it, so no line-level model can predict the
  // token text — identity does not have to.
  paint(context, '- item\n\n   ```py\n   x = 1\n   y = 2');
  assert.equal(calls.lang, 0);
  // A delimiter upgrade inside the still-open outer fence: the outer pair
  // closes (its completed block highlights once), the inner pair's delimiters
  // rewrite to five backticks, and the still-growing inner block skips.
  const upgraded = paint(context, '````\n```js\n```` x\n````\n```\nz');
  assert.equal(calls.lang, 0);
  assert.equal(calls.auto, 1, 'the closed outer block did not highlight exactly once');
  assert.match(upgraded, /HL\[AUTO\]/);
  assert.match(upgraded, /<code class="hljs">z<\/code>/, 'the growing tail rendered highlighted');
});

test('a longer bare fence inside a growing block is content, not its closer', async () => {
  const { calls, hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  // The outer ```` fence stays open; the inner ``` line is its content. The
  // raw's last line is a bare fence, but shorter than the opener — the skip
  // must still fire (this shape misreads as complete and only loses
  // coverage, but the common one-line-content case is worth pinning).
  const html = paint(context, '````markdown\n```js\nx');
  assert.equal(calls.auto, 0);
  assert.match(html, /<code class="hljs">```js\nx<\/code>/);
});

test('a throwing parse clears the recorder so later renders never skip', async () => {
  // End-to-end through the real usage.js paint path: its try/finally owns the
  // recorder window, so a parse that throws mid-render must leave it null.
  const { buildStreamHarness } = require('./stream_render_harness');
  const { fetchUrl } = require('./stream_collector_common');
  const markedSrc = await fetchUrl(MARKED_URL);
  const h = buildStreamHarness(markedSrc, {});
  h.context.hljs.highlightAuto = () => { throw new Error('boom'); };
  // A completed bare block (auto path, throws) followed by an open fence.
  assert.throws(() => h.showStreaming({ content: '```\nsha\n```\n\n```js\nx = 1' }));
  assert.equal(h.context.streamPaintCodeTokens, null, 'the recorder leaked past a throwing parse');

  const calls = { auto: 0, lang: 0 };
  h.context.hljs.highlightAuto = (code) => { calls.auto += 1; return { value: 'HL[AUTO]' + code }; };
  h.context.hljs.highlight = (code) => { calls.lang += 1; return { value: 'HL[LANG]' + code }; };
  h.showStreaming({ content: '```\nsha\n```\n\n```js\nx = 1' });
  h.advance(200); // the throw spent the leading-edge window; the retry rides the trailing flush
  assert.match(h.stats().frames.at(-1), /HL\[AUTO\]sha/, 'the completed block lost its highlight');
  assert.doesNotMatch(h.stats().frames.at(-1), /HL\[LANG\]/, 'the growing tail stopped skipping');
});
