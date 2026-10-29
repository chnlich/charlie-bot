// ---------------------------------------------------------------------------
// The chat column's pending-triggers tray (chat/triggers-tray.js): collapsed
// and expanded render, the four watch-target labels, pure-delay vs watch
// condition text, the (count, next)-driven refetch, the two-step cancel with
// its 3 s revert, the visible cancel failure, hidden at zero pending, and the
// bell's open-and-expand + title.
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
  await context.renderPendingTriggersTray('session-a');
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
  // Separator dots ride between the time, the target and the message, as in
  // the mockup's collapsed frames (one even without a watch target). They are
  // sm:-only: below sm: line 1 has no separators and line 2 is the message.
  assert.ok(html.includes('<span class="shrink-0 text-slate-600 hidden sm:inline">·</span>'));
  // Collapsed: no cancel button anywhere.
  assert.ok(!html.includes('Cancel'));
});

test('collapsed tray with one trigger shows no +N more', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([trigger('t1', at(1, 0), 'only one')]);
  await context.renderPendingTriggersTray('session-a');
  await tick();

  assert.equal(tray.classList.contains('hidden'), false);
  assert.ok(!tray.innerHTML.includes('more'), tray.innerHTML);
});

// Below 640 px (Tailwind's sm:) the collapsed row is two lines. The row is a
// wrapping flex row and the message is a full-basis, last-ordered item, so it
// always starts line 2: line 1 holds the bell, label, time and +N, and the
// message gets the row's whole content width to truncate in.
test('below sm the collapsed row wraps the message onto its own second line', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([
    trigger('t1', at(3, 52), 'DeepSeek 恢复或帖子新回复', [{kind: 'local_pid', pid: 2626907}]),
    trigger('t2', at(16, 17), 'check the eval sweep results'),
  ]);
  await context.renderPendingTriggersTray('session-a');
  await tick();

  const html = tray.innerHTML;
  // The row container: a flex row that wraps below sm:.
  const rowClass = 'tray-border rounded-xl bg-slate-800 flex flex-wrap sm:flex-nowrap'
    + ' items-center gap-x-2 gap-y-1 px-3 py-2 text-sm cursor-pointer';
  assert.ok(html.includes(`<div class="${rowClass}"`), html);
  // The message: full flex basis, placed last, so line breaking gives it a
  // line of its own; it truncates and titles on that line.
  const messageClass = 'truncate text-slate-400 min-w-[3rem] basis-full order-last sm:order-none sm:flex-1';
  const msgStart = html.indexOf(`<span class="${messageClass}"`);
  assert.ok(msgStart > -1, html);
  assert.ok(html.slice(msgStart).includes('title="DeepSeek 恢复或帖子新回复"'), 'the message keeps its title');
  // Line 1 carries the bell, the label and the time ahead of the message in
  // document order; +N and the chevron follow the message in the markup but
  // carry no order, so the order-modulated line breaking keeps them on line 1.
  for (const marker of [
    '<svg class="w-4 h-4 text-amber-400 flex-shrink-0"',
    '>Next trigger<',
    '>' + context.clockTimeHM(new context.Date(at(3, 52))) + '<',
  ]) {
    const at0 = html.indexOf(marker);
    assert.ok(at0 > -1 && at0 < msgStart, `${marker} belongs to line 1`);
  }
  assert.equal(html.match(/order-last/g).length, 1, 'the message is the row\'s only last-ordered item');
  assert.match(html, /<span class="text-xs text-slate-500 whitespace-nowrap" title="\+1 more">/,
    '+N stays on line 1');
  // No separator rides on line 1: the target dot and the message dot are both
  // sm:-only, so line 1 ends with +N and the chevron.
  assert.equal(html.match(/class="shrink-0 text-slate-600 hidden sm:inline"/g).length, 2);
  // The two-line form is the CSS the classes resolve to: a wrapping row plus a
  // 100%-basis item ordered after its siblings. Asserting the rules, not only
  // the names, is what makes the wrap mechanical.
  const css = fs.readFileSync(path.join(__dirname, '..', 'web', 'static', 'css', 'tailwind.css'), 'utf8');
  assert.match(css, /\.flex-wrap\s*{\s*flex-wrap: wrap;\s*}/);
  assert.match(css, /\.basis-full\s*{\s*flex-basis: 100%;\s*}/);
  assert.match(css, /\.order-last\s*{\s*order: 9999;\s*}/);
});

// From 640 px up the row is exactly the single line it has always been: no
// wrap, the message flex-1 and back in document order, and the watch target,
// relative time and separators visible.
test('at sm and wider the collapsed row stays one line', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([
    trigger('t1', at(3, 52), 'DeepSeek 恢复或帖子新回复', [{kind: 'local_pid', pid: 2626907}]),
    trigger('t2', at(16, 17), 'check the eval sweep results'),
  ]);
  await context.renderPendingTriggersTray('session-a');
  await tick();

  const html = tray.innerHTML;
  assert.match(html, /class="[^"]*sm:flex-nowrap[^"]*"/, 'the row never wraps from sm: up');
  // The message: flex-1 again, order restored, so it follows the watch target
  // and precedes +N as it always has.
  const messageClass = 'truncate text-slate-400 min-w-[3rem] basis-full order-last sm:order-none sm:flex-1';
  assert.ok(html.includes(`<span class="${messageClass}" title="DeepSeek 恢复或帖子新回复">`), html);
  assert.ok(html.indexOf('pid 2626907') < html.indexOf(messageClass),
      'the watch target precedes the message at sm: and wider');
  // The desktop-only spans stay in the markup and show at sm:.
  assert.match(html, /<span class="text-amber-300 font-medium whitespace-nowrap">Next trigger<\/span>/);
  assert.match(html, /<span class="hidden sm:inline text-slate-500 whitespace-nowrap"><span class="tray-reltime"/);
  const watchClass = 'tray-mono text-amber-300 whitespace-nowrap min-w-0 truncate hidden sm:inline';
  assert.ok(html.includes(`<span class="${watchClass}" title="pid 2626907">pid 2626907</span>`), html);
  // "+N more" reads in full, as before.
  assert.match(html, /\+1<span class="hidden sm:inline"> more<\/span>/);
  // The sm: overrides exist and restore the one-line layout.
  const css = fs.readFileSync(path.join(__dirname, '..', 'web', 'static', 'css', 'tailwind.css'), 'utf8');
  const smBlock = css.slice(css.lastIndexOf('@media (min-width: 640px)'));
  assert.match(smBlock, /\.sm\\:flex-nowrap\s*{\s*flex-wrap: nowrap;\s*}/);
  assert.match(smBlock, /\.sm\\:order-none\s*{\s*order: 0;\s*}/);
  assert.match(smBlock, /\.sm\\:flex-1\s*{\s*flex: 1 1 0%;\s*}/);
});

test('a long watch label keeps its full text in title while the span can shrink', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([
    trigger('t1', at(1, 0), 'training epoch boundary', [{kind: 'slurm_job', host: 'host3', job_id: 1234567}]),
  ]);
  await context.renderPendingTriggersTray('session-a');
  await tick();

  assert.match(tray.innerHTML,
    /title="host3:slurm:1234567">host3:slurm:1234567<\/span>/);
  assert.match(tray.innerHTML, /min-w-0 truncate/);
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
  await context.renderPendingTriggersTray('session-a');
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
  await context.renderPendingTriggersTray('session-a');
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
  await context.renderPendingTriggersTray('session-a');
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
  await context.renderPendingTriggersTray('session-a');
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
  await context.renderPendingTriggersTray('session-a');
  await tick();

  assert.equal(tray.classList.contains('hidden'), true);
  assert.equal(tray.innerHTML, '');

  context.setSessionPendingTriggerTrayStatus('session-a', {pending_trigger_count: 0, next_trigger_at: null});
  await tick();
  assert.equal(calls.length, 1, 'the open fetch already proved (0, null)');
  assert.equal(tray.classList.contains('hidden'), true);
});

test('cancel arms with Cancel? for 3 seconds, reverts, and fires on the second click', async () => {
  const {context, tray, calls, timeouts, cleared} = buildTrayContext();
  context.setTriggers([trigger('t1', at(1, 0), 'cancel me')]);
  await context.renderPendingTriggersTray('session-a');
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
  await context.renderPendingTriggersTray('session-a');
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
  await context.renderPendingTriggersTray('session-a');
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
  await context.renderPendingTriggersTray('session-a');
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
  await context.renderPendingTriggersTray('session-a');
  await tick();

  context.openPendingTriggers('session-b');
  assert.deepEqual(switchedTo, ['session-b']);

  // The switch lands: renderSessionView opens the new session's tray expanded.
  context.SESSION_ID = 'session-b';
  await context.renderPendingTriggersTray('session-b');
  await tick();
  assert.match(tray.innerHTML, /1 pending trigger\b/);
  assert.match(tray.innerHTML, /· next/, 'the tray opened expanded');
});

test('the bell on the open session expands the tray in place', async () => {
  const {context, tray} = buildTrayContext();
  context.setTriggers([trigger('t1', at(1, 0), 'already open')]);
  await context.renderPendingTriggersTray('session-a');
  await tick();
  assert.ok(!tray.innerHTML.includes('· next'), 'collapsed by default');

  context.openPendingTriggers('session-a');
  assert.match(tray.innerHTML, /· next/);
});

test('the bell title names the count and the next fire time, and clicking it expands', () => {
  const {context} = buildTrayContext();
  const html = context.renderPendingTriggerIndicator({
    id: 's1', has_pending_trigger: true, pending_trigger_count: 2,
    next_trigger_at: '2026-04-02T12:00:00Z',
  });
  assert.match(html, /title="2 pending delayed triggers · next /);
  assert.match(html, /onclick="event\.preventDefault\(\); event\.stopPropagation\(\); sidebarControl\('openPendingTriggers', 's1'\)"/);

  const idle = context.renderPendingTriggerIndicator({
    id: 's2', has_pending_trigger: false, pending_trigger_count: 0, next_trigger_at: null,
  });
  assert.match(idle, /class="[^"]*hidden/);
  assert.ok(!idle.includes('openPendingTriggers') || idle.includes('hidden'),
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
