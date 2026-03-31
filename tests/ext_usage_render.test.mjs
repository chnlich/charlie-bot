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
  constructor(id, initialClassName = '') {
    this.id = id;
    this.textContent = '';
    this.style = {};
    this._className = '';
    this.classList = new FakeClassList(this, initialClassName);
    Object.defineProperty(this, 'className', {
      get: () => this._className,
      set: (value) => this.classList._replace(value),
    });
    this.className = initialClassName;
  }
}

function loadExtUsageScript() {
  const ids = [
    'ext-usage-strip',
    'ext-usage-cc-5h-bar',
    'ext-usage-cc-5h-pct',
    'ext-usage-cc-5h-reset',
    'ext-usage-cc-7d-bar',
    'ext-usage-cc-7d-pct',
    'ext-usage-cc-7d-reset',
    'ext-usage-codex-state',
    'ext-usage-codex-5h-bar',
    'ext-usage-codex-5h-pct',
    'ext-usage-codex-5h-reset',
    'ext-usage-codex-7d-bar',
    'ext-usage-codex-7d-pct',
    'ext-usage-codex-7d-reset',
  ];
  const elements = new Map(ids.map((id) => {
    const className = id === 'ext-usage-strip' || id === 'ext-usage-codex-state' ? 'hidden' : '';
    return [id, new FakeElement(id, className)];
  }));
  const document = {
    addEventListener() {},
    getElementById(id) {
      return elements.get(id) || null;
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

  return { context, elements };
}

test('renderExtUsage shows business/unlimited Codex state explicitly', () => {
  const { context, elements } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      'cc-opus': {
        five_hour: {
          utilization: 42.0,
          resets_at: '2026-03-31T10:00:00+00:00',
        },
        seven_day: {
          utilization: 10.0,
          resets_at: '2026-04-03T10:00:00+00:00',
        },
        fetched_at: '2026-03-31T08:00:00+00:00',
        provider: 'cc-opus',
      },
      codex: {
        five_hour: {
          utilization: 0.0,
          resets_at: '',
        },
        seven_day: {
          utilization: 0.0,
          resets_at: '',
        },
        fetched_at: '2026-03-31T08:00:00+00:00',
        provider: 'codex',
        rate_limits_state: 'business-unlimited',
        token_count_observed_at: '2026-03-31T07:59:00Z',
      },
    },
  });

  assert.equal(elements.get('ext-usage-strip').classList.contains('hidden'), false);
  assert.equal(elements.get('ext-usage-cc-5h-pct').textContent, '42%');
  assert.equal(elements.get('ext-usage-codex-state').textContent, 'business / unlimited');
  assert.equal(elements.get('ext-usage-codex-state').classList.contains('hidden'), false);
  assert.equal(elements.get('ext-usage-codex-5h-pct').textContent, 'plan');
  assert.equal(elements.get('ext-usage-codex-5h-reset').textContent, 'no 5h cap');
  assert.equal(elements.get('ext-usage-codex-7d-pct').textContent, 'plan');
  assert.equal(elements.get('ext-usage-codex-7d-reset').textContent, 'no 7d cap');
});

test('renderExtUsage clears the Codex business/unlimited state when caps return', () => {
  const { context, elements } = loadExtUsageScript();

  context.renderExtUsage({
    providers: {
      codex: {
        five_hour: {
          utilization: 0.0,
          resets_at: '',
        },
        seven_day: {
          utilization: 0.0,
          resets_at: '',
        },
        fetched_at: '2026-03-31T08:00:00+00:00',
        provider: 'codex',
        rate_limits_state: 'business-unlimited',
        token_count_observed_at: '2026-03-31T07:59:00Z',
      },
    },
  });

  context.renderExtUsage({
    providers: {
      codex: {
        five_hour: {
          utilization: 8.0,
          resets_at: '2030-03-31T10:00:00+00:00',
        },
        seven_day: {
          utilization: 2.0,
          resets_at: '2030-04-02T10:00:00+00:00',
        },
        fetched_at: '2030-03-31T08:05:00+00:00',
        provider: 'codex',
        token_count_observed_at: '2030-03-31T08:04:00Z',
      },
    },
  });

  assert.equal(elements.get('ext-usage-codex-state').classList.contains('hidden'), true);
  assert.equal(elements.get('ext-usage-codex-state').textContent, '');
  assert.equal(elements.get('ext-usage-codex-5h-pct').textContent, '8%');
  assert.notEqual(elements.get('ext-usage-codex-5h-reset').textContent, 'no 5h cap');
});
