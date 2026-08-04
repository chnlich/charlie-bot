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

function loadExtUsageScript() {
  const strip = new FakeElement('div', 'hidden flex items-center gap-4');
  const document = {
    addEventListener() {},
    getElementById(id) {
      return id === 'ext-usage-strip' ? strip : null;
    },
    createElement(tag) {
      return new FakeElement(tag);
    },
  };
  const context = {
    console,
    Date,
    document,
    fetch() {
      throw new Error('fetch should not run during unit tests');
    },
    setInterval() {
      return 0;
    },
  };
  context.window = context;

  const scriptPath = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../web/static/js/ext_usage.js');
  const scriptSource = fs.readFileSync(scriptPath, 'utf8');
  vm.createContext(context);
  vm.runInContext(scriptSource, context, { filename: scriptPath });

  return { context, strip };
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

  // The old strip-level group-label spans and \u2502 separators are gone: every
  // strip child is now an account row carrying a data-key attribute.
  for (const child of strip.children) {
    assert.ok(child.getAttribute('data-key'), 'strip child is an account row with a data-key');
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

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload({
        windows: [
          { window_minutes: 10080, utilization: 85.0, resets_at: _iso(3 * DAY) },
          { window_minutes: 10080, utilization: 85.0, resets_at: _iso(3 * DAY), scope_label: 'Fable' },
        ],
      }),
    },
  });

  const row = _rowByKey(strip, 'claude:main');
  const plan = _field(row, '7d-bar');
  const scoped = _field(row, '7d-fable-bar');
  assert.ok(plan && scoped, 'both weekly bars rendered');
  // Both carry the same colour, an existing threshold class, not a made-up one.
  assert.ok(plan.classList.contains('bg-red-500'),
    'a >80% plan-wide bar is red via the shared threshold function');
  assert.equal(scoped.classList.contains('bg-red-500'), plan.classList.contains('bg-red-500'),
    'scoped bar coloured by the same threshold function as the plan-wide bar');
});
