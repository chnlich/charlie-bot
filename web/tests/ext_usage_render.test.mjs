import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

class FakeClassList {
  constructor(element, initialClassName = '') {
    this._element = element;
    this._classes = new Set(initialClassName.split(/\s+/).filter(Boolean));
  }

  _sync() {
    this._element._className = [...this._classes].join(' ');
  }

  _replace(className) {
    this._classes = new Set(className.split(/\s+/).filter(Boolean));
    this._sync();
  }

  add(...classes) {
    for (const name of classes) this._classes.add(name);
    this._sync();
  }

  remove(...classes) {
    for (const name of classes) this._classes.delete(name);
    this._sync();
  }

  toggle(name, force) {
    if (force === true) {
      this._classes.add(name);
    } else if (force === false) {
      this._classes.delete(name);
    } else if (this._classes.has(name)) {
      this._classes.delete(name);
    } else {
      this._classes.add(name);
    }
    this._sync();
  }

  contains(name) {
    return this._classes.has(name);
  }
}

class FakeElement {
  constructor(tagName = 'div', initialClassName = '') {
    this.tagName = tagName;
    this.textContent = '';
    this.style = {};
    this.children = [];
    this.attributes = new Map();
    this._listeners = {};
    this._className = '';
    this.classList = new FakeClassList(this, initialClassName);
    Object.defineProperty(this, 'className', {
      get: () => this._className,
      set: (value) => this.classList._replace(value),
    });
    this.className = initialClassName;
  }

  appendChild(child) {
    this.children.push(child);
    return child;
  }

  replaceChildren(...nodes) {
    this.children = [...nodes];
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  getAttribute(name) {
    return this.attributes.has(name) ? this.attributes.get(name) : null;
  }

  addEventListener(type, handler) {
    (this._listeners[type] = this._listeners[type] || []).push(handler);
  }
}

function _walk(root, predicate) {
  for (const child of root.children) {
    if (predicate(child)) return child;
    const found = _walk(child, predicate);
    if (found) return found;
  }
  return null;
}

function _byAttr(attr, value) {
  return (el) => el.getAttribute(attr) === value;
}

function _rowByKey(strip, key) {
  return _walk(strip, _byAttr('data-key', key));
}

function _field(row, name) {
  return _walk(row, _byAttr('data-field', name));
}

// Controllable in-memory localStorage: call counts let tests prove the
// default is never written back.
function makeStorage(initial = {}) {
  const map = new Map(Object.entries(initial));
  return {
    calls: { get: 0, set: 0 },
    getItem(key) { this.calls.get += 1; return map.has(key) ? map.get(key) : null; },
    setItem(key, value) { this.calls.set += 1; map.set(key, String(value)); },
    removeItem(key) { map.delete(key); },
  };
}

function makeThrowingStorage() {
  const denied = (op) => () => { throw new Error('localStorage ' + op + ' refused'); };
  return { getItem: denied('getItem'), setItem: denied('setItem'), removeItem: denied('removeItem') };
}

function loadExtUsageScript(options = {}) {
  const strip = new FakeElement('div', 'hidden flex items-center gap-4');
  const chip = new FakeElement('button', 'hidden absolute');
  const storage = options.storage || makeStorage();
  const documentHandlers = {};
  const document = {
    readyState: options.readyState || 'loading',
    addEventListener(type, handler) {
      documentHandlers[type] = handler;
    },
    getElementById(id) {
      if (id === 'ext-usage-strip') return strip;
      if (id === 'ext-usage-toggle') return chip;
      return null;
    },
    createElement(tag) {
      return new FakeElement(tag);
    },
  };
  const context = {
    console,
    Date,
    document,
    localStorage: storage,
    // config.js's shared meter-fill literal; this harness skips config.js.
    PROGRESS_BAR_FILL_CLASS: 'h-full rounded-full transition-all duration-300',
    // Never settles: the DOMContentLoaded bootstrap chain must not resolve
    // fetches during unit tests, and must not throw out of the handler either.
    fetch() {
      return new Promise(() => {});
    },
    setInterval() {
      return 0;
    },
    clearInterval() {},
    registerWsMessageHandler() {},
  };
  context.window = context;
  if (options.platform !== undefined) {
    context.platform = options.platform;
  }

  const scriptPath = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../features/usage/static/ext_usage.js');
  const scriptSource = fs.readFileSync(scriptPath, 'utf8');
  const pageTimersPath = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../static/js/page-timers.js');
  const pageTimersSource = fs.readFileSync(pageTimersPath, 'utf8');
  vm.createContext(context);
  // page-timers.js loads first, matching the script order in index.html — the
  // strip's refresh timers register through it.
  vm.runInContext(pageTimersSource, context, { filename: pageTimersPath });
  // Bridge the lexical module binding (a top-level binding is not a property
  // of the vm context object). This lets tests read the one in-memory collapsed
  // boolean rather than inferring it from DOM state.
  vm.runInContext(
    scriptSource + '\nglobalThis._peekExtUsageCollapsed = () => _extUsageCollapsed;',
    context,
    { filename: scriptPath });

  return {
    context,
    strip,
    chip,
    storage,
    isCollapsed: () => context._peekExtUsageCollapsed(),
    fireDOMContentLoaded() {
      assert.ok(documentHandlers.DOMContentLoaded, 'ext_usage.js registered a DOMContentLoaded handler');
      documentHandlers.DOMContentLoaded();
    },
  };
}

// Dispatches a click through every registered listener, returning how many
// fired — listener accumulation idempotence failures show up as counts > 1.
function _click(element) {
  const handlers = (element._listeners && element._listeners.click) || [];
  assert.ok(handlers.length > 0, 'expected a click listener on <' + element.tagName + '>');
  for (const handler of handlers) handler();
  return handlers.length;
}

const MINUTE = 60000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;

// Payloads are built relative to now so a fixed calendar date can never drift
// into "expired" and silently change what these tests assert.
function _iso(offsetMs) {
  return new Date(Date.now() + offsetMs).toISOString();
}

function _claudePayload(overrides = {}) {
  return {
    provider: 'claude',
    provider_label: 'Claude',
    account: 'main',
    windows: [
      { window_minutes: 300, utilization: 42.0, resets_at: _iso(2 * HOUR) },
      { window_minutes: 10080, utilization: 10.0, resets_at: _iso(3 * DAY) },
    ],
    fetched_at: _iso(-1 * MINUTE),
    ...overrides,
  };
}

function _codexPayload(overrides = {}) {
  return {
    provider: 'codex',
    provider_label: 'Codex',
    account: 'main',
    windows: [{ window_minutes: 10080, utilization: 96.0, resets_at: _iso(2 * DAY) }],
    fetched_at: _iso(-1 * MINUTE),
    token_count_observed_at: _iso(-2 * MINUTE),
    spend: null,
    ...overrides,
  };
}

test('renderExtUsage shows business/unlimited Codex state with no quota bars', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'codex:main': _codexPayload({ windows: [], state_label: 'business / unlimited' }),
    },
  });

  assert.equal(strip.classList.contains('hidden'), false);
  const claudeRow = _rowByKey(strip, 'claude:main');
  assert.ok(claudeRow, 'claude:main row rendered');
  assert.equal(_field(claudeRow, '5h-pct').textContent, '42%');
  const claudeReset = _field(claudeRow, '5h-reset');
  assert.ok(claudeReset, 'claude 5h reset rendered');
  assert.ok(claudeReset.textContent.startsWith('(') && claudeReset.textContent.includes(' \u2013 '));

  const codexRow = _rowByKey(strip, 'codex:main');
  assert.ok(codexRow, 'codex:main row rendered');
  const stateBadge = _field(codexRow, 'state');
  assert.ok(stateBadge, 'state badge rendered');
  assert.equal(stateBadge.textContent, 'business / unlimited');
  assert.equal(_field(codexRow, 'no-cap').textContent, 'plan \u00b7 no cap');
  assert.equal(_field(codexRow, '5h-bar'), null, 'no invented 5h bar');
  assert.equal(_field(codexRow, '7d-bar'), null, 'no invented 7d bar');
});

test('renderExtUsage uses provider window names', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'opencode:main': {
        provider: 'opencode',
        provider_label: 'OpenCode Go',
        account: 'main',
        fetched_at: _iso(-1 * MINUTE),
        windows: [
          { window_minutes: 300, utilization: 8, resets_at: _iso(HOUR) },
          { window_minutes: 10080, utilization: 25, resets_at: _iso(DAY) },
          { window_name: '1mo', utilization: 97, resets_at: _iso(14 * DAY) },
        ],
      },
    },
  });

  // The fixed windows are named from their minutes; the calendar month keeps its own name.
  const opencodeRow = _rowByKey(strip, 'opencode:main');
  for (const label of ['5h:', '7d:']) {
    assert.ok(_walk(opencodeRow, (el) => el.textContent === label), `${label} labels a fixed window`);
  }
  assert.ok(_walk(opencodeRow, _byAttr('data-window-name', '1mo')), 'the month window keeps its name');
  assert.ok(_walk(opencodeRow, (el) => el.textContent === '1mo:'), 'the month window is labelled 1mo');
});

test('renderExtUsage names a one-day window 1d from its minutes', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'devin:main': {
        provider: 'devin',
        provider_label: 'Devin',
        account: 'main',
        fetched_at: _iso(-1 * MINUTE),
        windows: [
          { window_minutes: 1440, utilization: 18, resets_at: _iso(5 * HOUR) },
          { window_minutes: 10080, utilization: 0, resets_at: _iso(6 * DAY) },
        ],
      },
    },
  });

  const devinRow = _rowByKey(strip, 'devin:main');
  assert.ok(_walk(devinRow, (el) => el.textContent === '1d:'), 'the daily window is labelled 1d');
  assert.ok(_walk(devinRow, (el) => el.textContent === '7d:'), 'the weekly window is labelled 7d');
});

// Metered credits: the payload's credits reading (unlimited=false + balance)
// replaces the no-cap marker on an empty-windows row and reports itself in the
// collapsed summary as "cr <balance>". The real wire balance is a fractional
// decimal; the UI rounds to the unit and groups thousands.
test('renderExtUsage shows the credits balance in place of no-cap for a metered account', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        windows: [],
        credits: { unlimited: false, balance: 29779.358283042908 },
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.ok(_field(row, 'credits'), 'credits segment rendered');
  assert.equal(_field(row, 'credits-value').textContent, '29,779');
  assert.equal(_field(row, 'no-cap'), null, 'metered credits replaces the no-cap marker');
  assert.equal(_field(row, 'state'), null, 'a metered account carries no unlimited badge');

  const summarySeg = _walk(strip.children[0], _byAttr('data-summary-account', 'codex:main'));
  assert.equal(_field(summarySeg, 'summary-pct').textContent, 'cr 29,779');
});

// The plan type rides beside the credits reading: the row shows the string the
// event named, and the folded summary names it too. No plan name is
// special-cased, so a second plan string must render the same way.
test('renderExtUsage shows the plan type beside the credits and in the summary', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        windows: [],
        credits: { unlimited: false, balance: 786.2718309164047 },
        plan_type: 'ent26',
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, 'plan-value').textContent, 'ent26');
  const creditsIndex = row.children.findIndex((c) => c.getAttribute('data-field') === 'credits');
  const planIndex = row.children.findIndex((c) => c.getAttribute('data-field') === 'plan');
  assert.ok(creditsIndex >= 0 && planIndex >= 0, 'credits and plan segments rendered');
  assert.equal(planIndex, creditsIndex + 1, 'the plan segment sits beside the credits group');

  const summarySeg = _walk(strip.children[0], _byAttr('data-summary-account', 'codex:main'));
  assert.equal(_field(summarySeg, 'summary-pct').textContent, 'cr 786 · ent26');

  // A row with windows reports each plan string the same way.
  context.renderExtUsage({
    providers: { 'codex:main': _codexPayload({ plan_type: 'business' }) },
  });

  const windowedSummary = _walk(strip.children[0], _byAttr('data-summary-account', 'codex:main'));
  assert.equal(_field(windowedSummary, 'summary-pct').textContent, '96% · business');
  assert.equal(_field(_rowByKey(strip, 'codex:main'), 'plan-value').textContent, 'business');

  // A row with neither windows nor credits names its plan in the no-cap marker,
  // which already says "plan", instead of repeating the word in a second segment.
  context.renderExtUsage({
    providers: { 'codex:main': _codexPayload({ windows: [], plan_type: 'business' }) },
  });

  const noCapRow = _rowByKey(strip, 'codex:main');
  assert.equal(_field(noCapRow, 'no-cap').textContent, 'plan business · no cap');
  assert.equal(_field(noCapRow, 'plan'), null, 'the marker carries the only plan text');
  const noCapSummary = _walk(strip.children[0], _byAttr('data-summary-account', 'codex:main'));
  assert.equal(_field(noCapSummary, 'summary-pct').textContent, 'no cap · business');
});

test('renderExtUsage renders a row without a plan type unchanged', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        windows: [],
        credits: { unlimited: false, balance: 786.2718309164047 },
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, 'plan'), null, 'a planless row renders no plan segment');
  const summarySeg = _walk(strip.children[0], _byAttr('data-summary-account', 'codex:main'));
  assert.equal(_field(summarySeg, 'summary-pct').textContent, 'cr 786');
});

test('renderExtUsage shows a dash for metered credits without a balance', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        windows: [],
        credits: { unlimited: false },
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, 'credits-value').textContent, '\u2014');

  const summarySeg = _walk(strip.children[0], _byAttr('data-summary-account', 'codex:main'));
  assert.equal(_field(summarySeg, 'summary-pct').textContent, 'cr \u2014');
});

// The collapsed summary merges one segment per provider. The account with the
// highest percentage keeps the segment and \u00d7N counts the accounts it stands
// for; segments run tightest percentage first, then readings without one.
test('renderExtUsage merges the collapsed summary into one segment per provider', () => {
  const { context, strip } = loadExtUsageScript();
  const week = (utilization) => [{ window_minutes: 10080, utilization: utilization, resets_at: _iso(3 * DAY) }];

  context.renderExtUsage({
    providers: {
      // Payload order is deliberately not the reading order.
      'claude:main': _claudePayload({ account: 'main', windows: week(62.0) }),
      'devin:main': {
        provider: 'devin', provider_label: 'Devin', account: 'main', plan_type: 'Pro',
        windows: [{ window_minutes: 1440, utilization: 0, resets_at: _iso(HOUR) }],
      },
      'claude:ext-2': _claudePayload({ account: 'ext-2', windows: week(60.0) }),
      'codex:main': _codexPayload({ windows: [], credits: { unlimited: false, balance: 692 }, plan_type: 'ent26' }),
      'claude:ext-1': _claudePayload({ account: 'ext-1', windows: week(71.0) }),
      'opencode:main': {
        provider: 'opencode', provider_label: 'OpenCode Go', account: 'main', error: 'credentials not found',
      },
    },
  });

  const summary = strip.children[0];
  const segments = summary.children.filter((c) => c.getAttribute('data-summary-account') !== null);
  assert.deepEqual(segments.map((c) => c.getAttribute('data-summary-account')),
      ['claude:ext-1', 'devin:main', 'codex:main', 'opencode:main'],
      'segments run by percentage, tightest first, then the readings without a percentage');

  const [claude, devin, codex, opencode] = segments;
  assert.equal(_field(claude, 'summary-pct').textContent, '71%', 'the tightest Claude account keeps the segment');
  assert.ok(_walk(claude, (el) => el.textContent === 'ext-1'), 'the kept account is named');
  assert.equal(_field(claude, 'summary-count').textContent, '\u00d73', 'the Claude segment counts its three accounts');
  assert.equal(_field(devin, 'summary-count'), null, 'a single-account segment carries no count');
  assert.equal(_field(devin, 'summary-pct').textContent, '0% \u00b7 Pro', 'a 0% reading is still a percentage');
  assert.equal(_field(codex, 'summary-pct').textContent, 'cr 692 \u00b7 ent26', 'credits follow the percentages');
  assert.equal(_field(opencode, 'summary-pct').textContent, 'error', 'an error state follows the percentages');

  // Every account keeps its own row in the opened strip.
  for (const key of ['claude:main', 'claude:ext-1', 'claude:ext-2', 'devin:main', 'codex:main', 'opencode:main']) {
    assert.ok(_rowByKey(strip, key), key + ' keeps its own detail row');
  }
});

// A tie keeps payload order, and a provider whose accounts all lack a
// percentage keeps the payload's first account.
test('renderExtUsage keeps payload order on ties and for providers without a percentage', () => {
  const { context, strip } = loadExtUsageScript();
  const half = [{ window_minutes: 10080, utilization: 50.0, resets_at: _iso(3 * DAY) }];

  context.renderExtUsage({
    providers: {
      'claude:second': _claudePayload({ account: 'second', windows: half }),
      'claude:first': _claudePayload({ account: 'first', windows: half }),
      'devin:main': {
        provider: 'devin', provider_label: 'Devin', account: 'main',
        windows: [{ window_minutes: 1440, utilization: 50.0, resets_at: _iso(HOUR) }],
      },
      'codex:b': _codexPayload({ account: 'b', windows: [] }),
      'codex:a': _codexPayload({ account: 'a', windows: [] }),
    },
  });

  const summary = strip.children[0];
  const segments = summary.children.filter((c) => c.getAttribute('data-summary-account') !== null);
  assert.deepEqual(segments.map((c) => c.getAttribute('data-summary-account')),
      ['claude:second', 'devin:main', 'codex:b'],
      'equal percentages keep payload order; the no-percentage provider follows them');
  assert.equal(_field(segments[0], 'summary-count').textContent, '\u00d72',
      'the first Claude account stands for both');
  assert.equal(_field(segments[2], 'summary-pct').textContent, 'no cap');
  assert.equal(_field(segments[2], 'summary-count').textContent, '\u00d72',
      'the first Codex account stands for both');
});

test('renderExtUsage keeps the unlimited badge and no-cap marker for unlimited credits', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        windows: [],
        credits: { unlimited: true },
        state_label: 'business / unlimited',
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, 'state').textContent, 'business / unlimited');
  assert.equal(_field(row, 'credits'), null, 'unlimited credits renders no credits segment');
  assert.equal(_field(row, 'no-cap').textContent, 'plan \u00b7 no cap');
});

test('renderExtUsage appends the credits segment after the window buckets', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({ credits: { unlimited: false, balance: 1234.5 } }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, 'credits-value').textContent, '1,235');
  const bucketIndex = row.children.findIndex((c) => _field(c, '7d-pct'));
  const creditsIndex = row.children.findIndex((c) => c.getAttribute('data-field') === 'credits');
  assert.ok(bucketIndex >= 0, 'window bucket rendered');
  assert.equal(creditsIndex, bucketIndex + 1, 'credits group directly follows the window buckets');
});

test('renderExtUsage renders only the windows the provider reported', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'codex:personal': _codexPayload({ account: 'personal' }),
    },
  });

  // Codex now reports a weekly window only: one 7d bar, and no 5h bar at all.
  const codexRow = _rowByKey(strip, 'codex:personal');
  assert.equal(_field(codexRow, '7d-pct').textContent, '96%');
  assert.match(_field(codexRow, '7d-reset').textContent, /^resets in /);
  assert.equal(_field(codexRow, '5h-bar'), null, 'weekly-only codex row has no 5h bar');
  assert.equal(_field(codexRow, '5h-pct'), null);

  // Claude still reports both, so both bars stay.
  const claudeRow = _rowByKey(strip, 'claude:main');
  assert.ok(_field(claudeRow, '5h-bar'), 'claude keeps its 5h bar');
  assert.equal(_field(claudeRow, '7d-pct').textContent, '10%');
});

test('renderExtUsage labels a window by its reported length, not its position', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        windows: [
          { window_minutes: 60, utilization: 5.0, resets_at: _iso(30 * MINUTE) },
          { window_minutes: 43200, utilization: 20.0, resets_at: _iso(10 * DAY) },
        ],
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, '1h-pct').textContent, '5%');
  assert.equal(_field(row, '30d-pct').textContent, '20%');
});

test('renderExtUsage shows unknown utilization as ? instead of 0%', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        windows: [{ window_minutes: 10080, utilization: null, resets_at: _iso(2 * DAY) }],
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, '7d-pct').textContent, '?');
  assert.equal(_field(row, '7d-bar').style.width, '0.0%');
  assert.ok(_field(row, '7d-bar').classList.contains('bg-slate-600'), 'unknown bar is grey, not green');
});

// Sampled 9 days ago, the 7d window has certainly rolled over; the server's
// annotation, not the browser's clock math, expires it.
test('renderExtUsage expires a codex window only on the server annotation', () => {
  const { context, strip } = loadExtUsageScript();
  const stalePayload = (extra) => _codexPayload({
    token_count_observed_at: _iso(-9 * DAY),
    windows: [{ window_minutes: 10080, utilization: 96.0, resets_at: _iso(2 * DAY), ...extra }],
  });

  context.renderExtUsage({ providers: { 'codex:main': stalePayload({}) } });
  assert.equal(_field(_rowByKey(strip, 'codex:main'), '7d-pct').textContent, '96%');

  context.renderExtUsage({ providers: { 'codex:main': stalePayload({ expired: true }) } });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, '7d-pct').textContent, '\u2014');
  assert.equal(_field(row, '7d-reset').textContent, 'window reset \u2014 reading expired');
  assert.ok(_field(row, '7d-bar').classList.contains('bg-slate-600'));
});

// The expiry judgement for claude is the server's: one frozen reading renders
// live or expired exactly when the emitted window carries `expired: true`.
// Neither fetched_at age alone nor the browser's own clock math expires it.
test('renderExtUsage expires a claude window only on the server annotation', () => {
  const { context, strip } = loadExtUsageScript();

  const stalePayload = (windows) => ({
    provider: 'claude',
    provider_label: 'Claude',
    account: 'main',
    windows,
    // Stale enough to fail every codex rule: sampled 2d ago, reset passed 1h ago.
    fetched_at: _iso(-2 * DAY),
  });
  const staleWindow = (extra) => Object.assign(
    { window_minutes: 300, utilization: 91.0, resets_at: _iso(-1 * HOUR) }, extra);

  context.renderExtUsage({ providers: { 'claude:main': stalePayload([staleWindow()]) } });
  let row = _rowByKey(strip, 'claude:main');
  assert.equal(_field(row, '5h-pct').textContent, '91%', 'no key: the stale reading still renders its number');
  const liveSummary = _walk(strip.children[0], _byAttr('data-summary-account', 'claude:main'));
  assert.equal(_field(liveSummary, 'summary-pct').textContent, '91%');

  context.renderExtUsage({ providers: { 'claude:main': stalePayload([staleWindow({ expired: true })]) } });
  row = _rowByKey(strip, 'claude:main');
  assert.equal(_field(row, '5h-pct').textContent, '\u2014');
  assert.equal(_field(row, '5h-reset').textContent, 'window reset \u2014 reading expired');
  assert.ok(_field(row, '5h-bar').classList.contains('bg-slate-600'));
  const expiredSummary = _walk(strip.children[0], _byAttr('data-summary-account', 'claude:main'));
  assert.equal(_field(expiredSummary, 'summary-pct').textContent, '\u2014',
    'the expired reading drops out of the collapsed summary');
});

test('renderExtUsage shows reading age and spend only on rows that carry them', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'codex:main': _codexPayload({ token_count_observed_at: _iso(-5 * DAY) }),
    },
  });

  assert.equal(_field(_rowByKey(strip, 'codex:main'), 'as-of').textContent, 'as of 5d ago');
  assert.equal(_field(_rowByKey(strip, 'claude:main'), 'as-of'), null, 'live Claude query carries no age');
  assert.equal(_field(_rowByKey(strip, 'claude:main'), 'spend-24h'), null, 'Claude payload carries no spend');
});

test('renderExtUsage formats Codex spend values and dashes a null reading', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({ spend: { last_24h_usd: 4.85, last_7d_usd: 13.2 } }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, 'spend-24h').textContent, '$4.85');
  assert.equal(_field(row, 'spend-7d').textContent, '$13.20');

  context.renderExtUsage({ providers: { 'codex:main': _codexPayload() } });

  const refreshed = _rowByKey(strip, 'codex:main');
  assert.equal(_field(refreshed, 'spend-24h').textContent, '\u2014');
  assert.equal(_field(refreshed, 'spend-7d').textContent, '\u2014');
});

test('renderExtUsage clears the Codex business/unlimited state when caps return', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({ windows: [], state_label: 'business / unlimited' }),
    },
  });

  assert.ok(_field(_rowByKey(strip, 'codex:main'), 'state'));

  context.renderExtUsage({ providers: { 'codex:main': _codexPayload() } });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, 'state'), null);
  assert.equal(_field(row, 'no-cap'), null);
  assert.equal(_field(row, '7d-pct').textContent, '96%');
});

test('renderExtUsage renders error rows greyed with the error text', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:invite-1': {
        provider: 'claude',
        provider_label: 'Claude',
        account: 'invite-1',
        error: 'credentials not found',
      },
    },
  });

  const row = _rowByKey(strip, 'claude:invite-1');
  assert.ok(row);
  assert.equal(row.classList.contains('opacity-60'), false, 'the provider pill keeps full contrast');
  // The muted words keep their readability: the note inherits slate-300, which
  // clears 4.5:1 at opacity-60, while a dimmed tinted pill would not.
  assert.ok(row.classList.contains('text-slate-300'), 'status words stay above 4.5:1');
  assert.equal(_field(row, 'error').classList.contains('opacity-60'), true, 'the words stay muted');
  assert.equal(_field(row, 'error').textContent, 'credentials not found');
  const errPill = row.children.find((c) => c.classList.contains('provider-pill'));
  assert.ok(errPill, 'error row carries a provider pill');
  assert.equal(errPill.textContent, 'Claude');
});

test('renderExtUsage renders a provider pill on every account row', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload({ account: 'main' }),
      'claude:invite-1': _claudePayload({
        account: 'invite-1',
        windows: [{ window_minutes: 300, utilization: 7.0, resets_at: '' }],
      }),
      'codex:main': _codexPayload(),
    },
  });

  // The old strip-level group-label spans and \u2502 separators are still gone.
  // Apart from the collapse summary row (data-field="summary"), which leads
  // the strip, every strip child is an account row carrying a data-key.
  assert.ok(strip.children.length > 0, 'strip rendered rows');
  assert.equal(strip.children[0].getAttribute('data-field'), 'summary',
    'the summary row leads the strip');
  const detailRows = strip.children.filter((c) => c.getAttribute('data-field') !== 'summary');
  assert.ok(detailRows.length > 0, 'detail rows rendered');
  for (const child of detailRows) {
    assert.ok(child.getAttribute('data-key'), 'strip detail row is an account row with a data-key');
  }

  const expected = {
    'claude:main': {label: 'Claude', cls: 'provider-claude'},
    'claude:invite-1': {label: 'Claude', cls: 'provider-claude'},
    'codex:main': {label: 'Codex', cls: 'provider-codex'},
  };
  for (const [key, want] of Object.entries(expected)) {
    const row = _rowByKey(strip, key);
    assert.ok(row, key + ' row rendered');
    const pill = row.children.find((c) => c.classList.contains('provider-pill'));
    assert.ok(pill, key + ' row has a provider pill');
    assert.equal(pill.textContent, want.label);
    assert.ok(pill.classList.contains(want.cls), key + ' pill has ' + want.cls);
  }
});

// A rule that keys off an attribute the renderer no longer sets is dead CSS that
// no rendering test can see. Cross-check the two against each other.
test('renderExtUsage renders a not-yet-read account as loading, never as no-cap', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'codex:personal': { provider: 'codex', provider_label: 'Codex', account: 'personal', pending: true },
    },
  });

  const row = _rowByKey(strip, 'codex:personal');
  assert.ok(row, 'pending row rendered');
  assert.equal(row.classList.contains('opacity-60'), false, 'the provider pill keeps full contrast');
  assert.equal(_field(row, 'pending').classList.contains('opacity-60'), true, 'the words stay muted');
  assert.equal(_field(row, 'pending').textContent, 'loading');
  // The dangerous wrong answer: an empty windows list on the quota path renders
  // "plan / no cap", which would claim an uncapped plan for an unread account.
  assert.equal(_field(row, 'no-cap'), null, 'pending row must not claim no cap');
  assert.equal(_field(row, 'spend-7d'), null, 'pending row invents no spend figures');
  const pill = row.children.find((c) => c.classList.contains('provider-pill'));
  assert.ok(pill, 'pending row carries a provider pill');
  assert.equal(pill.textContent, 'Codex');
});

// ---------------------------------------------------------------------------
// Scoped (per-model) weekly windows: one more 7d bar per row, labelled by the
// model. The scope_label rides in the window so both weekly bars coexist, and
// the group div holding the scoped bar is matched by the narrow-screen rule
// `div:has(> [data-field^="7d-"])`.
// ---------------------------------------------------------------------------

test('renderExtUsage binds each scoped reading to its own label and percent', () => {
  const { context, strip } = loadExtUsageScript();

  // Distinct percents per window, and a model name (Nimbus) absent from the
  // source, so a swapped or hardcoded implementation cannot fake this output.
  context.renderExtUsage({
    providers: {
      'claude:main': {
        provider: 'claude',
        provider_label: 'Claude',
        account: 'main',
        windows: [
          { window_minutes: 10080, utilization: 22.0, resets_at: _iso(3 * DAY) },
          { window_minutes: 10080, utilization: 33.0, resets_at: _iso(3 * DAY), scope_label: 'Nimbus' },
        ],
        fetched_at: _iso(-1 * MINUTE),
      },
    },
  });

  const row = _rowByKey(strip, 'claude:main');
  assert.equal(_field(row, '7d-pct').textContent, '22%');
  assert.equal(_field(row, '7d-nimbus-pct').textContent, '33%');
});

test('every data-field within a scoped row is unique', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': scopedClaudeFixture(),
    },
  });

  const row = _rowByKey(strip, 'claude:main');
  const fields = [];
  const collect = (node) => {
    const field = node.getAttribute('data-field');
    if (field) fields.push(field);
    for (const child of node.children) collect(child);
  };
  collect(row);
  assert.equal(new Set(fields).size, fields.length,
      'data-fields must be unique within a row: ' + fields.join(', '));
});

function scopedClaudeFixture() {
  return {
    provider: 'claude',
    provider_label: 'Claude',
    account: 'main',
    windows: [
      { window_minutes: 300, utilization: 42.0, resets_at: _iso(2 * HOUR) },
      { window_minutes: 10080, utilization: 10.0, resets_at: _iso(3 * DAY) },
      { window_minutes: 10080, utilization: 51.0, resets_at: _iso(3 * DAY), scope_label: 'Fable' },
    ],
    fetched_at: _iso(-1 * MINUTE),
  };
}

test('renderExtUsage scoped bucket carries no reset element', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload({
        windows: [
          { window_minutes: 10080, utilization: 10.0, resets_at: _iso(3 * DAY) },
          { window_minutes: 10080, utilization: 51.0, resets_at: _iso(3 * DAY), scope_label: 'Fable' },
        ],
      }),
    },
  });

  const row = _rowByKey(strip, 'claude:main');
  assert.equal(_field(row, '7d-fable-reset'), null, 'scoped bucket renders no -reset element');
  assert.ok(_field(row, '7d-reset'), 'unscoped bucket keeps its reset element');
  assert.ok(_field(row, '7d-fable-bar'), 'scoped bucket renders its bar');
});

test('scoped and plan-wide weekly bars share the threshold colour function', () => {
  const { context, strip } = loadExtUsageScript();
  // Which colour 85% maps to is _barColor's business, not this test's; only that
  // both bars land on the same one, given equal percents, matters here.
  const pct = 85.0;

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload({
        windows: [
          { window_minutes: 10080, utilization: pct, resets_at: _iso(3 * DAY) },
          { window_minutes: 10080, utilization: pct, resets_at: _iso(3 * DAY), scope_label: 'Fable' },
        ],
      }),
    },
  });

  const row = _rowByKey(strip, 'claude:main');
  const plan = _field(row, '7d-bar');
  const scoped = _field(row, '7d-fable-bar');
  assert.ok(plan && scoped, 'both weekly bars rendered');

  const THRESHOLD_CLASSES = ['bg-red-500', 'bg-yellow-500', 'bg-emerald-500'];
  const planColor = THRESHOLD_CLASSES.find((c) => plan.classList.contains(c));
  const scopedColor = THRESHOLD_CLASSES.find((c) => scoped.classList.contains(c));
  assert.ok(planColor, 'plan-wide bar carries one of the known threshold colours');
  assert.equal(scopedColor, planColor,
    'scoped bar shares the same threshold colour as the plan-wide bar for an equal percent');
});

// The existing selector-coverage test above only proves *some* emitted field
// matches the 7d- rule. It does not use a scoped fixture, so it cannot tell
// whether the scoped bucket's own field is one of the fields that rule sweeps
// up. Assert that directly, so the narrow-screen coupling covers the scoped
// bar too, not just the plan-wide one it happens to share a row with.
// ---------------------------------------------------------------------------
// Quota-strip collapse. The storage key literal is deliberately duplicated
// here — the key name is the persistence contract and a silent rename must
// fail these tests. In the fake DOM there is no CSS engine, so "visibility"
// is asserted as the class contract the two collapse rules implement. The
// rules' presence in usage.css is pinned separately.
// ---------------------------------------------------------------------------
const COLLAPSE_STORAGE_KEY = 'ext_usage_strip_collapsed_v1';
test('collapsed default is the summary row at every width, a stored choice wins, never written back', () => {
  const cases = [
    { name: 'missing key, mobile', initial: {}, isMobile: true, want: true },
    { name: 'missing key, desktop', initial: {}, isMobile: false, want: true },
    { name: 'missing key, no platform', initial: {}, isMobile: undefined, want: true },
    { name: 'invalid value, mobile', initial: { [COLLAPSE_STORAGE_KEY]: 'definitely-not-a-flag' }, isMobile: true, want: true },
    { name: 'invalid value, desktop', initial: { [COLLAPSE_STORAGE_KEY]: 'definitely-not-a-flag' },
      isMobile: false, want: true },
    { name: 'stored collapsed beats desktop', initial: { [COLLAPSE_STORAGE_KEY]: '1' }, isMobile: false, want: true },
    { name: 'stored expanded beats desktop', initial: { [COLLAPSE_STORAGE_KEY]: '0' }, isMobile: false, want: false },
    { name: 'stored expanded beats mobile', initial: { [COLLAPSE_STORAGE_KEY]: '0' }, isMobile: true, want: false },
  ];
  for (const c of cases) {
    const storage = makeStorage(c.initial);
    const options = { storage };
    if (c.isMobile !== undefined) options.platform = { isMobile: c.isMobile };
    const harness = loadExtUsageScript(options);
    harness.fireDOMContentLoaded();

    assert.equal(harness.isCollapsed(), c.want, c.name + ': collapsed boolean');
    assert.equal(storage.calls.set, 0, c.name + ': evaluating the default performs no writes');
    assert.equal(storage.getItem(COLLAPSE_STORAGE_KEY), c.initial[COLLAPSE_STORAGE_KEY] ?? null,
        c.name + ': an absent or invalid preference is never written back');

    assert.equal(harness.chip.classList.contains('hidden'), true,
        c.name + ': chip starts hidden with the strip');
    _click(harness.chip);
    assert.equal(harness.isCollapsed(), !c.want, c.name + ': explicit toggle flips the boolean');
    assert.equal(storage.getItem(COLLAPSE_STORAGE_KEY), (!c.want) ? '1' : '0',
        c.name + ': explicit toggle writes the new value');
  }
});

test('a module that lands after the document starts the strip without a page event', () => {
  const storage = makeStorage({ [COLLAPSE_STORAGE_KEY]: '1' });
  const harness = loadExtUsageScript({ readyState: 'complete', storage });

  assert.equal(harness.isCollapsed(), true, 'the stored choice applies as the module lands');
  assert.equal(storage.calls.get, 1, 'the module reads the stored choice once');
  assert.equal(storage.calls.set, 0, 'evaluating the default performs no writes');
});

test('repeated renders are structurally identical and click handlers never accumulate', () => {
  const harness = loadExtUsageScript({ platform: { isMobile: false } });
  const { context, strip, chip } = harness;
  harness.fireDOMContentLoaded();
  const payload = {
    providers: {
      'claude:main': scopedClaudeFixture(),
      'codex:main': _codexPayload(),
    },
  };

  // Time-derived labels (countdowns, clock windows, as-of ages) legitimately
  // drift between two renders — the 60s repaint exists for them — so the
  // structural signature normalizes them out. Everything else must be stable.
  const stableText = (text) => String(text)
      .replace(/^resets in .+$/, '<countdown>')
      .replace(/^as of .+ ago$/, '<age>')
      .replace(/^\(\d{2}:\d{2}:\d{2} .+\)$/, '<clock window>');
  const signature = (node) => ({
    tag: node.tagName,
    cls: node.className,
    text: stableText(node.textContent),
    field: node.getAttribute('data-field'),
    key: node.getAttribute('data-key'),
    acct: node.getAttribute('data-summary-account'),
    children: node.children.map(signature),
  });

  context.renderExtUsage(payload);
  const first = signature(strip);
  context.renderExtUsage(payload);
  assert.deepEqual(signature(strip), first, 'same payload renders the same structure twice');

  const summary = strip.children[0];
  assert.equal(summary._listeners.click.length, 1, 'fresh summary row holds exactly one listener');
  assert.equal(chip._listeners.click.length, 1, 're-rendering never re-registers the chip listener');

  const rowsBefore = [...strip.children];
  const collapsedBefore = harness.isCollapsed();
  assert.equal(_click(chip), 1, 'exactly one chip handler answers a click');
  assert.equal(harness.isCollapsed(), !collapsedBefore, 'one click flips the boolean exactly once');
  assert.equal(strip.children.length, rowsBefore.length, 'toggle rebuilds no children');
  rowsBefore.forEach((row, i) => {
    assert.ok(strip.children[i] === row, 'toggle keeps row element identity (child ' + i + ')');
  });
});

test('a refusing localStorage degrades to the in-memory boolean', () => {
  const harness = loadExtUsageScript({ storage: makeThrowingStorage(), platform: { isMobile: true } });
  const { context, strip, chip } = harness;

  assert.doesNotThrow(() => harness.fireDOMContentLoaded(), 'bootstrap survives a throwing storage');
  assert.equal(harness.isCollapsed(), true, 'the default survives the failed read');

  context.renderExtUsage({ providers: { 'claude:main': _claudePayload() } });
  assert.equal(strip.children.length, 2, 'rendering is unaffected by storage failures');

  const collapsedBefore = harness.isCollapsed();
  assert.doesNotThrow(() => _click(chip), 'toggle survives a throwing storage');
  assert.equal(harness.isCollapsed(), !collapsedBefore, 'toggle still flips the in-memory boolean');
  assert.equal(strip.classList.contains('collapsed'), !collapsedBefore, 'strip class still follows');
  assert.equal(chip.textContent, (!collapsedBefore) ? '▸' : '▾', 'chip glyph still follows');
});

test('renderExtUsage marks a pool account that needs a new login with its directory', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'claude:ext-2': _claudePayload({ account: 'ext-2', login_required: '~/.claude-ext-2' }),
    },
  });

  const flagged = _rowByKey(strip, 'claude:ext-2');
  assert.ok(flagged);
  assert.equal(_field(flagged, 'login-required').textContent, 're-login needed: ~/.claude-ext-2');
  const healthy = _rowByKey(strip, 'claude:main');
  assert.equal(_field(healthy, 'login-required'), null, 'a healthy account carries no login note');
});

test('a quota group error is shown beside the retained reading', () => {
  const { context, strip } = loadExtUsageScript();
  context.renderExtUsage({ providers: { 'antigravity:main': {
    provider: 'antigravity', provider_label: 'Antigravity', account: 'main', windows: [],
    status_rows: [{ name: 'Antigravity', state: 'error', message: 'model matched no quota group' }],
  } } });
  const row = _rowByKey(strip, 'antigravity:main:status:0');
  assert.equal(_field(row, 'member-state').textContent, 'model matched no quota group');
});
