// The Everything turn row's one-line title and conclusion: markdown the field cannot show is
// dropped, so a fenced block reads as its code, a table as its first data row, and emphasis as
// plain words.
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadChatRendering } = require('./chat_rendering_context_stub');

function rowFields(headText, conclusionText) {
  const context = loadChatRendering();
  const row = context.Chat.buildTurnRowFromSpec({
    headRole: 'assistant',
    headKind: null,
    headText,
    conclusionText,
    foldCount: 0,
    headTs: null,
  });
  const field = (className) => row.children.find((child) => child.className === className).textContent;
  return { title: field('turn-row-title'), conclusion: field('turn-row-conclusion') };
}

test('a fenced block reads as its first code line, not the language tag', () => {
  const { title } = rowFields('```typescript\nexport function pageOf() {}\n```', '');
  assert.equal(title, 'export function pageOf() {}');
});

test('a table reads as its first data row, not the header or the delimiter row', () => {
  const { conclusion } = rowFields(
      '',
      '| 项目 | 状态 | 说明 |\n| --- | --- | --- |\n| release checklist | 完成 | Keep it. |\n\nLater prose.');
  assert.equal(conclusion, 'release checklist · 完成 · Keep it.');
});

test('a table with no data row falls through to the next prose line', () => {
  const { conclusion } = rowFields('', '| 项目 | 状态 | 说明 |\n| --- | --- | --- |\n\nthe first prose line');
  assert.equal(conclusion, 'the first prose line');
});

test('emphasis markers are dropped', () => {
  const { conclusion } = rowFields('', '**release checklist**: 这里的锁粒度太粗。');
  assert.equal(conclusion, 'release checklist: 这里的锁粒度太粗。');
});

test('a plain line and the existing heading and link cleanup stay unchanged', () => {
  assert.equal(rowFields('', 'plain words').conclusion, 'plain words');
  assert.equal(rowFields('', '## 标题').conclusion, '标题');
  assert.equal(rowFields('', 'see [the doc](https://example.test/y) now').conclusion, 'see the doc now');
});
