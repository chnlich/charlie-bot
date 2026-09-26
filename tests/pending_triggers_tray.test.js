// ---------------------------------------------------------------------------
// The chat column's pending-triggers tray (chat/triggers-tray.js): collapsed
// and expanded render, the four watch-target labels, pure-delay vs watch
// condition text, the (count, next)-driven refetch, the two-step cancel with
// its 3 s revert, the visible cancel failure, hidden at zero pending and in a
// legacy thread view, and the bell's open-and-expand + title.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const {readStatic} = require('./read_static');
const {createChatSidebarContext, baseSessionContext} = require('./session_context_stub');
const {createElement} = require('./dom_element_stub');

// The tray's clock is fixed, so "(in 3h 52m)"-shaped text is assertable: the
// module's Date.now() and bare new Date() both answer REAL_NOW.
const REAL_NOW = Date.parse('2026-04-02T10:00:00Z');

class FixedDate extends Date {
  constructor(...args) {
    if (args.length === 0) super(REAL_NOW);
    else super(...args);
  }

  static now() { return REAL_NOW; }
}

const at = (hours, minutes) => new Date(REAL_NOW + hours * 3600000 + minutes * 60000).toISOString();
const offsetForm = (iso) => iso.replace(/Z$/, '+00:00');

function trigger(id, fireIso, message, targets = []) {
  return {
    id, session_id: 'session-a', message, fire_at: fireIso,
    created_at: '2026-04-01T00:00:00Z', status: 'pending', fired_at: null,
    watch_targets: targets, fire_reason: null,
  };
}

function buildTrayContext({elements = new Map(), initialTriggers = null} = {}) {
  const tray = createElement({id: 'pending-triggers-tray', className: 'hidden'});
  elements.set('pending-triggers-tray', tray);
  const {context} = baseSessionContext({elements});
  context.Date = FixedDate;
  context.SESSION_ID = 'session-a';

  let currentTriggers = initialTriggers;
  const calls = [];
  context.fetch = async (url, opts = {}) => {
    const method = opts.method || 'GET';
    calls.push({url, method});
    if (url.endsWith('/pending-triggers')) {
      return {ok: true, status: 200, headers: {get: () => 'application/json'},
        json: async () => (currentTriggers || []).map((t) => ({...t}))};
    }
    if (url.includes('/cancel')) {
      return {ok: true, status: 200, headers: {get: () => 'application/json'}, json: async () => ({ok: true})};
    }
    throw new Error('unexpected fetch ' + url);
  };

  const timeouts = [];
  const cleared = [];
  context.setTimeout = (fn, ms) => { timeouts.push({fn, ms}); return timeouts.length; };
  context.clearTimeout = (id) => cleared.push(id);

  context.setTriggers = (list) => { currentTriggers = list; };

  context.document.getElementById = (id) => elements.get(id) || null;
  context.document.querySelectorAll = () => [];

  // The page loads utils.js before the chat/sidebar modules (index.html); the
  // harness mirrors that so the tray's time helpers are the real ones.
  vm.createContext(context);
  vm.runInContext(readStatic('utils.js'), context, {filename: 'utils.js'});
  createChatSidebarContext(context);
  // session-view.js's wire exposes the real switchSession; the bell test's
  // stand-in replaces it after load, the same call-time seam the module reads.
  const switchedTo = [];
  context.switchSession = (sid) => { switchedTo.push(sid); };
  return {context, tray, calls, timeouts, cleared, switchedTo};  // switchedTo mutates in place
}

const tick = () => new Promise((resolve) => setImmediate(resolve));

test('collapsed tray shows the next trigger line, target, message and +N more', async () => {
  const {context, tray, calls} = buildTrayContext();
  context.setTriggers([
    trigger('t1', at(3, 52), 'tests finished: confirm the push landed on origin/main, then deploy',
        [{kind: 'local_pid', pid: 12345}]),
    trigger('t2', at(16, 17), 'check the eval sweep results'),
    trigger('t3', at(19, 17), 'summarize the training run'),
  ]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();

  assert.equal(calls.length, 1);
  assert.equal(calls[0].url, '/api/sessions/session-a/pending-triggers');
  assert.equal(tray.classList.contains('hidden'), false);
  const html = tray.innerHTML;
  assert.match(html, /Next trigger/);
  // The earliest trigger leads, in the viewer's local clock.
  assert.ok(html.includes(context.clockTimeHM(new context.Date(at(3, 52)))), html);
  assert.match(html, /\(in 3h 52m\)/);
  assert.match(html, /pid 12345/);
  assert.match(html, /tests finished: confirm the push landed on origin\/main, then deploy/);
  assert.match(html, /\+2 more/);
  // Collapsed: no cancel button anywhere.
  assert.ok(!html.includes('Cancel'));
});

test('collapsed tray with one trigger shows no +N more', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([trigger('t1', at(1, 0), 'only one')]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();

  assert.equal(tray.classList.contains('hidden'), false);
  assert.ok(!tray.innerHTML.includes('more'), tray.innerHTML);
});

test('expanded tray lists every trigger in fire_at order with header and condition lines', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([
    trigger('t1', at(3, 52), 'tests finished: confirm the push landed',
        [{kind: 'local_pid', pid: 12345}]),
    trigger('t2', at(16, 17), 'check the eval sweep results and report the best checkpoint', []),
    trigger('t3', at(19, 17), 'summarize the training run',
        [{kind: 'slurm_job', host: 'cluster-a', job_id: 48213}]),
  ]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();
  context.togglePendingTriggersTray(true);

  const html = tray.innerHTML;
  assert.match(html, /3 pending triggers/);
  assert.match(html, /· next/);
  const i1 = html.indexOf('tests finished: confirm the push landed');
  const i2 = html.indexOf('check the eval sweep results and report the best checkpoint');
  const i3 = html.indexOf('summarize the training run');
  assert.ok(i1 > -1 && i2 > i1 && i3 > i2, 'rows are in fire_at order');
  // The three condition spellings: watched, pure delay, remote slurm.
  assert.match(html, /fires when <span class="tray-mono text-amber-300">pid 12345<\/span> exits · at the latest /);
  assert.match(html, /fires at /);
  assert.match(html, /fires when <span class="tray-mono text-amber-300">cluster-a:slurm:48213<\/span> exits · at the latest /);
  // Cancel buttons ride every row.
  assert.match(html, /title="Cancel trigger"/g);
});

test('the four watch-target labels render as the --watch spellings', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([
    trigger('t1', at(1, 0), 'local', [{kind: 'local_pid', pid: 123}]),
    trigger('t2', at(2, 0), 'remote', [{kind: 'remote_pid', host: 'neptune', pid: 456}]),
    trigger('t3', at(3, 0), 'slurm local', [{kind: 'slurm_job', host: null, job_id: 98765}]),
    trigger('t4', at(4, 0), 'slurm remote', [{kind: 'slurm_job', host: 'neptune', job_id: 122111}]),
  ]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();
  context.togglePendingTriggersTray(true);

  const html = tray.innerHTML;
  assert.match(html, />pid 123<\/span> exits/);
  assert.match(html, />neptune:456<\/span> exits/);
  assert.match(html, />slurm:98765<\/span> exits/);
  assert.match(html, />neptune:slurm:122111<\/span> exits/);
});

test('two watch targets read "exit", one reads "exits"', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([
    trigger('t1', at(1, 0), 'two targets',
        [{kind: 'local_pid', pid: 123}, {kind: 'local_pid', pid: 456}]),
  ]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();
  context.togglePendingTriggersTray(true);

  assert.match(tray.innerHTML, />pid 123, pid 456<\/span> exit · at the latest /);
});

test('a changed (count, next) pair refetches; an unchanged pair does not', async () => {
  const {context, calls} = buildTrayContext();
  const nextIso = at(3, 52);
  context.setTriggers([
    trigger('t1', nextIso, 'first'),
    trigger('t2', at(8, 0), 'second'),
  ]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();
  assert.equal(calls.length, 1);

  // Same count and the same instant in the status payload's isoformat shape.
  context.setSessionPendingTriggerTrayStatus('session-a',
      {pending_trigger_count: 2, next_trigger_at: offsetForm(nextIso)});
  await tick();
  assert.equal(calls.length, 1, 'unchanged pair fetches nothing');

  context.setSessionPendingTriggerTrayStatus('session-a',
      {pending_trigger_count: 2, next_trigger_at: at(9, 30)});
  await tick();
  assert.equal(calls.length, 2, 'a moved next_trigger_at refetches');

  context.setSessionPendingTriggerTrayStatus('session-a',
      {pending_trigger_count: 3, next_trigger_at: at(9, 30)});
  await tick();
  assert.equal(calls.length, 3, 'a moved count refetches');

  // Another session's status never touches this tray.
  context.setSessionPendingTriggerTrayStatus('session-b',
      {pending_trigger_count: 9, next_trigger_at: at(9, 30)});
  await tick();
  assert.equal(calls.length, 3);
});

test('a zero-pending poll hides the tray and an empty session stays hidden', async () => {
  const {context, tray, calls} = buildTrayContext();
  context.setTriggers([]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();

  assert.equal(tray.classList.contains('hidden'), true);
  assert.equal(tray.innerHTML, '');

  context.setSessionPendingTriggerTrayStatus('session-a', {pending_trigger_count: 0, next_trigger_at: null});
  await tick();
  assert.equal(calls.length, 1, 'the open fetch already proved (0, null)');
  assert.equal(tray.classList.contains('hidden'), true);
});

test('a legacy thread view shows no tray and answers no status', async () => {
  const {context, tray, calls} = buildTrayContext();
  context.setTriggers([trigger('t1', at(1, 0), 'hidden here')]);
  await context.renderPendingTriggersTray('session-a', true);
  await tick();

  assert.equal(calls.length, 0);
  assert.equal(tray.classList.contains('hidden'), true);

  context.setSessionPendingTriggerTrayStatus('session-a', {pending_trigger_count: 1, next_trigger_at: at(1, 0)});
  await tick();
  assert.equal(calls.length, 0);
});

test('cancel arms with Cancel? for 3 seconds, reverts, and fires on the second click', async () => {
  const {context, tray, calls, timeouts, cleared} = buildTrayContext();
  context.setTriggers([trigger('t1', at(1, 0), 'cancel me')]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();
  context.togglePendingTriggersTray(true);

  const evt = {preventDefault() {}, stopPropagation() {}};
  context.trayCancelClick(evt, 'session-a', 't1');
  assert.match(tray.innerHTML, /Cancel\?/);
  assert.match(tray.innerHTML, /Click again to cancel/);
  assert.equal(calls.filter((c) => c.method === 'POST').length, 0, 'the first click sends nothing');
  assert.equal(timeouts.length, 1);
  assert.equal(timeouts[0].ms, 3000);

  // The second click inside the window cancels through the internal endpoint.
  context.trayCancelClick(evt, 'session-a', 't1');
  await tick();
  assert.deepEqual(calls.filter((c) => c.method === 'POST'), [
    {url: '/api/internal/triggers/session-a/t1/cancel', method: 'POST'},
  ]);
  assert.ok(cleared.includes(1), 'the revert timer is disarmed');
  // A 2xx refetches the list immediately.
  assert.equal(calls.filter((c) => c.url.endsWith('/pending-triggers')).length, 2);
});

test('an armed cancel reverts to the X button after 3 seconds without a second click', async () => {
  const {context, tray, timeouts} = buildTrayContext();
  context.setTriggers([trigger('t1', at(1, 0), 'cancel me')]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();
  context.togglePendingTriggersTray(true);

  const evt = {preventDefault() {}, stopPropagation() {}};
  context.trayCancelClick(evt, 'session-a', 't1');
  assert.match(tray.innerHTML, /Cancel\?/);

  timeouts[0].fn();
  assert.ok(!tray.innerHTML.includes('Cancel?'), tray.innerHTML);
  assert.match(tray.innerHTML, /title="Cancel trigger"/);
});

test('a failed cancel keeps the row and shows the HTTP status and detail', async () => {
  const {context, tray, calls} = buildTrayContext();
  let cancelStatus = 404;
  context.fetch = async (url, opts = {}) => {
    calls.push({url, method: opts.method || 'GET'});
    if (url.endsWith('/pending-triggers')) {
      return {ok: true, status: 200, headers: {get: () => 'application/json'},
        json: async () => [trigger('t1', at(1, 0), 'cancel me')]};
    }
    return {ok: false, status: cancelStatus, headers: {get: () => 'application/json'},
      json: async () => ({detail: 'Trigger not found'})};
  };
  await context.renderPendingTriggersTray('session-a', false);
  await tick();
  context.togglePendingTriggersTray(true);

  const evt = {preventDefault() {}, stopPropagation() {}};
  context.trayCancelClick(evt, 'session-a', 't1');
  context.trayCancelClick(evt, 'session-a', 't1');
  await tick();

  const html = tray.innerHTML;
  assert.match(html, /Cancel failed: HTTP 404 · Trigger not found/);
  assert.match(html, /cancel me/, 'the row stays');
  assert.ok(!calls.slice(1).some((c) => c.url.endsWith('/pending-triggers')),
      'a failed cancel does not refetch');
});

test('a network-failed cancel keeps the row and shows the error', async () => {
  const {context, tray} = buildTrayContext();
  context.fetch = async (url) => {
    if (url.endsWith('/pending-triggers')) {
      return {ok: true, status: 200, headers: {get: () => 'application/json'},
        json: async () => [trigger('t1', at(1, 0), 'cancel me')]};
    }
    throw new TypeError('Failed to fetch');
  };
  await context.renderPendingTriggersTray('session-a', false);
  await tick();
  context.togglePendingTriggersTray(true);

  const evt = {preventDefault() {}, stopPropagation() {}};
  context.trayCancelClick(evt, 'session-a', 't1');
  context.trayCancelClick(evt, 'session-a', 't1');
  await tick();

  assert.match(tray.innerHTML, /Cancel failed: network error · /);
  assert.match(tray.innerHTML, /cancel me/, 'the row stays');
});

test('the bell opens another session and expands the tray it lands on', async () => {
  const {context, tray, switchedTo} = buildTrayContext();
  context.setTriggers([trigger('t1', at(1, 0), 'bell target')]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();

  context.expandPendingTriggersTray('session-b');
  assert.deepEqual(switchedTo, ['session-b']);

  // The switch lands: renderSessionView opens the new session's tray expanded.
  context.SESSION_ID = 'session-b';
  await context.renderPendingTriggersTray('session-b', false);
  await tick();
  assert.match(tray.innerHTML, /1 pending trigger\b/);
  assert.match(tray.innerHTML, /· next/, 'the tray opened expanded');
});

test('the bell on the open session expands the tray in place', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([trigger('t1', at(1, 0), 'already open')]);
  await context.renderPendingTriggersTray('session-a', false);
  await tick();
  assert.ok(!tray.innerHTML.includes('· next'), 'collapsed by default');

  context.expandPendingTriggersTray('session-a');
  assert.match(tray.innerHTML, /· next/);
});

test('the bell title names the count and the next fire time, and clicking it expands', () => {
  const {context} = buildTrayContext();
  const html = context.renderPendingTriggerIndicator({
    id: 's1', has_pending_trigger: true, pending_trigger_count: 2,
    next_trigger_at: '2026-04-02T12:00:00Z',
  });
  assert.match(html, /title="2 pending delayed triggers · next /);
  assert.match(html, /onclick="event\.preventDefault\(\); event\.stopPropagation\(\); expandPendingTriggersTray\('s1'\)"/);

  const idle = context.renderPendingTriggerIndicator({
    id: 's2', has_pending_trigger: false, pending_trigger_count: 0, next_trigger_at: null,
  });
  assert.match(idle, /class="[^"]*hidden/);
  assert.ok(!idle.includes('expandPendingTriggersTray') || idle.includes('hidden'),
      'the hidden bell keeps its click attr but is not shown');
});

test('updateRelativeTimes refreshes the tray spans through the existing sweep', () => {
  const {context} = buildTrayContext();
  const rel = createElement({className: 'tray-reltime'});
  rel.dataset.time = at(3, 52);
  context.document.querySelectorAll = (selector) => {
    if (selector === '.tray-reltime[data-time]') return [rel];
    if (selector === '.session-time[data-time]') return [];
    throw new Error('unexpected selector ' + selector);
  };
  context.updateRelativeTimes();
  assert.equal(rel.textContent, '(in 3h 52m)');
});

test('the tray node sits between the thinking indicator and the input area', () => {
  const html = fs.readFileSync(path.join(__dirname, '..', 'web', 'templates', 'index.html'), 'utf8');
  const thinking = html.indexOf('id="thinking"');
  const tray = html.indexOf('id="pending-triggers-tray"');
  const input = html.indexOf('id="input-area"');
  assert.ok(thinking > -1 && tray > thinking && input > tray,
      `tray must sit between #thinking (${thinking}) and #input-area (${input}); got ${tray}`);
});
