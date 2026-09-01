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
// platform-derived default is never written back.
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
  };
  context.window = context;
  if (options.platform !== undefined) {
    context.platform = options.platform;
  }

  const scriptPath = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../web/static/js/ext_usage.js');
  const scriptSource = fs.readFileSync(scriptPath, 'utf8');
  const pageTimersPath = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../web/static/js/page-timers.js');
  const pageTimersSource = fs.readFileSync(pageTimersPath, 'utf8');
  vm.createContext(context);
  // page-timers.js loads first, matching the script order in index.html — the
  // strip's refresh timers register through it.
  vm.runInContext(pageTimersSource, context, { filename: pageTimersPath });
  // Bridge the lexical module binding (same pattern as the backlogPanel bridge
  // in tailwind_class_coverage.test.js) so tests can read the one in-memory
  // collapsed boolean rather than inferring it from DOM state.
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
    account: 'main',
    windows: [{ window_minutes: 10080, utilization: 96.0, resets_at: _iso(2 * DAY) }],
    fetched_at: _iso(-1 * MINUTE),
    token_count_observed_at: _iso(-2 * MINUTE),
    ...overrides,
  };
}

test('renderExtUsage shows business/unlimited Codex state with no quota bars', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'codex:main': _codexPayload({ windows: [], rate_limits_state: 'business-unlimited' }),
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

test('renderExtUsage marks a reading older than its window as expired', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        // Sampled 9 days ago: the 7d window has certainly rolled over since.
        token_count_observed_at: _iso(-9 * DAY),
        windows: [{ window_minutes: 10080, utilization: 96.0, resets_at: _iso(2 * DAY) }],
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, '7d-pct').textContent, '\u2014');
  assert.equal(_field(row, '7d-reset').textContent, 'window reset \u2014 reading expired');
  assert.ok(_field(row, '7d-bar').classList.contains('bg-slate-600'));
});

test('renderExtUsage shows reading age on Codex rows only', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'codex:main': _codexPayload({ token_count_observed_at: _iso(-5 * DAY) }),
    },
  });

  assert.equal(_field(_rowByKey(strip, 'codex:main'), 'as-of').textContent, 'as of 5d ago');
  assert.equal(_field(_rowByKey(strip, 'claude:main'), 'as-of'), null, 'live Claude query carries no age');
});

test('renderExtUsage formats Codex spend values and dashes when absent', () => {
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
      'codex:main': _codexPayload({ windows: [], rate_limits_state: 'business-unlimited' }),
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
        account: 'invite-1',
        error: 'credentials not found',
      },
    },
  });

  const row = _rowByKey(strip, 'claude:invite-1');
  assert.ok(row);
  assert.equal(row.classList.contains('opacity-60'), true);
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

  // The old strip-level group-label spans and \u2502 separators are still gone:
  // apart from the collapse summary row (data-field="summary"), which leads
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
test('mobile CSS selectors for the strip match the attributes the renderer emits', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'codex:main': _codexPayload(),
    },
  });

  const emitted = new Set();
  const collect = (node) => {
    const field = node.getAttribute('data-field');
    if (field) emitted.add(field);
    for (const child of node.children) collect(child);
  };
  collect(strip);
  assert.ok(emitted.size > 0, 'renderer emitted data-field attributes');

  const cssPath = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../web/static/css/styles.css');
  const css = fs.readFileSync(cssPath, 'utf8');
  const selectors = [...css.matchAll(/#ext-usage-strip[^{}]*?\[([a-z-]+)([$^*]?=)"([^"]+)"\]/g)];
  assert.ok(selectors.length > 0, 'styles.css scopes attribute selectors to #ext-usage-strip');

  for (const [whole, attr, op, value] of selectors) {
    assert.equal(attr, 'data-field',
      `#ext-usage-strip rules must key off data-field, not ${attr}: the renderer sets no other identifying attribute (selector: ${whole})`);
    const matches = [...emitted].filter((f) => (
      op === '$=' ? f.endsWith(value)
        : op === '^=' ? f.startsWith(value)
          : op === '*=' ? f.includes(value)
            : f === value));
    assert.ok(matches.length > 0,
      `selector ${whole} matches none of the emitted data-field values: ${[...emitted].sort().join(', ')}`);
    if (op === '^=' && value === '7d-') {
      assert.ok(!matches.includes('spend-7d'),
        'the 7d-bucket rule must not sweep up the codex spend column');
    }
  }
});

test('renderExtUsage renders a not-yet-read account as loading, never as no-cap', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'codex:personal': { provider: 'codex', account: 'personal', pending: true },
    },
  });

  const row = _rowByKey(strip, 'codex:personal');
  assert.ok(row, 'pending row rendered');
  assert.equal(row.classList.contains('opacity-60'), true);
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
// matches the 7d- rule; it does not use a scoped fixture, so it cannot tell
// whether the scoped bucket's own field is one of the fields that rule sweeps
// up. Assert that directly, so the narrow-screen coupling covers the scoped
// bar too, not just the plan-wide one it happens to share a row with.
test('the 7d- narrow-screen rule also sweeps up the scoped weekly bucket', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': scopedClaudeFixture(),
    },
  });

  const emitted = new Set();
  const collect = (node) => {
    const field = node.getAttribute('data-field');
    if (field) emitted.add(field);
    for (const child of node.children) collect(child);
  };
  collect(strip);

  const cssPath = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../web/static/css/styles.css');
  const css = fs.readFileSync(cssPath, 'utf8');
  assert.ok(css.includes('[data-field^="7d-"]'),
      'styles.css:395 must still key the narrow-screen 7d group off the data-field prefix');

  const scopedFields = [...emitted].filter((f) => f.startsWith('7d-') && f !== '7d-bar' && f !== '7d-pct' && f !== '7d-reset');
  assert.ok(scopedFields.length > 0,
      'scoped weekly bucket must emit a field the ^="7d-" rule also matches: ' + [...emitted].join(', '));
  assert.ok(!emitted.has('spend-7d'), 'the 7d- rule must not sweep up the codex spend column');
});

// ---------------------------------------------------------------------------
// Quota-strip collapse. The storage key literal is deliberately duplicated
// here — the key name is the persistence contract and a silent rename must
// fail these tests. In the fake DOM there is no CSS engine, so "visibility"
// is asserted as the class contract the two collapse rules implement; the
// rules' presence in styles.css is pinned separately.
// ---------------------------------------------------------------------------
const COLLAPSE_STORAGE_KEY = 'ext_usage_strip_collapsed_v1';
const COLLAPSED_CSS_RULE = '#ext-usage-strip.collapsed [data-key]';
const EXPANDED_CSS_RULE = '#ext-usage-strip:not(.collapsed) [data-field="summary"]';

function _readStylesCss() {
  return fs.readFileSync(
      path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../web/static/css/styles.css'), 'utf8');
}

// The in-memory boolean, the stored preference, the strip class, row
// visibility, and the chip must agree after every single step.
function _assertCollapseConsistent(harness, expect, label) {
  const { strip, chip, storage } = harness;
  assert.equal(harness.isCollapsed(), expect.collapsed, label + ': in-memory boolean');
  assert.equal(storage.getItem(COLLAPSE_STORAGE_KEY), expect.stored, label + ': stored value');
  assert.equal(storage.calls.set, expect.writes, label + ': only user toggles write storage');
  assert.equal(strip.classList.contains('collapsed'), expect.collapsed,
      label + ': strip collapsed class mirrors the boolean');
  assert.equal(chip.classList.contains('hidden'), strip.classList.contains('hidden'),
      label + ': chip hidden exactly when the strip is');
  assert.equal(chip.textContent, expect.collapsed ? '▸' : '▾', label + ': chip glyph');

  if (expect.providers === 0) {
    assert.equal(strip.children.length, 0, label + ': no providers renders no rows');
    return;
  }
  assert.equal(strip.children.length, expect.providers + 1,
      label + ': summary row plus one detail row per account');
  strip.children.forEach((child, i) => {
    const isSummary = i === 0;
    assert.equal(child.getAttribute('data-field') === 'summary', isSummary,
        label + ': child ' + i + ' is the summary row iff it leads');
    assert.equal(child.getAttribute('data-key') !== null, !isSummary,
        label + ': child ' + i + ' carries data-key iff it is a detail row');
    // No CSS engine here, so restate the two pinned rules as a mapping from
    // (strip classes, row kind) to visibility and check it agrees with the
    // visibility the in-memory boolean demands.
    const stripHidden = strip.classList.contains('hidden');
    const visiblePerRules = !stripHidden && (strip.classList.contains('collapsed') ? isSummary : !isSummary);
    const visiblePerBoolean = !stripHidden && (expect.collapsed ? isSummary : !isSummary);
    assert.equal(visiblePerRules, visiblePerBoolean,
        label + ': child ' + i + ' visibility under the CSS rules matches the boolean');
  });
}

test('collapse state stays internally consistent across render and toggle sequences', () => {
  // Pin the two CSS rules the class contract stands for.
  const css = _readStylesCss();
  assert.ok(css.includes(COLLAPSED_CSS_RULE), 'styles.css hides detail rows when collapsed');
  assert.ok(css.includes(EXPANDED_CSS_RULE), 'styles.css hides the summary row when expanded');

  const harness = loadExtUsageScript({ platform: { isMobile: false } });
  const { context, strip } = harness;
  harness.fireDOMContentLoaded();

  const payloads = [
    ['scoped windows', { providers: { 'claude:main': scopedClaudeFixture(), 'codex:main': _codexPayload() } }],
    ['error account', { providers: { 'claude:invite-1': { provider: 'claude', account: 'invite-1', error: 'credentials not found' } } }],
    ['pending account', { providers: { 'codex:personal': { provider: 'codex', account: 'personal', pending: true } } }],
    ['no-cap account', { providers: { 'codex:main': _codexPayload({ windows: [], rate_limits_state: 'business-unlimited' }) } }],
    ['expired reading', { providers: { 'codex:main': _codexPayload({
      token_count_observed_at: _iso(-9 * DAY),
      windows: [{ window_minutes: 10080, utilization: 96.0, resets_at: _iso(2 * DAY) }],
    }) } }],
    ['unknown utilization', { providers: { 'codex:main': _codexPayload({
      windows: [{ window_minutes: 10080, utilization: null, resets_at: _iso(2 * DAY) }],
    }) } }],
  ];

  // Track the expected truth outside the implementation: starts expanded
  // (desktop default, key absent), each chip click flips it and writes.
  const expect = { collapsed: false, stored: null, writes: 0, providers: 0 };

  for (const [name, data] of payloads) {
    expect.providers = Object.keys(data.providers).length;
    context.renderExtUsage(data);
    _assertCollapseConsistent(harness, expect, 'render ' + name);

    assert.equal(_click(harness.chip), 1, 'chip fires exactly once per click');
    expect.collapsed = !expect.collapsed;
    expect.stored = expect.collapsed ? '1' : '0';
    expect.writes += 1;
    _assertCollapseConsistent(harness, expect, 'chip after ' + name);

    context.renderExtUsage(data);
    _assertCollapseConsistent(harness, expect, 're-render ' + name);
  }

  // An empty payload renders no rows; the strip's hidden class is only ever
  // removed (pre-existing behaviour), so the chip stays put and in sync.
  expect.providers = 0;
  context.renderExtUsage({ providers: {} });
  _assertCollapseConsistent(harness, expect, 'render empty payload');

  // Collapse, then expand by clicking the summary row itself.
  if (!expect.collapsed) {
    assert.equal(_click(harness.chip), 1);
    expect.collapsed = true;
    expect.stored = '1';
    expect.writes += 1;
  }
  expect.providers = Object.keys(payloads[0][1].providers).length;
  context.renderExtUsage(payloads[0][1]);
  _assertCollapseConsistent(harness, expect, 'collapsed before summary click');

  const summaryRow = strip.children[0];
  assert.equal(_click(summaryRow), 1, 'summary row fires exactly once per click');
  expect.collapsed = false;
  expect.stored = '0';
  expect.writes += 1;
  _assertCollapseConsistent(harness, expect, 'summary click expands');

  // Clicking the summary row while expanded is a no-op: no state change, no
  // storage write (the row is CSS-hidden in that state anyway).
  _click(strip.children[0]);
  _assertCollapseConsistent(harness, expect, 'summary click while expanded is inert');
});

test('collapsed default follows storage then platform.isMobile, never written back', () => {
  const cases = [
    { name: 'missing key, mobile', initial: {}, isMobile: true, want: true },
    { name: 'missing key, desktop', initial: {}, isMobile: false, want: false },
    { name: 'missing key, no platform', initial: {}, isMobile: undefined, want: false },
    { name: 'invalid value, mobile', initial: { [COLLAPSE_STORAGE_KEY]: 'definitely-not-a-flag' }, isMobile: true, want: true },
    { name: 'invalid value, no platform', initial: { [COLLAPSE_STORAGE_KEY]: 'definitely-not-a-flag' }, isMobile: undefined, want: false },
    { name: 'stored collapsed beats desktop', initial: { [COLLAPSE_STORAGE_KEY]: '1' }, isMobile: false, want: true },
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

test('summary worst reading is the max live utilization across all reported windows', () => {
  const { context, strip } = loadExtUsageScript();
  const accounts = {
    'claude:main': _claudePayload({
      windows: [
        { window_minutes: 300, utilization: 42.0, resets_at: _iso(2 * HOUR) },
        { window_minutes: 10080, utilization: 10.0, resets_at: _iso(3 * DAY) },
        { window_minutes: 10080, utilization: 51.0, resets_at: _iso(3 * DAY), scope_label: 'Fable' },
      ],
    }),
    'codex:main': _codexPayload(),
    // Sampled 2h ago: the window whose reset already passed while the sample
    // predates it is expired; the live 12% must win over the expired 96%.
    'codex:rolled': _codexPayload({
      account: 'rolled',
      token_count_observed_at: _iso(-2 * HOUR),
      windows: [
        { window_minutes: 10080, utilization: 96.0, resets_at: _iso(-1 * HOUR) },
        { window_minutes: 10080, utilization: 12.0, resets_at: _iso(2 * DAY) },
      ],
    }),
    'codex:unknown': _codexPayload({
      account: 'unknown',
      windows: [{ window_minutes: 10080, utilization: null, resets_at: _iso(2 * DAY) }],
    }),
    'codex:expired': _codexPayload({
      account: 'expired',
      token_count_observed_at: _iso(-9 * DAY),
      windows: [{ window_minutes: 10080, utilization: 96.0, resets_at: _iso(2 * DAY) }],
    }),
    'codex:unlimited': _codexPayload({ account: 'unlimited', windows: [], rate_limits_state: 'business-unlimited' }),
    'codex:loading': { provider: 'codex', account: 'loading', pending: true },
    'claude:broken': { provider: 'claude', account: 'broken', error: 'credentials not found' },
  };
  context.renderExtUsage({ providers: accounts });

  const summary = strip.children[0];
  assert.equal(summary.getAttribute('data-field'), 'summary', 'summary row leads the strip');
  assert.equal(summary.children[summary.children.length - 1].getAttribute('data-field'), 'summary-chevron',
      'summary row ends with its chevron');
  assert.equal(summary.children[summary.children.length - 1].textContent, '▸',
      'summary chevron is the collapsed glyph');

  // The threshold hues are _barColor's business; only the bg→text sibling
  // mapping is restated here. The text classes must come from the committed
  // stylesheet (which carries no plain text-emerald-400), so pin that too.
  const TEXT_BY_BAR_COLOR = {
    'bg-red-500': 'text-red-400',
    'bg-yellow-500': 'text-yellow-400',
    'bg-emerald-500': 'text-green-400',
  };
  const committedCss = fs.readFileSync(
      path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../web/static/css/tailwind.css'), 'utf8');
  for (const textClass of Object.values(TEXT_BY_BAR_COLOR)) {
    assert.ok(committedCss.includes('.' + textClass + ' {'),
        'committed tailwind.css carries .' + textClass + ' — the summary colour must exist in the build');
  }

  for (const [key, providerData] of Object.entries(accounts)) {
    const seg = _walk(summary, _byAttr('data-summary-account', key));
    assert.ok(seg, key + ': summary segment rendered');
    const detailRow = _rowByKey(strip, key);
    const pill = seg.children.find((c) => c.classList.contains('provider-pill'));
    const detailPill = detailRow.children.find((c) => c.classList.contains('provider-pill'));
    assert.ok(pill && detailPill, key + ': both rows carry a provider pill');
    assert.equal(pill.textContent, detailPill.textContent, key + ': summary pill matches the detail pill');
    const pctEl = _field(seg, 'summary-pct');
    assert.ok(pctEl, key + ': summary reading element rendered');

    // Independently recompute the expectation straight from the payload.
    let wantText;
    let wantPct = null;
    if (providerData.pending) {
      wantText = 'loading';
    } else if (providerData.error) {
      wantText = 'error';
    } else if (!Array.isArray(providerData.windows) || providerData.windows.length === 0) {
      wantText = 'no cap';
    } else {
      const sampled = Date.parse(providerData.token_count_observed_at || providerData.fetched_at);
      const live = providerData.windows.filter((win) => {
        if (typeof win.utilization !== 'number' || !Number.isFinite(win.utilization)) return false;
        if (providerData.provider === 'codex' && Number.isFinite(sampled)) {
          const reset = Date.parse(win.resets_at);
          if (Number.isFinite(reset) && reset <= Date.now() && sampled < reset) return false;
          if (Number.isFinite(win.window_minutes) && Date.now() - sampled > win.window_minutes * 60000) return false;
        }
        return true;
      });
      if (live.length === 0) {
        wantText = '—';
      } else {
        wantPct = Math.round(Math.max(...live.map((win) => win.utilization)));
        wantText = wantPct + '%';
      }
    }
    assert.equal(pctEl.textContent, wantText, key + ': summary reading recomputed from the payload');
    if (wantPct === null) {
      assert.ok(pctEl.classList.contains('text-slate-500'), key + ': special state is muted');
    } else {
      const wantColor = TEXT_BY_BAR_COLOR[context._barColor(wantPct)];
      assert.ok(wantColor, key + ': _barColor(' + wantPct + ') has a text sibling');
      assert.ok(pctEl.classList.contains(wantColor),
          key + ': summary colour tracks _barColor(' + wantPct + ')');
    }
  }
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
  // structural signature normalizes them out; everything else must be stable.
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
  assert.equal(harness.isCollapsed(), true, 'platform default survives the failed read');

  context.renderExtUsage({ providers: { 'claude:main': _claudePayload() } });
  assert.equal(strip.children.length, 2, 'rendering is unaffected by storage failures');

  const collapsedBefore = harness.isCollapsed();
  assert.doesNotThrow(() => _click(chip), 'toggle survives a throwing storage');
  assert.equal(harness.isCollapsed(), !collapsedBefore, 'toggle still flips the in-memory boolean');
  assert.equal(strip.classList.contains('collapsed'), !collapsedBefore, 'strip class still follows');
  assert.equal(chip.textContent, (!collapsedBefore) ? '▸' : '▾', 'chip glyph still follows');
});
