const assert = require('node:assert/strict');
const test = require('node:test');

const {loadGroupsWithCron, row} = require('./sidebar_groups_context_stub');

// The model element must be a sibling of .session-time, never inside it.
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

test('an Auto session row shows the Auto label and its current member', () => {
  const model = modelSpan(row({backend: 'auto', pool_member: 'claude-opus-5'}));
  assert.equal(model[2], 'Auto → CC · Opus 5');
  assert.equal(model[3], 'Auto → CC · Opus 5');
});

test('an Auto session row names the recorded model after the member', () => {
  const model = modelSpan(row({backend: 'auto', pool_member: 'claude-opus-5', native_model: 'claude-opus-5-5'}));
  assert.equal(model[2], 'Auto → CC · Opus 5 · claude-opus-5-5');
  assert.equal(model[3], 'Auto → CC · Opus 5 · claude-opus-5-5');
});

test('an Auto row with no recorded member shows the Auto label alone', () => {
  const model = modelSpan(row({backend: 'auto'}));
  assert.equal(model[2], 'Auto');
  assert.equal(model[3], 'Auto');
});

test('a session without a backend renders no model element', () => {
  assert.equal(modelSpan(row({backend: ''})), null);
  assert.equal(modelSpan(row({})), null);
});

test('.session-time holds only the timestamp, so the time refresh cannot erase the model', () => {
  const html = row({backend: 'claude-opus-5'});
  assert.equal(sessionTimeInnerHtml(html), 'Jul 29, 5:12 PM');
});

test('a bound node row shows its cron lines in place of the time line, in every view', () => {
  const html = row({backend: 'claude-opus-5', schedule_cron: '0 9 * * *', schedule_timezone: 'PT'});
  const bound = loadGroupsWithCron().Sidebar.renderSessionItem(
    {id: 's1', name: 'demo', updated_at: '2026-07-29T17:12:00Z', backend: 'claude-opus-5',
     schedule_task: 'nightly', schedule_cron: '0 9 * * *', schedule_timezone: 'PT',
     schedule_enabled: true, schedule_next_run: '2026-07-30T09:00:00Z'},
    'all'
  );
  assert.ok(html.includes('session-backend'), 'an unbound row should show the model');
  assert.ok(bound.includes('0 9 * * *'));
  // The records hold no model for a plain backend that has not run, so the row keeps only its schedule lines.
  assert.equal(modelSpan(bound), null);
  // The same bound row form rides the Later and Archive views.
  assert.equal(modelSpan(loadGroupsWithCron().Sidebar.renderSessionItem(
    {id: 's1', name: 'demo', updated_at: '2026-07-29T17:12:00Z',
     schedule_task: 'nightly', schedule_cron: '0 9 * * *', schedule_timezone: 'PT',
     schedule_enabled: true, schedule_next_run: '2026-07-30T09:00:00Z'}, 'starred')), null);
});

// The scheduled-task view: the bound node's schedule lines name the member and
// model the session's records carry — `backend_from_pool: false` gates nothing.
// The fixture mirrors the speedup-h2-wake records: backend cheap, pool_member and
// native_backend codex-gpt-luna, native_model gpt-6-luna, backend_from_pool false.
function boundPoolRow(extra) {
  return loadGroupsWithCron().Sidebar.renderSessionItem(
    {id: 's1', name: 'h2 实现提速循环（Opus）', updated_at: '2026-07-29T17:12:00Z',
     backend: 'cheap', pool_member: 'codex-gpt-luna', native_backend: 'codex-gpt-luna', native_model: 'gpt-6-luna',
     backend_from_pool: false, scheduled_task: 'speedup-h2-wake',
     schedule_task: 'speedup-h2-wake', schedule_cron: '17 */4 * * *', schedule_timezone: 'America/Los_Angeles',
     schedule_enabled: true, schedule_next_run: '2026-07-30T01:00:00Z', last_run_status: 'success',
     ...extra},
    'all'
  );
}

test('a pool-bound task row shows the pool, its recorded member and its model', () => {
  const html = boundPoolRow({});
  const model = modelSpan(html);
  assert.ok(model, 'bound row is missing its backend line');
  assert.equal(model[3], 'Cheap → Codex GPT Luna · gpt-6-luna');
  assert.equal(model[2], 'Cheap → Codex GPT Luna · gpt-6-luna');
  assert.ok(html.includes('17 */4 * * *'), 'the cron line stays');
  assert.ok(html.includes('Last: success'), 'the Last line stays');
});

test('a pool-bound task that has not run shows the pool label alone', () => {
  const html = boundPoolRow({pool_member: undefined, native_model: undefined, last_run_status: undefined});
  const model = modelSpan(html);
  assert.ok(model, 'a pool-bound row keeps its pool label');
  assert.equal(model[3], 'Cheap');
});

test('a plain-bound task row shows the recorded model alone', () => {
  const html = loadGroupsWithCron().Sidebar.renderSessionItem(
    {id: 's1', name: 'demo', updated_at: '2026-07-29T17:12:00Z', backend: 'claude-opus-5',
     native_model: 'claude-opus-5-5',
     schedule_task: 'nightly', schedule_cron: '0 9 * * *', schedule_timezone: 'PT',
     schedule_enabled: true, schedule_next_run: '2026-07-30T09:00:00Z'},
    'all'
  );
  const model = modelSpan(html);
  assert.ok(model, 'a run plain-bound row names its model');
  assert.equal(model[3], 'claude-opus-5-5');
});
