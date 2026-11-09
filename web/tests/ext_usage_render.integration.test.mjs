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

// Metered credits: the payload's credits reading (unlimited=false + balance)
// replaces the no-cap marker on an empty-windows row and reports itself in the
// collapsed summary as "cr <balance>". The real wire balance is a fractional
// decimal; the UI rounds to the unit and groups thousands.
// Sampled 9 days ago, the 7d window has certainly rolled over; the server's
// annotation, not the browser's clock math, expires it.
// The expiry judgement for claude is the server's: one frozen reading renders
// live or expired exactly when the emitted window carries `expired: true`.
// Neither fetched_at age alone nor the browser's own clock math expires it.
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

  const css = _readUsageCss();
  const selectors = [...css.matchAll(/#ext-usage-strip[^{}]*?\[([a-z-]+)([$^*]?=)"([^"]+)"\]/g)];
  assert.ok(selectors.length > 0, 'usage.css scopes attribute selectors to #ext-usage-strip');

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
// ---------------------------------------------------------------------------
// Scoped (per-model) weekly windows: one more 7d bar per row, labelled by the
// model. The scope_label rides in the window so both weekly bars coexist, and
// the group div holding the scoped bar is matched by the narrow-screen rule
// `div:has(> [data-field^="7d-"])`.
// ---------------------------------------------------------------------------

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

// The existing selector-coverage test above only proves *some* emitted field
// matches the 7d- rule. It does not use a scoped fixture, so it cannot tell
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

  const css = _readUsageCss();
  assert.ok(css.includes('[data-field^="7d-"]'),
      'usage.css must still key the narrow-screen 7d group off the data-field prefix');

  const scopedFields = [...emitted].filter((f) => f.startsWith('7d-') && f !== '7d-bar' && f !== '7d-pct' && f !== '7d-reset');
  assert.ok(scopedFields.length > 0,
      'scoped weekly bucket must emit a field the ^="7d-" rule also matches: ' + [...emitted].join(', '));
  assert.ok(!emitted.has('spend-7d'), 'the 7d- rule must not sweep up the codex spend column');
});

// ---------------------------------------------------------------------------
// Quota-strip collapse. The storage key literal is deliberately duplicated
// here — the key name is the persistence contract and a silent rename must
// fail these tests. In the fake DOM there is no CSS engine, so "visibility"
// is asserted as the class contract the two collapse rules implement. The
// rules' presence in usage.css is pinned separately.
// ---------------------------------------------------------------------------
const COLLAPSE_STORAGE_KEY = 'ext_usage_strip_collapsed_v1';
const COLLAPSED_CSS_RULE = '#ext-usage-strip.collapsed [data-key]';
const EXPANDED_CSS_RULE = '#ext-usage-strip:not(.collapsed) [data-field="summary"]';

function _readUsageCss() {
  return fs.readFileSync(
      path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../features/usage/templates/usage/usage.css'), 'utf8');
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
    // (strip classes, row kind) to visibility. Check that the mapping agrees
    // with the visibility the in-memory boolean demands.
    const stripHidden = strip.classList.contains('hidden');
    const visiblePerRules = !stripHidden && (strip.classList.contains('collapsed') ? isSummary : !isSummary);
    const visiblePerBoolean = !stripHidden && (expect.collapsed ? isSummary : !isSummary);
    assert.equal(visiblePerRules, visiblePerBoolean,
        label + ': child ' + i + ' visibility under the CSS rules matches the boolean');
  });
}

test('collapse state stays internally consistent across render and toggle sequences', () => {
  // Pin the two CSS rules the class contract stands for.
  const css = _readUsageCss();
  assert.ok(css.includes(COLLAPSED_CSS_RULE), 'usage.css hides detail rows when collapsed');
  assert.ok(css.includes(EXPANDED_CSS_RULE), 'usage.css hides the summary row when expanded');

  const harness = loadExtUsageScript({ platform: { isMobile: false } });
  const { context, strip } = harness;
  harness.fireDOMContentLoaded();

  const payloads = [
    ['scoped windows', { providers: { 'claude:main': scopedClaudeFixture(), 'codex:main': _codexPayload() } }],
    ['error account', { providers: { 'claude:invite-1': { provider: 'claude', provider_label: 'Claude', account: 'invite-1', error: 'credentials not found' } } }],
    ['pending account', { providers: { 'codex:personal': { provider: 'codex', provider_label: 'Codex', account: 'personal', pending: true } } }],
    ['no-cap account', { providers: { 'codex:main': _codexPayload({ windows: [], state_label: 'business / unlimited' }) } }],
    ['expired reading', { providers: { 'codex:main': _codexPayload({
      token_count_observed_at: _iso(-9 * DAY),
      windows: [{ window_minutes: 10080, utilization: 96.0, resets_at: _iso(2 * DAY), expired: true }],
    }) } }],
    ['unknown utilization', { providers: { 'codex:main': _codexPayload({
      windows: [{ window_minutes: 10080, utilization: null, resets_at: _iso(2 * DAY) }],
    }) } }],
  ];

  // Track the expected truth outside the implementation: starts collapsed
  // (the default at every width, key absent), each chip click flips it and writes.
  const expect = { collapsed: true, stored: null, writes: 0, providers: 0 };

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
    // Sampled 2h ago: the server marked the window whose reset already passed
    // while the sample predates it; the live 12% must win over the expired 96%.
    'codex:rolled': _codexPayload({
      account: 'rolled',
      token_count_observed_at: _iso(-2 * HOUR),
      windows: [
        { window_minutes: 10080, utilization: 96.0, resets_at: _iso(-1 * HOUR), expired: true },
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
      windows: [{ window_minutes: 10080, utilization: 96.0, resets_at: _iso(2 * DAY), expired: true }],
    }),
    'codex:unlimited': _codexPayload({ account: 'unlimited', windows: [], state_label: 'business / unlimited' }),
    'codex:loading': { provider: 'codex', provider_label: 'Codex', account: 'loading', pending: true },
    'claude:broken': { provider: 'claude', provider_label: 'Claude', account: 'broken', error: 'credentials not found' },
  };
  context.renderExtUsage({ providers: accounts });

  const summary = strip.children[0];
  assert.equal(summary.getAttribute('data-field'), 'summary', 'summary row leads the strip');
  const lastSegment = summary.children[summary.children.length - 1];
  assert.equal(lastSegment.getAttribute('data-summary-account'), 'claude:broken',
      'summary row ends at its last account reading');
  assert.equal(_field(summary, 'summary-chevron'), null,
      'summary row draws no second disclosure arrow beside the floating chip');

  // The threshold hues are _barColor's business; only the bg→text sibling
  // mapping is restated here. The text classes must come from the committed
  // stylesheet (which carries no plain text-emerald-400), so pin that too.
  const TEXT_BY_BAR_COLOR = {
    'bg-red-500': 'text-red-400',
    'bg-yellow-500': 'text-yellow-400',
    'bg-emerald-500': 'text-green-400',
  };
  const committedCss = fs.readFileSync(
      path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../static/css/tailwind.css'), 'utf8');
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
      const live = providerData.windows.filter((win) => {
        if (typeof win.utilization !== 'number' || !Number.isFinite(win.utilization)) return false;
        return win.expired !== true;
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
      assert.ok(pctEl.classList.contains('text-slate-400'), key + ': special state reads at 6:1, not 3:1');
    } else {
      const wantColor = TEXT_BY_BAR_COLOR[context._barColor(wantPct)];
      assert.ok(wantColor, key + ': _barColor(' + wantPct + ') has a text sibling');
      assert.ok(pctEl.classList.contains(wantColor),
          key + ': summary colour tracks _barColor(' + wantPct + ')');
    }
  }
});

