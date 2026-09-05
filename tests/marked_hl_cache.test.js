const assert = require('node:assert/strict');
const test = require('node:test');
const { loadRenderer } = require('./marked_renderer_harness');

// Counting hljs double: every call is recorded, and the emitted value carries a
// call counter so a memo hit (same bytes, no new call) is distinguishable from
// a re-run (new bytes) even when the language grammar is trivial.
function countingHljs() {
  const calls = { highlightAuto: 0, highlight: 0 };
  let n = 0;
  return {
    calls,
    getLanguage: (lang) => (lang === 'python' ? { name: 'python' } : null),
    highlightAuto: (s) => {
      calls.highlightAuto += 1;
      return { value: `auto#${n += 1}:${s}` };
    },
    highlight: (s) => {
      calls.highlight += 1;
      return { value: `lang#${n += 1}:${s}` };
    },
  };
}

test('a repeat paint of an unchanged untagged block serves the memo, not highlightAuto', async () => {
  const hljs = countingHljs();
  const marked = await loadRenderer(hljs);
  const md = 'prose\n\n```\nconst x = 1\n```\n\nmore prose\n';
  const first = marked.parse(md);
  const second = marked.parse(md);
  assert.equal(second, first, 'repeat parse bytes differ');
  assert.equal(hljs.calls.highlightAuto, 1, 'highlightAuto re-ran for an unchanged block');
});

test('a repeat paint of a tagged block serves the memo per (lang, code)', async () => {
  const hljs = countingHljs();
  const marked = await loadRenderer(hljs);
  const md = '```python\nprint(1)\n```\n';
  const first = marked.parse(md);
  assert.equal(marked.parse(md), first);
  assert.equal(hljs.calls.highlight, 1);
  // Same code under a different lang key must not serve the other lang's entry.
  const other = marked.parse('```\nprint(1)\n```\n');
  assert.notEqual(other, first);
  assert.equal(hljs.calls.highlightAuto, 1);
});

test('a growing code block (the streaming tail) re-highlights; completed blocks do not', async () => {
  const hljs = countingHljs();
  const marked = await loadRenderer(hljs);
  const first = marked.parse('done\n\n```\nblock one\n```\n\ntail\n\n```\ngrowing');
  const second = marked.parse('done\n\n```\nblock one\n```\n\ntail\n\n```\ngrowing more');
  assert.equal(hljs.calls.highlightAuto, 3, 'expected block-one + two tail renders');
  assert.match(first, /auto#\d+:block one/);
  assert.match(second, /growing more/);
  // A third paint with no change anywhere re-parses and re-serves both blocks.
  marked.parse('done\n\n```\nblock one\n```\n\ntail\n\n```\ngrowing more');
  assert.equal(hljs.calls.highlightAuto, 3, 'an unchanged draft re-ran highlightAuto');
});

test('the LRU evicts its oldest entry past the cap', async () => {
  const hljs = countingHljs();
  const marked = await loadRenderer(hljs);
  const block = (i) => '```\nblock ' + i + '\n```\n';
  marked.parse(block(0));
  for (let i = 1; i <= 32; i++) marked.parse(block(i));
  const callsAtCap = hljs.calls.highlightAuto;
  assert.equal(callsAtCap, 33, 'every distinct block should have highlighted once');
  marked.parse(block(0)); // evicted: the first entry is the oldest
  assert.equal(hljs.calls.highlightAuto, callsAtCap + 1, 'the evicted block re-ran highlightAuto');
  marked.parse(block(32)); // still resident
  assert.equal(hljs.calls.highlightAuto, callsAtCap + 1, 'a resident block re-ran highlightAuto');
});
