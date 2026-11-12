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
  return { row, title: field('turn-row-title'), conclusion: field('turn-row-conclusion') };
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

test('inline code keeps its text, so underscores in a file name survive', () => {
  const { conclusion } = rowFields('', 'see `__init__.py` now');
  assert.equal(conclusion, 'see __init__.py now');
});

test('table cells carry their own inline cleanup', () => {
  const { conclusion } = rowFields(
      '', '| 项目 | 状态 | 说明 |\n| --- | --- | --- |\n| **done** | [doc](https://example.test/y) | plain |');
  assert.equal(conclusion, 'done · doc · plain');
});

test('a delimiter-shaped data row after the delimiter row is data', () => {
  const { conclusion } = rowFields('', '| 项目 |\n| --- |\n| - |\n| second |');
  assert.equal(conclusion, '-');
});

test('a data-less table does not swallow the next table\'s header', () => {
  const { conclusion } = rowFields(
      '', '| 项目 |\n| --- |\n\n| x | y |\n| --- | --- |\n| data | row |');
  assert.equal(conclusion, 'data · row');
});

test('a plain line and the existing heading and link cleanup stay unchanged', () => {
  assert.equal(rowFields('', 'plain words').conclusion, 'plain words');
  assert.equal(rowFields('', '## 标题').conclusion, '标题');
  assert.equal(rowFields('', 'see [the doc](https://example.test/y) now').conclusion, 'see the doc now');
});

// The row drops the conclusion column when it has no text, so the title keeps
// that width. The class drives the grid in styles.css.
test('a row marks itself when it has no conclusion for the column to show', () => {
  assert.equal(rowFields('a title', '').row.classList.contains('turn-row-no-conclusion'), true);
  assert.equal(rowFields('a title', null).row.classList.contains('turn-row-no-conclusion'), true);
  assert.equal(rowFields('a title', 'the conclusion').row.classList.contains('turn-row-no-conclusion'), false);
});

// The row is a button, and a phone has no hover to show it: the row ends in
// the decorative caret styles.css draws, so the reader sees it opens.
test('a row ends in the disclosure caret that assistive technology skips', () => {
  const { row } = rowFields('a title', '');
  const caret = row.children[row.children.length - 1];
  assert.equal(caret.className, 'turn-row-caret');
  assert.equal(caret.getAttribute('aria-hidden'), 'true');
});
