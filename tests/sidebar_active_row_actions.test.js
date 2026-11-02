// ---------------------------------------------------------------------------
// The current session's row (groups.js): its action buttons are hover-revealed
// like every other row's, so the name column keeps the row's full width. A
// pinned button would reserve its stack from the name, and the name would
// truncate on the one row the reader is looking at. The starred star is the
// exception: it shows the row's state, so it keeps its flow slot. Harness
// follows sidebar_group_create_button.test.js: the shared groups context stub.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const test = require('node:test');

const {loadGroups} = require('./sidebar_groups_context_stub');

function buildContext() {
  const context = loadGroups();
  const nav = {innerHTML: ''};
  context.document = {getElementById: (id) => (id === 'session-list' ? nav : null)};
  context.updateRelativeTimes = () => {};
  return {context, nav};
}

function sessionRow(id, group) {
  return {id, name: 'Session ' + id, group, updated_at: '2026-07-29T17:12:00Z'};
}

test('the current session row leaves its action buttons hover-revealed, like every other row', () => {
  const {context, nav} = buildContext();
  context.SESSION_ID = 's1';

  context.renderSessionList([sessionRow('s1', 'alpha')], 'all');

  const row = nav.innerHTML.match(/<a href="\/\?session=s1[^"]*"[\s\S]*?<\/a>/);
  assert.ok(row, 'the current session row is missing');
  assert.match(row[0], /bg-blue-600\/20/, 'the current session row keeps its highlight');
  const buttons = row[0].match(/<button[\s\S]*?<\/button>/g) || [];
  assert.ok(buttons.length >= 1, 'the row is missing its action buttons');
  buttons.forEach((button) => {
    assert.match(button, /class="[^"]*opacity-0 group-hover:opacity-100[^"]*"/);
    assert.doesNotMatch(button, /!opacity-100/,
      'a pinned button reserves the row width the session name reads');
  });
});

test('a starred row keeps its star in the flow while its other actions stay hover-revealed', () => {
  const {context, nav} = buildContext();
  context.SESSION_ID = 's1';

  context.renderSessionList([{...sessionRow('s1', 'alpha'), starred: true}], 'all');

  const star = nav.innerHTML.match(/<button[^>]*id="star-s1"[^>]*>/);
  assert.ok(star, 'the starred row is missing its star');
  assert.match(star[0], /!opacity-100/, 'the star shows the starred state at rest');
  const gear = nav.innerHTML.match(/<button[^>]*title="Settings"[^>]*>/);
  assert.ok(gear, 'the row is missing its Settings gear');
  assert.doesNotMatch(gear[0], /!opacity-100/);
});
