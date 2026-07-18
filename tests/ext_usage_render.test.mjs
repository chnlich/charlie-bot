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

function _findAll(root, predicate, acc = []) {
  for (const child of root.children) {
    if (predicate(child)) acc.push(child);
    _findAll(child, predicate, acc);
  }
  return acc;
}

function _byAttr(attr, value) {
  return (el) => el.getAttribute(attr) === value;
}

function _byClass(className) {
  return (el) => el.classList.contains(className);
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

function _claudePayload(overrides = {}) {
  return {
    provider: 'claude',
    account: 'main',
    five_hour: { utilization: 42.0, resets_at: '2026-03-31T10:00:00+00:00' },
    seven_day: { utilization: 10.0, resets_at: '2026-04-03T10:00:00+00:00' },
    fetched_at: '2026-03-31T08:00:00+00:00',
    ...overrides,
  };
}

function _codexPayload(overrides = {}) {
  return {
    provider: 'codex',
    account: 'main',
    five_hour: { utilization: 0.0, resets_at: '' },
    seven_day: { utilization: 0.0, resets_at: '' },
    fetched_at: '2026-03-31T08:00:00+00:00',
    token_count_observed_at: '2026-03-31T07:59:00Z',
    ...overrides,
  };
}

test('renderExtUsage shows business/unlimited Codex state explicitly', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload(),
      'codex:main': _codexPayload({ rate_limits_state: 'business-unlimited' }),
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
  assert.equal(_field(codexRow, '5h-reset').textContent, 'no 5h cap');
  assert.equal(_field(codexRow, '7d-reset').textContent, 'no 7d cap');
});

test('renderExtUsage formats Codex spend values and dashes when absent', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        five_hour: { utilization: 8.0, resets_at: '2030-03-31T10:00:00+00:00' },
        seven_day: { utilization: 2.0, resets_at: '2030-04-02T10:00:00+00:00' },
        fetched_at: '2030-03-31T08:05:00+00:00',
        token_count_observed_at: '2030-03-31T08:04:00Z',
        spend: { last_24h_usd: 4.85, last_7d_usd: 13.2 },
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, 'spend-24h').textContent, '$4.85');
  assert.equal(_field(row, 'spend-7d').textContent, '$13.20');

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        five_hour: { utilization: 8.0, resets_at: '2030-03-31T10:00:00+00:00' },
        seven_day: { utilization: 2.0, resets_at: '2030-04-02T10:00:00+00:00' },
        fetched_at: '2030-03-31T08:05:00+00:00',
        token_count_observed_at: '2030-03-31T08:04:00Z',
      }),
    },
  });

  const refreshed = _rowByKey(strip, 'codex:main');
  assert.equal(_field(refreshed, 'spend-24h').textContent, '\u2014');
  assert.equal(_field(refreshed, 'spend-7d').textContent, '\u2014');
});

test('renderExtUsage clears the Codex business/unlimited state when caps return', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({ rate_limits_state: 'business-unlimited' }),
    },
  });

  assert.ok(_field(_rowByKey(strip, 'codex:main'), 'state'));

  context.renderExtUsage({
    providers: {
      'codex:main': _codexPayload({
        five_hour: { utilization: 8.0, resets_at: '2030-03-31T10:00:00+00:00' },
        seven_day: { utilization: 2.0, resets_at: '2030-04-02T10:00:00+00:00' },
        fetched_at: '2026-03-31T08:05:00+00:00',
        token_count_observed_at: '2026-03-31T08:04:00Z',
      }),
    },
  });

  const row = _rowByKey(strip, 'codex:main');
  assert.equal(_field(row, 'state'), null);
  assert.equal(_field(row, '5h-pct').textContent, '8%');
  assert.notEqual(_field(row, '5h-reset').textContent, 'no 5h cap');
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
});

test('renderExtUsage groups accounts by provider with labels and a separator', () => {
  const { context, strip } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'claude:main': _claudePayload({ account: 'main' }),
      'claude:invite-1': _claudePayload({ account: 'invite-1', five_hour: { utilization: 7.0, resets_at: '' } }),
      'codex:main': _codexPayload(),
    },
  });

  const labelEls = strip.children.filter((c) => c.classList.contains('text-slate-500') && c.classList.contains('font-medium'));
  assert.equal(labelEls.length, 2);
  assert.equal(labelEls[0].textContent, 'Claude');
  assert.equal(labelEls[1].textContent, 'Codex');

  const separators = strip.children.filter((c) => c.textContent === '\u2502');
  assert.equal(separators.length, 1);

  assert.ok(_rowByKey(strip, 'claude:main'));
  assert.ok(_rowByKey(strip, 'claude:invite-1'));
  assert.ok(_rowByKey(strip, 'codex:main'));
});
