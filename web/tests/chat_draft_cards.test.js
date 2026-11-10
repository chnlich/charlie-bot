// ---------------------------------------------------------------------------
// The draft card: a message or publication in the operator's name that the
// operator sends by clicking the card's button. The card shows the target and
// the whole body — no fold and no truncation — and a sent, superseded or
// stale card loses its button. A failed card keeps it, and an `unknown`
// failure tells the operator to check the target before resending.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const {loadChatRendering} = require('./chat_rendering_context_stub');
const {readPackageStatic} = require('./read_static');

const LOADER = path.join(__dirname, '..', 'templates', 'drafts', 'draft-cards.js');

function loadDraftCards() {
  const context = loadChatRendering();
  vm.runInContext(readPackageStatic('features', 'drafts', 'draft-cards.js'), context, {filename: 'drafts/draft-cards.js'});
  return context;
}

function longBody() {
  let body = '';
  for (let index = 0; index < 400; index += 1) body += 'line ' + index + ' of the approved message\n';
  return body;
}

function draftMessage(body, replaces) {
  return {
    role: 'draft',
    draft_id: 'd7',
    platform: 'slack',
    target: {channel: 'C1', thread: null},
    target_label: 'Slack channel C1',
    body,
    replaces: replaces === undefined ? null : replaces,
    sha256: 'a'.repeat(64),
    created_at: '2026-11-02T00:00:00Z',
  };
}

function fakeCard() {
  const line = {innerHTML: ''};
  const button = {
    removed: false,
    remove() {
      this.removed = true;
    },
  };
  return {
    dataset: {},
    line,
    button,
    querySelector(selector) {
      if (selector === '.draft-state-line') return line;
      if (selector === '.draft-send') return button;
      return null;
    },
  };
}

// A rendered root holding the outcome markers of one batch: `querySelector` finds
// the cards it knows, like the document the live path searches.
function fakeRoot({replaces = [], sent = [], failed = [], cards = {}} = {}) {
  const marker = (attributes) => ({
    getAttribute(name) {
      return name in attributes ? attributes[name] : null;
    },
  });
  return {
    querySelectorAll(selector) {
      if (selector === '[data-draft-replaces]') return replaces.map((id) => marker({'data-draft-replaces': id}));
      if (selector === '[data-draft-sent]') {
        return sent.map((entry) => marker({
          'data-draft-sent': entry.draft_id,
          'data-sender': entry.sender,
          'data-url': entry.url,
        }));
      }
      if (selector === '[data-draft-failed]') {
        return failed.map((entry) => marker({
          'data-draft-failed': entry.draft_id,
          'data-error': entry.error,
          'data-outcome': entry.outcome,
        }));
      }
      return [];
    },
    querySelector(selector) {
      const match = /data-draft-id="([^"]+)"/.exec(selector);
      return match ? cards[match[1]] || null : null;
    },
  };
}

test('a draft card shows the target and the whole body with one send button', () => {
  const context = loadDraftCards();
  const body = longBody();

  const html = context.renderMessage(draftMessage(body), 'sess-1');

  assert.ok(html.includes('data-message-role="draft"'), 'the card is a rendered message');
  assert.ok(html.includes('data-draft-id="d7"'));
  assert.ok(html.includes('To: Slack channel C1'), 'the target shows');
  assert.ok(html.includes(context.escapeHtml(body)), 'the whole body reaches the card');
  assert.ok(html.includes('whitespace-pre-wrap'), 'the body keeps its line breaks');
  assert.ok(!html.includes('truncate'), 'nothing cuts the body to one line');
  assert.ok(!html.includes('max-h-'), 'nothing folds the body');
  assert.ok(!html.includes('line-clamp'), 'nothing clamps the body');
  assert.ok(html.includes('>Send as me</button>'), 'one button sends the card');
  assert.equal((html.match(/draft-send/g) || []).length, 1, 'one button only');
});

test('a body with markup characters renders as text', () => {
  const context = loadDraftCards();

  const html = context.renderMessage(draftMessage('<b>not markup</b> & more'), 'sess-1');

  assert.ok(html.includes('&lt;b&gt;not markup&lt;/b&gt; &amp; more'));
  assert.ok(!html.includes('<b>not markup</b>'));
});

test('a replaced draft loses its button', () => {
  const context = loadDraftCards();
  const card = fakeCard();
  const root = fakeRoot({replaces: ['d7'], cards: {d7: card}});

  context.Chat.applyDraftUpdates(root);

  assert.equal(card.dataset.draftStatus, 'superseded');
  assert.equal(card.button.removed, true, 'a superseded card has no button');
  assert.ok(card.line.innerHTML.includes('Superseded by a newer draft'));
});

test('a sent draft loses its button and shows the sender and link', () => {
  const context = loadDraftCards();
  const card = fakeCard();
  const root = fakeRoot({
    sent: [{draft_id: 'd7', sender: 'U0SYNTHETIC', url: 'https://chat.example/archives/C1/p1'}],
    cards: {d7: card},
  });

  context.Chat.applyDraftUpdates(root);

  assert.equal(card.dataset.draftStatus, 'sent');
  assert.equal(card.button.removed, true);
  assert.ok(card.line.innerHTML.includes('Sent by U0SYNTHETIC'));
  assert.ok(card.line.innerHTML.includes('href="https://chat.example/archives/C1/p1"'));
});

test('an unknown failure keeps the button and tells the operator to check the target', () => {
  const context = loadDraftCards();
  const card = fakeCard();
  const root = fakeRoot({
    failed: [{draft_id: 'd7', error: 'request timed out', outcome: 'unknown'}],
    cards: {d7: card},
  });

  context.Chat.applyDraftUpdates(root);

  assert.equal(card.dataset.draftStatus, 'failed');
  assert.equal(card.button.removed, false, 'a failed draft stays sendable');
  assert.ok(card.line.innerHTML.includes('request timed out'));
  assert.ok(card.line.innerHTML.includes('Check the target before resending.'));
});

test('a rejection keeps the button without the resend warning', () => {
  const context = loadDraftCards();
  const card = fakeCard();
  const root = fakeRoot({failed: [{draft_id: 'd7', error: 'missing_scope', outcome: 'rejected'}], cards: {d7: card}});

  context.Chat.applyDraftUpdates(root);

  assert.equal(card.button.removed, false);
  assert.ok(card.line.innerHTML.includes('missing_scope'));
  assert.ok(!card.line.innerHTML.includes('Check the target before resending.'));
});

test('the outcome lines carry the marker the card update reads', () => {
  const context = loadDraftCards();

  const sent = context.renderMessage(
      {role: 'draft_sent', draft_id: 'd7', sender: 'U0SYNTHETIC', url: 'https://chat.example/p1'}, 'sess-1');
  const failed = context.renderMessage(
      {role: 'draft_failed', draft_id: 'd7', error: 'missing_scope', outcome: 'rejected'}, 'sess-1');

  assert.ok(sent.includes('data-draft-sent="d7"'));
  assert.ok(sent.includes('data-sender="U0SYNTHETIC"'));
  assert.ok(failed.includes('data-draft-failed="d7"'));
  assert.ok(failed.includes('data-outcome="rejected"'));
});

test('the page part loads the module on the first draft and fills the placeholder in place', () => {
  const context = loadChatRendering();
  const loads = [];
  context.loadPageScripts = (key, sources) => {
    loads.push({key, sources});
    return Promise.resolve();
  };
  vm.runInContext(fs.readFileSync(LOADER, 'utf8'), context, {filename: 'drafts/draft-cards.js'});

  const placeholder = context.renderMessage(draftMessage('first body'), 'sess-1');
  assert.match(placeholder, /id="draft-pending-0"/, 'a draft message renders the placeholder');
  assert.equal(loads.length, 1, 'the first draft starts the download');
  assert.equal(loads[0].sources.length, 1, 'the download asks for the one module');
  assert.equal(loads[0].sources[0], '/static/features/drafts/draft-cards.js');

  context.renderMessage(draftMessage('second body'), 'sess-1');
  assert.equal(loads.length, 1, 'a later draft joins the same download');

  const nodes = new Map();
  context.document.getElementById = (id) => {
    if (!nodes.has(id)) nodes.set(id, {outerHTML: ''});
    return nodes.get(id);
  };
  context.document.querySelectorAll = () => [];
  vm.runInContext(readPackageStatic('features', 'drafts', 'draft-cards.js'), context, {filename: 'drafts/draft-cards.js'});

  assert.match(nodes.get('draft-pending-0').outerHTML, /data-draft-id="d7"/, 'the first placeholder holds a card');
  assert.match(nodes.get('draft-pending-1').outerHTML, /data-draft-id="d7"/, 'the second placeholder holds a card');
  const after = context.renderMessage(draftMessage('third body'), 'sess-1');
  assert.match(after, /data-draft-id="d7"/, 'a later draft renders through the module at once');
});
