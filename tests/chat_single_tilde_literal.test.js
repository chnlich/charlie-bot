const assert = require('node:assert/strict');
const test = require('node:test');

const { loadRenderer } = require('./marked_renderer_harness');

function tildeCount(html) {
  return (html.match(/<del>/g) || []).length;
}

test('two lone tildes in one paragraph render literally with zero <del>', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('用 ~89 ms、共 ~19 个');
  assert.equal(tildeCount(html), 0, `unexpected <del> in: ${html}`);
  assert.match(html, /用 ~89 ms、共 ~19 个/);
  assert.doesNotMatch(html, /<del>/);
});

test('a lone tilde adjacent to prose does not cross-pair into <del>', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('耗时~89ms…共~19个');
  assert.equal(tildeCount(html), 0, `unexpected <del> in: ${html}`);
  assert.match(html, /耗时~89ms…共~19个/);
});

test('~~删除~~ still renders as <del>删除</del>', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('~~删除~~');
  assert.match(html, /<del>删除<\/del>/);
});

test('inline code `cd ~` keeps its tilde with no backslash', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('`cd ~`');
  assert.match(html, /<code>cd ~<\/code>/);
  assert.doesNotMatch(html, /\\\\/);
  assert.doesNotMatch(html, /\\~/);
});

test('fenced code block containing a tilde is byte-identical', async () => {
  const marked = await loadRenderer();
  const html = marked.parse('```\n~x~\n```');
  assert.match(html, />~x~</, 'fence body ~x~ changed or disappeared');
  assert.doesNotMatch(html, /<del>/);
  assert.doesNotMatch(html, /\\~/);
});

test('~~~x~~~ and ~~~~x~~~~ produce no <del>', async () => {
  const marked = await loadRenderer();
  const triple = marked.parse('~~~x~~~');
  const quad = marked.parse('~~~~x~~~~');
  assert.doesNotMatch(triple, /<del>/);
  assert.doesNotMatch(quad, /<del>/);
});
