const assert = require('node:assert/strict');
const test = require('node:test');
const { loadRendererContext } = require('./marked_renderer_harness');

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

// One streaming paint against the real marked + renderer: set the tail the way
// usage.js's paintStreamDraft does, parse, clear.
function paint(context, draft) {
  context.streamPaintTailCode = context.openFenceTail(draft);
  const html = context.marked.parse(context.fixNestedFences(draft));
  context.streamPaintTailCode = null;
  return html;
}

test('openFenceTail reports the block still growing at EOF and null when every fence closed', async () => {
  const context = await loadRendererContext();
  assert.equal(context.openFenceTail('prose\n\n```js\nconst x = 1;'), 'const x = 1;');
  assert.equal(context.openFenceTail('a\n```\nb\n```\nc'), null);
  // A longer bare fence closes a shorter one (CommonMark), so nothing is open here.
  assert.equal(context.openFenceTail('```\na\n````\nb'), null);
  // When nested fences are both unclosed, marked's unterminated block starts at
  // the earliest one and the inner fence line is part of its content.
  assert.equal(context.openFenceTail('````\na\n```\nb'), 'a\n```\nb');
  assert.equal(context.openFenceTail('```'), '');
  assert.equal(context.openFenceTail('no fences at all'), null);
});

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

test('the committed path ignores the tail and always highlights', async () => {
  const { calls, hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  context.streamPaintTailCode = null;
  context.marked.parse(context.fixNestedFences('```js\nconst a = 1;\n'));
  assert.equal(calls.lang, 1);
});

test('a completed block keeps its highlight while a same-content tail grows', async () => {
  const { calls, hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  // Paint 1: the js block closes inside this draft; the bare tail shares its
  // content and misses the cache, so both render plain this paint.
  paint(context, '```js\nSHARED\n```\n\nprose\n\n```\nSHARED');
  assert.equal(calls.lang, 0);
  // Paint 2: the tail grew, so the js block no longer matches it — it
  // highlights once and the cache serves it from here on.
  paint(context, '```js\nSHARED\n```\n\nprose\n\n```\nSHARED\nmore');
  assert.equal(calls.lang, 1);
  // Paint 3: the tail closed; the js block hits the cache, the bare tail
  // highlights once via highlightAuto, and no entry point re-runs.
  const final = paint(context, '```js\nSHARED\n```\n\nprose\n\n```\nSHARED\nmore\n```\n');
  assert.equal(calls.lang, 1);
  assert.equal(calls.auto, 1);
  assert.match(final, /HL\[LANG\]SHARED/);
  assert.match(final, /HL\[AUTO\]SHARED\nmore/);
});

test('the tail comparison matches marked unterminated token text across the trailing newline', async () => {
  const { calls, hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  // openFenceTail keeps the trailing newline ("x = 1\n"); marked's
  // unterminated token text strips it ("x = 1") — the skip must still match.
  paint(context, '```py\nx = 1\n');
  assert.equal(calls.lang, 0);
  assert.doesNotMatch(paint(context, '```py\nx = 1\n'), /HL\[LANG\]/);
});

test('fixNestedFences still upgrades an outer fence that inner fences would close', async () => {
  const { hljs } = countingHljs();
  const context = await loadRendererContext(hljs);
  // The outer ``` fence holds a ```js fence; without the delimiter upgrade the
  // first bare close would split the draft into two blocks.
  const nested = '```\na\n```js\nb\n```\nc\n```';
  const html = context.marked.parse(context.fixNestedFences(nested));
  assert.match(html, /HL\[AUTO\]/);
  assert.equal(html.split('code-block').length - 1, 1, 'the nested draft split into two blocks');
});
