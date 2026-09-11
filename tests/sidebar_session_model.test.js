const assert = require('node:assert/strict');
const test = require('node:test');

const {loadGroups, row} = require('./sidebar_groups_context_stub');

// The model element must be a sibling of .session-time, never inside it:
// updateRelativeTimes() (web/static/js/utils.js) reassigns .session-time's
// textContent on every tick and would erase any model text nested in it.
function sessionTimeInnerHtml(html) {
  const open = html.match(/<span class="session-time[^"]*"[^>]*>/);
  assert.ok(open, 'row is missing a .session-time span');
  const start = html.indexOf(open[0]) + open[0].length;
  const end = html.indexOf('</span>', start);
  assert.ok(end > -1, '.session-time span is unterminated');
  return html.slice(start, end);
}

function modelSpan(html) {
  return html.match(/<span class="session-backend([^"]*)" title="([^"]*)">([^<]*)<\/span>/);
}

test('row shows the backend label without its family prefix, next to the time', () => {
  const html = row({backend: 'claude-opus-5'});
  const model = modelSpan(html);
  assert.ok(model, 'row is missing a .session-backend span');
  assert.equal(model[3], 'Opus 5');
  assert.equal(model[2], 'CC · Opus 5');
  // model text follows the time in document order
  assert.ok(html.indexOf('session-time') < html.indexOf('session-backend'));
});

test('long labels keep the truncate class so the 320px sidebar clips them', () => {
  const model = modelSpan(row({backend: 'codex-gpt-5.3-codex-spark'}));
  assert.equal(model[3], 'GPT-5.3 Codex Spark xHigh (personal)');
  assert.match(model[1], /\btruncate\b/);
});

test('a backend id retired from config renders the raw id', () => {
  const model = modelSpan(row({backend: 'claude-fable-sub'}));
  assert.equal(model[3], 'claude-fable-sub');
  assert.equal(model[2], 'claude-fable-sub');
});

test('a session without a backend renders no model element', () => {
  assert.equal(modelSpan(row({backend: ''})), null);
  assert.equal(modelSpan(row({})), null);
});

test('.session-time holds only the timestamp, so the time refresh cannot erase the model', () => {
  const html = row({backend: 'claude-opus-5'});
  assert.equal(sessionTimeInnerHtml(html), 'Jul 29, 5:12 PM');
});

test('scheduled rows keep their cron lines untouched', () => {
  const html = row({backend: 'claude-opus-5', schedule_cron: '0 9 * * *', schedule_timezone: 'PT'});
  const scheduled = loadGroups().Sidebar.renderSessionItem(
    {id: 's1', name: 'demo', updated_at: '2026-07-29T17:12:00Z', backend: 'claude-opus-5',
     schedule_cron: '0 9 * * *', schedule_timezone: 'PT'},
    'scheduled'
  );
  assert.ok(html.includes('session-backend'), 'non-scheduled row should show the model');
  assert.ok(scheduled.includes('0 9 * * *'));
  assert.equal(modelSpan(scheduled), null);
});

// bumpCurrentSessionToTop() (web/static/js/chat/input.js) locates the PM head
// row purely by this attribute; if the builders drift away from it, the bump
// tests stay green while the bug returns in production.
function rootOpenTag(html) {
  const match = html.match(/^<[a-z]+\b[^>]*>/);
  assert.ok(match, `no root open tag in: ${html}`);
  return match[0];
}

test('both Project Manager row builders mark their root element with data-pm-head="1"', () => {
  const {Sidebar} = loadGroups();
  const pmRow = Sidebar.renderProjectManagerRow(
    {id: 'pm1', name: 'PM · demo', updated_at: '2026-07-29T17:12:00Z'}
  );
  const slotRow = Sidebar.renderProjectManagerSlotRow('demo group');
  assert.ok(rootOpenTag(pmRow).startsWith('<a'), pmRow);
  assert.ok(rootOpenTag(pmRow).includes('data-pm-head="1"'), rootOpenTag(pmRow));
  assert.ok(rootOpenTag(slotRow).startsWith('<button'), slotRow);
  assert.ok(rootOpenTag(slotRow).includes('data-pm-head="1"'), rootOpenTag(slotRow));
});
