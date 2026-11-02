// ---------------------------------------------------------------------------
// Child report card fold (renderMessage's child_report branch): a report past
// three lines, past a weight of 360 (a wide code point counts 2), or with
// result refs folds behind a button. The full text stays in the markup, and
// the button flips data-folded on the card. Fixtures are synthetic text.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');
const vm = require('node:vm');

const { loadChatRenderingModules, makeChatRenderContext } = require('./chat_rendering_context_stub');
const { FakeElement, FakeText, loadChatContext } = require('./turn_engine_harness');

function loadContext() {
  const context = makeChatRenderContext();
  vm.createContext(context);
  loadChatRenderingModules(context);
  return context;
}

function childReport(overrides) {
  return {
    role: 'child_report',
    child_session_id: 'child-1',
    outcome: 'completed',
    content: 'Child finished the task.',
    result_refs: [],
    ...overrides,
  };
}

const FOUR_LINES = 'first line\nsecond line\nthird line\nfourth line';
const ONE_LINE_361 = 'x'.repeat(361);
const SHORT = 'Short report.';
const THREE_LINES_360 = ['a'.repeat(119), 'b'.repeat(120), 'c'.repeat(119)].join('\n');
const HAN = '中';

// A folded report as a turn: the card's report body is its .whitespace-pre-wrap
// node, the element the turn fold row reads its first line from, and a
// separator closes the turn.
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

// A click runs the button's inline onclick with the button as `this`. The card
// and the button are fakes that hold only what that handler reads.
function fakeCardAndButton(html) {
  const card = {dataset: {folded: /data-folded="(true|false)"/.exec(html)[1]}};
  const button = {
    attrs: {'aria-expanded': 'false'},
    closest: (selector) => (selector === '.child-report' ? card : null),
    setAttribute(name, value) { this.attrs[name] = value; },
  };
  return {card, button};
}

function click(context, html, button) {
  const onclick = /<button[^>]*onclick="([^"]*)"/.exec(html)[1];
  vm.runInContext(`(function () {${onclick}})`, context).call(button);
}

test('a report past three lines and a 361-character report fold with their full text', () => {
  const ctx = loadContext();
  for (const content of [FOUR_LINES, ONE_LINE_361]) {
    const html = ctx.renderMessage(childReport({content}), 'sess-1');
    assert.match(html, /data-folded="true"/);
    assert.match(html, /class="child-report-clamp whitespace-pre-wrap break-words"/);
    assert.ok(html.includes(content), 'full text stays in the markup');
    assert.match(html, /class="child-report-toggle"/);
  }
});

test('a short report with refs folds, keeps its refs in the markup, and does not clamp', () => {
  const ctx = loadContext();
  const html = ctx.renderMessage(childReport({content: SHORT, result_refs: ['run:1', 'run:2']}), 'sess-1');
  assert.match(html, /data-folded="true"/);
  assert.doesNotMatch(html, /child-report-clamp/);
  assert.ok(html.includes('run:1') && html.includes('run:2'));
});

test('the button label names the full report and the refs', () => {
  const ctx = loadContext();
  const cases = [
    [FOUR_LINES, [], 'Show full report ▾'],
    [FOUR_LINES, ['run:1'], 'Show full report · 1 ref ▾'],
    [FOUR_LINES, ['run:1', 'run:2'], 'Show full report · 2 refs ▾'],
    [SHORT, ['run:1'], 'Show 1 ref ▾'],
    [SHORT, ['run:1', 'run:2'], 'Show 2 refs ▾'],
  ];
  for (const [content, result_refs, label] of cases) {
    const html = ctx.renderMessage(childReport({content, result_refs}), 'sess-1');
    assert.ok(html.includes(label), `label "${label}" in card`);
    assert.ok(html.includes('Show less ▴'), 'expanded label in card');
  }
});

test('wide code points weigh 2: 181 Han characters fold and 180 with no refs do not', () => {
  const ctx = loadContext();
  // The stub's function is markdown-renderer.js's own: Han is wide, ASCII is not.
  assert.equal(ctx.wc2chIsWide(0x4e2d), true);
  assert.equal(ctx.wc2chIsWide(0x61), false);
  const folded = ctx.renderMessage(childReport({content: HAN.repeat(181)}), 'sess-1');
  assert.match(folded, /data-folded="true"/);
  const flat = ctx.renderMessage(childReport({content: HAN.repeat(180)}), 'sess-1');
  assert.doesNotMatch(flat, /data-folded/);
  assert.doesNotMatch(flat, /<button/);
});

test('a report of three lines and at most 360 characters with no refs renders as before', () => {
  const ctx = loadContext();
  const html = ctx.renderMessage(childReport({content: THREE_LINES_360}), 'sess-1');
  assert.doesNotMatch(html, /data-folded/);
  assert.doesNotMatch(html, /<button/);
  assert.doesNotMatch(html, /child-report-clamp/);
  assert.ok(html.includes(THREE_LINES_360));
});

test('a click expands a folded card, and a second click folds it again', () => {
  const ctx = loadContext();
  const html = ctx.renderMessage(childReport({content: FOUR_LINES, result_refs: ['run:1']}), 'sess-1');
  assert.match(html, /aria-expanded="false"/);
  const {card, button} = fakeCardAndButton(html);
  click(ctx, html, button);
  assert.equal(card.dataset.folded, 'false');
  assert.equal(button.attrs['aria-expanded'], 'true');
  click(ctx, html, button);
  assert.equal(card.dataset.folded, 'true');
  assert.equal(button.attrs['aria-expanded'], 'false');
});

test('the turn fold row of a folded report keeps the report first line', () => {
  const root = foldedReportTurn(FOUR_LINES);
  const row = root.querySelector('.turn-row-title');
  assert.equal(row.textContent, 'first line');
});
