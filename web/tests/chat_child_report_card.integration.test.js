// ---------------------------------------------------------------------------
// Child report card fold (renderMessage's child_report branch): a report past
// three lines folds behind a button. A report past a weight of 360 (a wide code
// point counts 2) folds too, and so does one with result refs. The full text
// stays in the markup, and the button flips data-folded on the card. Fixtures
// are synthetic text.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const { FakeElement, FakeText, loadChatContext } = require('./turn_engine_harness');

const FOUR_LINES = 'first line\nsecond line\nthird line\nfourth line';
// A folded report as a turn: the card's report body is its .whitespace-pre-wrap
// node, the element the turn fold row reads its first line from. A separator
// closes the turn.
function foldedReportTurn(content) {
  const root = new FakeElement('DIV', {id: 'messages', className: 'space-y-3'});
  const card = new FakeElement('DIV', {className: 'flex justify-start'});
  card.dataset.messageId = 'c1';
  card.dataset.messageRole = 'child_report';
  const bubble = new FakeElement('DIV', {className: 'child-report max-w-[90%]'});
  bubble.dataset.folded = 'true';
  const body = new FakeElement('DIV', {className: 'child-report-clamp whitespace-pre-wrap break-words'});
  body.appendChild(new FakeText(content));
  bubble.appendChild(body);
  card.appendChild(bubble);
  root.appendChild(card);
  const separator = new FakeElement('DIV', {className: 'separator-line group/sep'});
  separator.dataset.messageId = 's1';
  separator.dataset.messageRole = 'separator';
  root.appendChild(separator);
  const {context} = loadChatContext({getElementById: (id) => (id === 'messages' ? root : null)});
  context.applyTurnOutline(root);
  return root;
}

test('the turn fold row of a folded report keeps the report first line', () => {
  const root = foldedReportTurn(FOUR_LINES);
  const row = root.querySelector('.turn-row-title');
  assert.equal(row.textContent, 'first line');
});
