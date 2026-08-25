const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');
const { escapeHtml } = require('./escape_html_stub');

const PLAN_PANEL_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'plan-panel.js'),
  'utf8'
);

// ---------------------------------------------------------------------------
// Minimal DOM shim used both as the `DOMParser` injected into the plan-panel
// vm context (so `_renderActionBar` can parse fetched plan HTML) and directly
// in the template contract test. It implements the small subset of the DOM
// surface that `parseOpenForks` and the action-bar path rely on:
// querySelector/querySelectorAll with `tag.class` selectors, cloneNode(deep),
// child.remove(), and a live textContent computed from child nodes.
// ---------------------------------------------------------------------------

const VOID_TAGS = new Set([
  'AREA', 'BASE', 'BR', 'COL', 'EMBED', 'HR', 'IMG', 'INPUT',
  'LINK', 'META', 'PARAM', 'SOURCE', 'TRACK', 'WBR',
]);
const RAW_TEXT_TAGS = new Set(['SCRIPT', 'STYLE']);

class MinimalElement {
  constructor(tagName, attrs) {
    this.tagName = String(tagName).toUpperCase();
    this.attributes = attrs || {};
    this.children = [];
    this.parentNode = null;
  }
  get textContent() {
    let text = '';
    for (const child of this.children) {
      if (typeof child === 'string') text += child;
      else text += child.textContent;
    }
    return text;
  }
  cloneNode(deep) {
    const clone = new MinimalElement(this.tagName, {...this.attributes});
    if (deep) {
      for (const child of this.children) {
        if (typeof child === 'string') {
          clone.children.push(child);
        } else {
          const c = child.cloneNode(true);
          c.parentNode = clone;
          clone.children.push(c);
        }
      }
    }
    return clone;
  }
  remove() {
    if (!this.parentNode) return;
    const idx = this.parentNode.children.indexOf(this);
    if (idx >= 0) this.parentNode.children.splice(idx, 1);
    this.parentNode = null;
  }
  querySelector(selector) {
    const matches = this._all(selector);
    return matches.length ? matches[0] : null;
  }
  querySelectorAll(selector) {
    return this._all(selector);
  }
  _all(selector) {
    const out = [];
    for (const child of this.children) {
      if (typeof child === 'string') continue;
      if (_matchesSelector(child, selector)) out.push(child);
      for (const n of child._all(selector)) out.push(n);
    }
    return out;
  }
}

function _matchesSelector(el, selector) {
  const m = /^([a-zA-Z][\w-]*)?(?:\.([a-zA-Z_][\w-]*))?$/.exec(selector || '');
  if (!m) return false;
  const wantTag = m[1] ? m[1].toUpperCase() : null;
  const wantClass = m[2] || null;
  if (wantTag && el.tagName !== wantTag) return false;
  if (wantClass) {
    const cls = (el.attributes.class || '').split(/\s+/);
    if (!cls.includes(wantClass)) return false;
  }
  return true;
}

function parseHTML(html) {
  const root = new MinimalElement('#document', {});
  const stack = [root];
  let i = 0;
  while (i < html.length) {
    if (html[i] === '<') {
      if (html.substr(i, 4) === '<!--') {
        const end = html.indexOf('-->', i + 4);
        if (end === -1) break;
        i = end + 3;
        continue;
      }
      if (html[i + 1] === '!') {
        const close = html.indexOf('>', i);
        if (close === -1) break;
        i = close + 1;
        continue;
      }
      if (html[i + 1] === '/') {
        const close = html.indexOf('>', i);
        if (close === -1) break;
        const tag = html.slice(i + 2, close).trim().toUpperCase();
        for (let j = stack.length - 1; j > 0; j--) {
          if (stack[j].tagName === tag) { stack.length = j; break; }
        }
        i = close + 1;
        continue;
      }
      const close = html.indexOf('>', i);
      if (close === -1) break;
      let inner = html.slice(i + 1, close);
      const selfClosing = inner.endsWith('/');
      if (selfClosing) inner = inner.slice(0, -1).trim();
      const sp = inner.search(/\s/);
      let tagName, attrStr;
      if (sp === -1) { tagName = inner.toUpperCase(); attrStr = ''; }
      else { tagName = inner.slice(0, sp).toUpperCase(); attrStr = inner.slice(sp + 1); }
      const attrs = {};
      const are = /([a-zA-Z_:][-a-zA-Z0-9_:.]*)(?:\s*=\s*"([^"]*)")?/g;
      let am;
      while ((am = are.exec(attrStr)) !== null) {
        attrs[am[1].toLowerCase()] = am[2] != null ? am[2] : '';
      }
      const el = new MinimalElement(tagName, attrs);
      el.parentNode = stack[stack.length - 1];
      stack[stack.length - 1].children.push(el);
      i = close + 1;
      if (selfClosing || VOID_TAGS.has(tagName)) {
        // no children
      } else if (RAW_TEXT_TAGS.has(tagName)) {
        const closeTag = '</' + tagName.toLowerCase() + '>';
        const end = html.indexOf(closeTag, i);
        if (end === -1) {
          if (i < html.length) el.children.push(html.slice(i));
          break;
        }
        if (end > i) el.children.push(html.slice(i, end));
        i = end + closeTag.length;
      } else {
        stack.push(el);
      }
    } else {
      let next = html.indexOf('<', i);
      if (next === -1) next = html.length;
      const text = html.slice(i, next).replace(/\s+/g, ' ');
      if (text.trim()) stack[stack.length - 1].children.push(text);
      i = next;
    }
  }
  return root;
}

class DOMParser {
  parseFromString(html, mimeType) {
    return parseHTML(html);
  }
}

function loadPlanPanelScript(opts = {}) {
  const noop = () => {};
  const document = opts.document || {
    readyState: 'loading',
    addEventListener: noop,
    getElementById: () => null,
    createElement: () => ({
      _listeners: {},
      addEventListener(ev, fn) { this._listeners[ev] = fn; },
      appendChild() {},
      set type(v) { this._type = v; },
      get type() { return this._type; },
    }),
  };
  const context = {
    SESSION_ID: opts.sessionId || 'test-session',
    SESSIONS_ROOT: opts.sessionsRoot || '/home/user/.charliebot/sessions',
    console: {error: noop, log: noop, warn: noop},
    document,
    window: {SESSIONS_ROOT: opts.sessionsRoot || '/home/user/.charliebot/sessions'},
    localStorage: {getItem: () => null, setItem: noop},
    fetch: opts.fetch || (async () => {
      throw new Error('fetch should not run during script load');
    }),
    DOMParser,
    escapeHtmlAttr: (value) => escapeHtml(value == null ? '' : String(value)),
  };
  vm.createContext(context);
  vm.runInContext(PLAN_PANEL_JS, context, {filename: 'plan-panel.js'});
  const planPanel = vm.runInContext('planPanel', context);
  return {context, planPanel};
}

function makePlan(id, versions, opts) {
  const o = opts || {};
  return {
    id: id,
    title: o.title || 'Plan ' + id,
    versions: versions,
    takeoff: o.takeoff || null,
    closed: o.closed || null,
    state: o.state || 'in flight',
  };
}

function makeVersion(v, file) {
  return {
    v: v,
    file: file || ('artifacts/plan_' + String(v).padStart(2, '0') + '.html'),
    created_at: '2026-07-20T00:00:00+00:00',
    trigger: v === 1 ? 'initial' : 'feedback',
    verify_thread: 'th_' + v,
    verify_state: 'pending',
    base: null,
  };
}

// ---------------------------------------------------------------------------
// selectDefaultLineage
// ---------------------------------------------------------------------------

test('selectDefaultLineage returns null for empty plans', () => {
  const {planPanel} = loadPlanPanelScript();
  assert.equal(planPanel.selectDefaultLineage([]), null);
  assert.equal(planPanel.selectDefaultLineage(null), null);
  assert.equal(planPanel.selectDefaultLineage({plans: []}), null);
});

test('selectDefaultLineage picks the newest not-closed not-approved lineage', () => {
  const {planPanel} = loadPlanPanelScript();
  const plans = [
    makePlan(1, [makeVersion(1)], {state: 'approved', takeoff: {v: 1, at: 'x'}}),
    makePlan(2, [makeVersion(1)], {state: 'superseded', closed: {as: 'superseded', at: 'x'}}),
    makePlan(3, [makeVersion(1)], {state: 'in flight'}),
    makePlan(4, [makeVersion(1)], {state: 'awaiting approval'}),
  ];
  const result = planPanel.selectDefaultLineage(plans);
  assert.equal(result.planId, 4);
  assert.equal(result.version, 1);
});

test('selectDefaultLineage falls back to the newest lineage when all are closed or approved', () => {
  const {planPanel} = loadPlanPanelScript();
  const plans = [
    makePlan(1, [makeVersion(1), makeVersion(2)], {state: 'approved', takeoff: {v: 2, at: 'x'}}),
    makePlan(2, [makeVersion(1)], {state: 'abandoned', closed: {as: 'abandoned', at: 'x'}}),
  ];
  const result = planPanel.selectDefaultLineage(plans);
  assert.equal(result.planId, 2);
  assert.equal(result.version, 1);
});

test('selectDefaultLineage selects the latest version of the chosen lineage', () => {
  const {planPanel} = loadPlanPanelScript();
  const plans = [
    makePlan(1, [makeVersion(1), makeVersion(2), makeVersion(3)], {state: 'in flight'}),
  ];
  const result = planPanel.selectDefaultLineage(plans);
  assert.equal(result.planId, 1);
  assert.equal(result.version, 3);
});

// ---------------------------------------------------------------------------
// isStaleVersion
// ---------------------------------------------------------------------------

test('isStaleVersion returns true when viewing a non-latest version', () => {
  const {planPanel} = loadPlanPanelScript();
  const plan = makePlan(1, [makeVersion(1), makeVersion(2), makeVersion(3)]);
  assert.equal(planPanel.isStaleVersion(plan, 1), true);
  assert.equal(planPanel.isStaleVersion(plan, 2), true);
  assert.equal(planPanel.isStaleVersion(plan, 3), false);
});

test('isStaleVersion returns false for the latest version', () => {
  const {planPanel} = loadPlanPanelScript();
  const plan = makePlan(1, [makeVersion(1)]);
  assert.equal(planPanel.isStaleVersion(plan, 1), false);
});

test('isStaleVersion returns false for a plan with no versions', () => {
  const {planPanel} = loadPlanPanelScript();
  assert.equal(planPanel.isStaleVersion({versions: []}, 1), false);
  assert.equal(planPanel.isStaleVersion(null, 1), false);
});

test('render shows the stale notice when an older version is selected', async () => {
  const elements = {};
  for (const id of ['tab-plans', 'plan-empty-state', 'plan-viewer', 'plan-selector',
    'plan-version-selector', 'plan-stale-notice', 'plan-action-bar']) {
    const classes = new Set(id === 'plan-stale-notice' ? ['hidden'] : []);
    elements[id] = {
      style: {},
      innerHTML: '',
      src: '',
      classList: {
        add(name) { classes.add(name); },
        toggle(name, force) {
          if (force) classes.add(name);
          else classes.delete(name);
        },
        contains(name) { return classes.has(name); },
      },
    };
  }
  elements['tab-plans'].classList.toggle = () => {};
  const document = {
    readyState: 'loading',
    addEventListener: () => {},
    getElementById: (id) => elements[id] || null,
    createElement: () => ({
      _listeners: {},
      addEventListener(ev, fn) { this._listeners[ev] = fn; },
      appendChild() {},
    }),
  };
  const fetch = async () => ({
    ok: true,
    json: async () => ({plans: [makePlan(1, [makeVersion(1), makeVersion(2)])]}),
    text: async () => '',
  });
  const {planPanel} = loadPlanPanelScript({document, fetch});

  await planPanel.refresh();
  assert.equal(elements['plan-stale-notice'].classList.contains('hidden'), true);
  planPanel.selectVersion('1');
  assert.equal(elements['plan-stale-notice'].classList.contains('hidden'), false);
});

// ---------------------------------------------------------------------------
// detectNewPlanOrVersion
// ---------------------------------------------------------------------------

test('detectNewPlanOrVersion returns null when nothing changed', () => {
  const {planPanel} = loadPlanPanelScript();
  const snap = {plans: [makePlan(1, [makeVersion(1)])]};
  assert.equal(planPanel.detectNewPlanOrVersion(snap, snap), null);
});

test('detectNewPlanOrVersion detects a new lineage', () => {
  const {planPanel} = loadPlanPanelScript();
  const prev = {plans: [makePlan(1, [makeVersion(1)])]};
  const next = {
    plans: [
      makePlan(1, [makeVersion(1)]),
      makePlan(2, [makeVersion(1)]),
    ],
  };
  const result = planPanel.detectNewPlanOrVersion(prev, next);
  assert.equal(result.planId, 2);
  assert.equal(result.version, 1);
});

test('detectNewPlanOrVersion detects a new version in an existing lineage', () => {
  const {planPanel} = loadPlanPanelScript();
  const prev = {plans: [makePlan(1, [makeVersion(1)])]};
  const next = {plans: [makePlan(1, [makeVersion(1), makeVersion(2)])]};
  const result = planPanel.detectNewPlanOrVersion(prev, next);
  assert.equal(result.planId, 1);
  assert.equal(result.version, 2);
});

test('detectNewPlanOrVersion returns null when only state changed (no new lineage or version)', () => {
  const {planPanel} = loadPlanPanelScript();
  const prev = {
    plans: [makePlan(1, [makeVersion(1)], {state: 'in flight'})],
  };
  const next = {
    plans: [makePlan(1, [makeVersion(1)], {state: 'awaiting approval'})],
  };
  const result = planPanel.detectNewPlanOrVersion(prev, next);
  assert.equal(result, null);
});

test('detectNewPlanOrVersion returns null when going from plans to empty', () => {
  const {planPanel} = loadPlanPanelScript();
  const prev = {plans: [makePlan(1, [makeVersion(1)])]};
  const next = {plans: []};
  assert.equal(planPanel.detectNewPlanOrVersion(prev, next), null);
});

// ---------------------------------------------------------------------------
// parseOpenForks
// ---------------------------------------------------------------------------

function makeForkElement(opts) {
  const o = opts || {};
  function makeFn() {
    return o.fn === false ? null : {
      textContent: String(o.fn != null ? o.fn : '1'),
      remove() {},
    };
  }
  function makeQ() {
    if (o.q === false) return null;
    const qText = o.q || 'Question?';
    const qNode = {
      textContent: qText,
      querySelector(selector) {
        if (selector === 'span.fn') return makeFn();
        return null;
      },
      cloneNode(deep) {
        // The clone carries the question text only; span.fn is modelled as
        // a removable child so parseOpenForks can drop it. Removing it does
        // not change textContent here because the stub's qText is already the
        // clean question (the real DOM path is exercised by the template
        // contract test).
        const clone = {
          textContent: qText,
          querySelector(selector) {
            if (selector === 'span.fn') return makeFn();
            return null;
          },
        };
        return clone;
      },
    };
    return qNode;
  }
  return {
    querySelector(selector) {
      if (selector === 'span.fn') return makeFn();
      if (selector === 'p.q') return makeQ();
      if (selector === 'p.rec') return o.rec === false ? null : {textContent: 'Recommendation'};
      if (selector === 'p.resolved') return o.resolved === true ? {textContent: 'Resolved'} : null;
      return null;
    },
  };
}

function makeFakeDoc(forks) {
  return {
    querySelectorAll(selector) {
      if (selector === 'div.fork') return forks;
      return [];
    },
  };
}

test('parseOpenForks returns forks with p.rec and no p.resolved as open', () => {
  const {planPanel} = loadPlanPanelScript();
  const forks = [
    makeForkElement({fn: '1', q: 'First trade-off'}),
    makeForkElement({fn: '2', q: 'Second trade-off'}),
  ];
  const doc = makeFakeDoc(forks);
  const result = planPanel.parseOpenForks(doc);
  assert.equal(result.length, 2);
  assert.equal(result[0].n, '1');
  assert.equal(result[0].question, 'First trade-off');
  assert.equal(result[1].n, '2');
  assert.equal(result[1].question, 'Second trade-off');
});

test('parseOpenForks excludes resolved forks (those with p.resolved)', () => {
  const {planPanel} = loadPlanPanelScript();
  const forks = [
    makeForkElement({fn: '1', q: 'Open trade-off'}),
    makeForkElement({fn: '2', q: 'Resolved trade-off', resolved: true}),
  ];
  const doc = makeFakeDoc(forks);
  const result = planPanel.parseOpenForks(doc);
  assert.equal(result.length, 1);
  assert.equal(result[0].n, '1');
  assert.equal(result[0].question, 'Open trade-off');
});

test('parseOpenForks excludes forks without p.rec (no recommendation)', () => {
  const {planPanel} = loadPlanPanelScript();
  const forks = [
    makeForkElement({fn: '1', q: 'Has recommendation'}),
    makeForkElement({fn: '2', q: 'No recommendation', rec: false}),
  ];
  const doc = makeFakeDoc(forks);
  const result = planPanel.parseOpenForks(doc);
  assert.equal(result.length, 1);
  assert.equal(result[0].n, '1');
});

test('parseOpenForks excludes forks missing span.fn or p.q', () => {
  const {planPanel} = loadPlanPanelScript();
  const forks = [
    makeForkElement({fn: false, q: 'Missing fn'}),
    makeForkElement({fn: '2', q: false}),
    makeForkElement({fn: '3', q: 'Valid fork'}),
  ];
  const doc = makeFakeDoc(forks);
  const result = planPanel.parseOpenForks(doc);
  assert.equal(result.length, 1);
  assert.equal(result[0].n, '3');
});

test('parseOpenForks returns empty for null or empty documents', () => {
  const {planPanel} = loadPlanPanelScript();
  assert.equal(planPanel.parseOpenForks(null).length, 0);
  assert.equal(planPanel.parseOpenForks(undefined).length, 0);
  assert.equal(planPanel.parseOpenForks(makeFakeDoc([])).length, 0);
});

// ---------------------------------------------------------------------------
// buildIframeUrl
// ---------------------------------------------------------------------------

test('buildIframeUrl builds a real /files URL with cbsession and cbpanel marker', () => {
  const {planPanel} = loadPlanPanelScript({sessionId: 'sess-42', sessionsRoot: '/home/alice/.charliebot/sessions'});
  const url = planPanel.buildIframeUrl('artifacts/plan_01.html', 'sess-42', '/home/alice/.charliebot/sessions');
  const expectedPath = '/files/home/alice/.charliebot/sessions/sess-42/artifacts/plan_01.html';
  assert.ok(url.startsWith(expectedPath), 'URL starts with the real /files path');
  assert.ok(url.indexOf('#cbsession=sess-42') !== -1, 'URL carries the cbsession fragment');
  assert.ok(url.indexOf('&cbpanel=1') !== -1, 'URL carries the cbpanel=1 marker');
});

test('buildIframeUrlFromVersion resolves the correct version file', () => {
  const {planPanel} = loadPlanPanelScript({sessionId: 's1', sessionsRoot: '/home/u/.charliebot/sessions'});
  const plan = makePlan(1, [
    makeVersion(1, 'artifacts/plan_01.html'),
    makeVersion(2, 'artifacts/plan_02.html'),
  ]);
  const url = planPanel.buildIframeUrlFromVersion(plan, 2, 's1', '/home/u/.charliebot/sessions');
  assert.ok(url.indexOf('artifacts/plan_02.html') !== -1, 'URL points to v2 file');
  assert.ok(url.indexOf('&cbpanel=1') !== -1, 'URL carries the panel marker');
});

test('buildIframeUrlFromVersion returns null for unknown version', () => {
  const {planPanel} = loadPlanPanelScript();
  const plan = makePlan(1, [makeVersion(1)]);
  assert.equal(planPanel.buildIframeUrlFromVersion(plan, 99, 's1', '/home/u/.charliebot/sessions'), null);
  assert.equal(planPanel.buildIframeUrlFromVersion(null, 1, 's1', '/home/u/.charliebot/sessions'), null);
});

// ---------------------------------------------------------------------------
// buildStandaloneUrl / buildStandaloneUrlFromVersion (Open in tab path)
// ---------------------------------------------------------------------------

test('buildStandaloneUrl builds a real /files URL with cbsession and NO cbpanel marker', () => {
  const {planPanel} = loadPlanPanelScript({sessionId: 'sess-42', sessionsRoot: '/home/alice/.charliebot/sessions'});
  const url = planPanel.buildStandaloneUrl('artifacts/plan_01.html', 'sess-42', '/home/alice/.charliebot/sessions');
  const expectedPath = '/files/home/alice/.charliebot/sessions/sess-42/artifacts/plan_01.html';
  assert.ok(url.startsWith(expectedPath), 'URL starts with the real /files path');
  assert.ok(url.indexOf('#cbsession=sess-42') !== -1, 'URL carries the cbsession fragment');
  assert.equal(url.indexOf('cbpanel'), -1, 'standalone URL must NOT carry the cbpanel marker');
});

test('buildStandaloneUrlFromVersion resolves the version file without cbpanel', () => {
  const {planPanel} = loadPlanPanelScript({sessionId: 's1', sessionsRoot: '/home/u/.charliebot/sessions'});
  const plan = makePlan(1, [
    makeVersion(1, 'artifacts/plan_01.html'),
    makeVersion(2, 'artifacts/plan_02.html'),
  ]);
  const url = planPanel.buildStandaloneUrlFromVersion(plan, 2, 's1', '/home/u/.charliebot/sessions');
  assert.ok(url.indexOf('artifacts/plan_02.html') !== -1, 'URL points to v2 file');
  assert.ok(url.indexOf('#cbsession=s1') !== -1, 'URL carries the cbsession fragment');
  assert.equal(url.indexOf('cbpanel'), -1, 'standalone URL must NOT carry the cbpanel marker');
});

test('buildStandaloneUrlFromVersion returns null for unknown version', () => {
  const {planPanel} = loadPlanPanelScript();
  const plan = makePlan(1, [makeVersion(1)]);
  assert.equal(planPanel.buildStandaloneUrlFromVersion(plan, 99, 's1', '/home/u/.charliebot/sessions'), null);
  assert.equal(planPanel.buildStandaloneUrlFromVersion(null, 1, 's1', '/home/u/.charliebot/sessions'), null);
});

// ---------------------------------------------------------------------------
// stateBadgeClass
// ---------------------------------------------------------------------------

test('stateBadgeClass maps derived state strings to badge classes', () => {
  const {planPanel} = loadPlanPanelScript();
  assert.match(planPanel.stateBadgeClass('approved'), /bg-green-900/);
  assert.match(planPanel.stateBadgeClass('awaiting approval'), /bg-blue-900/);
  assert.match(planPanel.stateBadgeClass('in flight'), /bg-slate-700/);
  assert.match(planPanel.stateBadgeClass('approved \u00B7 awaiting clean verify'), /bg-green-900/);
  assert.match(planPanel.stateBadgeClass('superseded'), /bg-gray-700/);
  assert.match(planPanel.stateBadgeClass('abandoned'), /bg-gray-700/);
  assert.match(planPanel.stateBadgeClass('unknown state'), /bg-gray-700/);
});

// ---------------------------------------------------------------------------
// ensureLoaded / ready — session-keyed load (SPA switch safety)
// ---------------------------------------------------------------------------

function makeSessionFetch(registries) {
  return async (url) => {
    const m = String(url).match(/\/api\/sessions\/([^/]+)\/plans/);
    const sid = m ? decodeURIComponent(m[1]) : null;
    return {ok: true, json: async () => registries[sid] || {plans: []}};
  };
}

test('ready() reloads the registry for the current session after an SPA session switch', async () => {
  const registries = {
    A: {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'Plan A'})]},
    B: {plans: [makePlan(9, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'Plan B'})]},
  };
  const {context, planPanel} = loadPlanPanelScript({sessionId: 'A', fetch: makeSessionFetch(registries)});

  await planPanel.ready();
  assert.equal(planPanel.getRegistrySnapshot().plans[0].id, 1, 'loads the current (A) session registry');

  // Session switching is an in-place SPA swap: SESSION_ID changes with no reload.
  context.SESSION_ID = 'B';
  await planPanel.ready();
  const snap = planPanel.getRegistrySnapshot();
  assert.equal(snap.plans[0].id, 9, 'ready() re-fetches session B, not the stale session A snapshot');
  assert.equal(snap.plans[0].title, 'Plan B');
});

test('ensureLoaded issues a single fetch per session across repeated ready() calls', async () => {
  let calls = 0;
  const fetch = async (url) => {
    calls += 1;
    return {ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1)])]})};
  };
  const {planPanel} = loadPlanPanelScript({sessionId: 'A', fetch});
  await Promise.all([planPanel.ready(), planPanel.ready(), planPanel.ready()]);
  assert.equal(calls, 1, 'no polling / no per-call refetch for the same session');
});

// ---------------------------------------------------------------------------
// Item 1: single session-checked commit point — stale session / generation
// ---------------------------------------------------------------------------

test('_commitRegistry discards a stale-session write (session switched mid-flight)', async () => {
  let resolveApi;
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1) {
      return new Promise((resolve) => { resolveApi = resolve; });
    }
    return {ok: true, text: async () => ''};
  };
  const {context, planPanel} = loadPlanPanelScript({sessionId: 'A', fetch});
  const readyPromise = planPanel.ready();
  // Session switches before the fetch completes.
  context.SESSION_ID = 'B';
  resolveApi({ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1)], {title: 'A plan'})]})});
  await readyPromise;
  const snap = planPanel.getRegistrySnapshot();
  assert.equal(snap.plans.length, 0, 'stale session-A write discarded after switch to B');
});

test('_commitRegistry discards a stale-generation write (newer fetch supersedes older)', async () => {
  const queue = [];
  let calls = 0;
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1) {
      calls += 1;
      const myCall = calls;
      return new Promise((resolve) => { queue.push({myCall, resolve}); });
    }
    return {ok: true, text: async () => ''};
  };
  const {planPanel} = loadPlanPanelScript({sessionId: 'A', fetch});
  const r1 = planPanel.refresh();
  const r2 = planPanel.refresh();
  assert.equal(queue.length, 2, 'two /api fetches in flight');
  // Resolve the newer (r2, generation 2) first.
  queue[1].resolve({ok: true, json: async () => ({plans: [makePlan(2, [makeVersion(1)], {title: 'newer'})]})});
  await r2;
  // Then resolve the older (r1, generation 1); its commit must be discarded.
  queue[0].resolve({ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1)], {title: 'older'})]})});
  await r1;
  const snap = planPanel.getRegistrySnapshot();
  assert.equal(snap.plans.length, 1);
  assert.equal(snap.plans[0].id, 2, 'newer generation wins; older stale write discarded');
});

test('_commitRegistry warns and renders normally when the response carries a non-empty errors array', async () => {
  const warnings = [];
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1)
      return {ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1)])], errors: ['boom']})};
    return {ok: true, text: async () => ''};
  };
  const {context, planPanel} = loadPlanPanelScript({sessionId: 's1', fetch});
  context.console.warn = (...args) => { warnings.push(args); };
  await planPanel.refresh();
  assert.equal(warnings.length, 1, 'non-empty errors array is warned once');
  assert.equal(planPanel.getRegistrySnapshot().plans.length, 1, 'registry still rendered normally');
});

// ---------------------------------------------------------------------------
// Item 2: navigation invariant — same-selection render keeps src untouched
// ---------------------------------------------------------------------------

function makePanelElements() {
  const elements = {};
  for (const id of ['tab-plans', 'plan-empty-state', 'plan-selector', 'plan-version-selector',
    'plan-stale-notice', 'plan-action-bar']) {
    elements[id] = {
      style: {},
      innerHTML: '',
      classList: {
        add() {}, toggle() {}, contains() { return false; },
      },
    };
  }
  return elements;
}

test('_renderViewer leaves iframe.src untouched when selection is unchanged', async () => {
  const elements = makePanelElements();
  let srcVal = '';
  const srcWrites = [];
  const viewer = {
    style: {},
    addEventListener: () => {},
    classList: {add() {}, toggle() {}, contains() { return false; }},
  };
  Object.defineProperty(viewer, 'src', {
    get() { return srcVal; },
    set(v) { srcWrites.push(v); srcVal = v; },
    configurable: true, enumerable: true,
  });
  elements['plan-viewer'] = viewer;
  const document = {
    readyState: 'loading',
    addEventListener: () => {},
    getElementById: (id) => elements[id] || null,
    createElement: () => ({addEventListener() {}, appendChild() {}}),
  };
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1)
      return {ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1), makeVersion(2)])]})};
    return {ok: true, text: async () => ''};
  };
  const {planPanel} = loadPlanPanelScript({document, fetch});
  await planPanel.refresh();
  const writesAfterRefresh = srcWrites.length;
  assert.ok(writesAfterRefresh >= 1, 'initial render navigates the iframe');
  // Re-render with the same selection → no new src write.
  planPanel.render();
  assert.equal(srcWrites.length, writesAfterRefresh, 'same selection does not reload the iframe');
  // A selection jump navigates → a new src write.
  planPanel.selectVersion('1');
  assert.ok(srcWrites.length > writesAfterRefresh, 'a selection jump reloads the iframe');
});

// ---------------------------------------------------------------------------
// Item 3: no forced tab switch on plan update
// ---------------------------------------------------------------------------

test('onPlanUpdated keeps the badge + selection jump but does not force a tab switch', async () => {
  let registryResponse = {plans: [makePlan(1, [makeVersion(1)])]};
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1)
      return {ok: true, json: async () => registryResponse};
    return {ok: true, text: async () => ''};
  };
  const switchCalls = [];
  const {context, planPanel} = loadPlanPanelScript({sessionId: 's1', fetch});
  context.switchTab = (t) => { switchCalls.push(t); };
  await planPanel.onPlanUpdated(1);
  assert.deepEqual(switchCalls, [], 'no switch on first plan appearance');
  registryResponse = {plans: [makePlan(1, [makeVersion(1), makeVersion(2)])]};
  await planPanel.onPlanUpdated(1);
  assert.deepEqual(switchCalls, [], 'no switch even when a new version appears');
});

// ---------------------------------------------------------------------------
// Item 4: action bar chip text and ellipsis
// ---------------------------------------------------------------------------

test('_renderActionBar chip text is the question without the fork number, truncated with ellipsis', async () => {
  const longQuestion = 'A really long trade-off question that is definitely more than forty characters';
  const forkHtml = '<div class="fork"><p class="q"><span class="fn">1</span> ' + longQuestion + '</p><p class="rec">R</p></div>';
  const elements = makePanelElements();
  const bar = {innerHTML: '', _children: [], appendChild(c) { this._children.push(c); }};
  elements['plan-action-bar'] = bar;
  const viewer = {style: {}, src: '', addEventListener: () => {}, classList: {add() {}, toggle() {}, contains() { return false; }}};
  elements['plan-viewer'] = viewer;
  const document = {
    readyState: 'loading',
    addEventListener: () => {},
    getElementById: (id) => elements[id] || null,
    createElement: () => {
      const o = {
        _listeners: {}, _text: '', _title: '', _className: '', _type: '',
        addEventListener(ev, fn) { this._listeners[ev] = fn; },
        appendChild() {},
      };
      Object.defineProperties(o, {
        type: {get() { return this._type; }, set(v) { this._type = v; }, configurable: true, enumerable: true},
        className: {get() { return this._className; }, set(v) { this._className = v; }, configurable: true, enumerable: true},
        textContent: {get() { return this._text; }, set(v) { this._text = v; }, configurable: true, enumerable: true},
        title: {get() { return this._title; }, set(v) { this._title = v; }, configurable: true, enumerable: true},
      });
      return o;
    },
  };
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1)
      return {ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1, 'plan_01.html')])]})};
    return {ok: true, text: async () => forkHtml};
  };
  const {planPanel} = loadPlanPanelScript({document, fetch, sessionId: 's1', sessionsRoot: '/home/u/.charliebot/sessions'});
  await planPanel.refresh();
  assert.equal(bar._children.length, 1, 'one chip rendered for the one open fork');
  const chip = bar._children[0];
  assert.equal(chip.textContent.startsWith('1'), false, 'chip text must not start with the fork number');
  assert.equal(chip.textContent.length, 41, 'chip text is 40 chars + ellipsis');
  assert.ok(chip.textContent.endsWith('\u2026'), 'chip text ends with an ellipsis');
  assert.equal(chip.title, 'Prefill: Trade-off 1');
});

// ---------------------------------------------------------------------------
// Item 5: formatPlanStateLabel variants + selector label use
// ---------------------------------------------------------------------------

test('formatPlanStateLabel: plain state, takeoff-qualified approved, and selector use', async () => {
  const {planPanel} = loadPlanPanelScript();
  assert.equal(planPanel.formatPlanStateLabel(null), '');
  assert.equal(planPanel.formatPlanStateLabel({state: 'in flight'}), 'in flight');
  assert.equal(planPanel.formatPlanStateLabel({state: 'awaiting approval'}), 'awaiting approval');
  assert.equal(planPanel.formatPlanStateLabel({state: 'approved', takeoff: {v: 3}}), 'approved \u00B7 v3');
  assert.equal(planPanel.formatPlanStateLabel({state: 'in flight', takeoff: {v: 3}}), 'in flight');
  assert.equal(planPanel.formatPlanStateLabel({state: 'approved'}), 'approved');
  assert.equal(planPanel.formatPlanStateLabel({}), '');

  // The plan-selector label uses formatPlanStateLabel (takeoff-qualified when approved).
  const elements = makePanelElements();
  const sel = {innerHTML: '', style: {}, classList: {add() {}, toggle() {}, contains() { return false; }}};
  elements['plan-selector'] = sel;
  elements['plan-viewer'] = {style: {}, src: '', addEventListener: () => {}, classList: {add() {}, toggle() {}, contains() { return false; }}};
  const document = {
    readyState: 'loading', addEventListener: () => {},
    getElementById: (id) => elements[id] || null,
    createElement: () => ({addEventListener() {}, appendChild() {}}),
  };
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1)
      return {ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1)], {state: 'approved', takeoff: {v: 1, at: 'x'}})]})};
    return {ok: true, text: async () => ''};
  };
  const {planPanel: pp} = loadPlanPanelScript({document, fetch});
  await pp.refresh();
  assert.ok(sel.innerHTML.indexOf('approved \u00B7 v1') !== -1, 'selector label is takeoff-qualified');
});

// ---------------------------------------------------------------------------
// Item 6: template contract — parseOpenForks against prompts/plan_template.html
// ---------------------------------------------------------------------------

test('plan_template.html contract: open forks extracted, no question starts with its number, resolved excluded', () => {
  const {planPanel} = loadPlanPanelScript();
  const templatePath = path.join(__dirname, '..', 'prompts', 'plan_template.html');
  const html = fs.readFileSync(templatePath, 'utf8');
  const doc = new DOMParser().parseFromString(html, 'text/html');
  const forks = planPanel.parseOpenForks(doc);
  assert.ok(forks.length >= 1, 'template has at least one open fork');
  for (const f of forks) {
    assert.ok(f.n && f.question, 'each open fork has a number and a question');
    assert.equal(String(f.question).startsWith(String(f.n)), false,
      'question text must not start with its fork number');
  }
  const ns = forks.map((f) => String(f.n));
  assert.ok(ns.includes('1'), 'open fork #1 (scope question) is present');
  assert.equal(ns.includes('2'), false, 'resolved fork #2 is excluded');
});

// ---------------------------------------------------------------------------
// Session-change resync: the viewer reloads on a session switch even when
// (planId, version) is unchanged; onActiveSessionChanged() blanks the iframe
// synchronously and re-fetches; a plain reconnect only marks stale.
// ---------------------------------------------------------------------------

function makeRecordingViewer() {
  const writes = [];
  let srcVal = '';
  const viewer = {
    style: {},
    addEventListener: () => {},
    classList: {add() {}, toggle() {}, contains() { return false; }},
  };
  Object.defineProperty(viewer, 'src', {
    get() { return srcVal; },
    set(v) { writes.push(v); srcVal = v; },
    configurable: true, enumerable: true,
  });
  return {viewer, writes};
}

function makePlanDocument(elements) {
  return {
    readyState: 'loading',
    addEventListener: () => {},
    getElementById: (id) => elements[id] || null,
    createElement: () => ({addEventListener() {}, appendChild() {}}),
  };
}

function makeSessionFetchWithFiles(registries) {
  const plansFetch = makeSessionFetch(registries);
  return async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1) return plansFetch(url);
    return {ok: true, text: async () => ''};
  };
}

test('viewer reloads on a session change even when (planId, version) is unchanged across sessions', async () => {
  const registries = {
    A: {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')])]},
    B: {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')])]},
  };
  const elements = makePanelElements();
  const {viewer, writes} = makeRecordingViewer();
  elements['plan-viewer'] = viewer;
  const {context, planPanel} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch: makeSessionFetchWithFiles(registries),
    sessionId: 'A',
  });

  await planPanel.refresh();
  const writesAfterA = writes.length;
  assert.ok(writesAfterA >= 1, 'initial render loads the iframe for session A');
  assert.ok(viewer.src.indexOf('sessions/A/') !== -1, 'iframe URL targets session A');

  // SPA session switch: same (planId, version) in B, but the session differs.
  context.SESSION_ID = 'B';
  await planPanel.onActiveSessionChanged();
  assert.ok(writes.length > writesAfterA, 'viewer reloads on session change even with same planId/version');
  assert.ok(viewer.src.indexOf('sessions/B/') !== -1, 'new iframe URL targets session B');
  assert.ok(viewer.src.indexOf('sessions/A/') === -1, 'new iframe URL no longer targets session A');
});

test('onActiveSessionChanged blanks iframe.src synchronously before the new registry fetch resolves', async () => {
  const fetchQueue = [];
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1) {
      return new Promise((resolve) => { fetchQueue.push({url, resolve}); });
    }
    return {ok: true, text: async () => ''};
  };
  const elements = makePanelElements();
  const {viewer, writes} = makeRecordingViewer();
  elements['plan-viewer'] = viewer;
  const {context, planPanel} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch,
    sessionId: 'A',
  });

  // Load session A with a plan so the viewer has content to blank.
  const refreshA = planPanel.refresh();
  assert.equal(fetchQueue.length, 1, 'refresh fetched A registry');
  fetchQueue[0].resolve({ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')])]})});
  await refreshA;
  assert.ok(viewer.src.indexOf('sessions/A/') !== -1, 'viewer loaded session A');
  const writesAfterA = writes.length;

  // SPA session switch to B. onActiveSessionChanged blanks the iframe
  // synchronously, then starts an async refresh for B's registry.
  context.SESSION_ID = 'B';
  const pendingChange = planPanel.onActiveSessionChanged();
  // Synchronous assertion: the iframe is blanked before B's fetch resolves.
  assert.equal(viewer.src, '', 'iframe.src is blanked synchronously before the fetch resolves');
  assert.ok(writes.length > writesAfterA, 'a blank src write happened synchronously');

  // Now resolve B's registry (also has a plan) and await.
  assert.equal(fetchQueue.length, 2, 'refresh fetched B registry');
  fetchQueue[1].resolve({ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')])]})});
  await pendingChange;
  assert.ok(viewer.src.indexOf('sessions/B/') !== -1, 'viewer reloaded with session B URL after fetch resolved');
});

test('onReconnect only marks stale and does not refresh or reset selection on a plain WS reconnect', async () => {
  const fetchCalls = [];
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1) {
      fetchCalls.push(url);
      return {ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1), makeVersion(2)])]})};
    }
    return {ok: true, text: async () => ''};
  };
  const elements = makePanelElements();
  const {viewer} = makeRecordingViewer();
  elements['plan-viewer'] = viewer;
  const {planPanel} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch,
    sessionId: 'A',
  });

  await planPanel.refresh();
  const callsBeforeReconnect = fetchCalls.length;
  // Select an older version so selection state is populated and observable.
  planPanel.selectVersion('1');
  planPanel.render();
  const vsel = elements['plan-version-selector'];
  assert.ok(vsel.innerHTML.indexOf('value="1" selected') !== -1, 'v1 is selected before reconnect');

  // A plain WS reconnect (no session change) must only mark stale.
  planPanel.onReconnect();
  assert.equal(fetchCalls.length, callsBeforeReconnect, 'onReconnect does not trigger a fetch (no eager refresh)');

  // Selection survives: re-render and confirm v1 is still selected.
  planPanel.render();
  assert.ok(vsel.innerHTML.indexOf('value="1" selected') !== -1, 'selection survives onReconnect');

  // _stale was set: onTabShown now triggers a refresh (fetch count increases).
  const callsBeforeTabShown = fetchCalls.length;
  await planPanel.onTabShown();
  assert.ok(fetchCalls.length > callsBeforeTabShown, 'onReconnect marked stale so onTabShown refreshes');
});

test('onActiveSessionChanged renders the empty state and clears the iframe when the new session has no plans', async () => {
  const registries = {
    A: {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')])]},
    B: {plans: []},
  };
  const elements = makePanelElements();
  const {viewer} = makeRecordingViewer();
  elements['plan-viewer'] = viewer;
  const {context, planPanel} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch: makeSessionFetchWithFiles(registries),
    sessionId: 'A',
  });

  await planPanel.refresh();
  assert.ok(viewer.src.indexOf('sessions/A/') !== -1, 'viewer loaded for session A');
  assert.equal(elements['plan-empty-state'].style.display, 'none', 'empty state hidden when A has plans');

  context.SESSION_ID = 'B';
  await planPanel.onActiveSessionChanged();
  assert.equal(elements['plan-empty-state'].style.display, '', 'empty state is shown for the plan-less session B');
  assert.equal(viewer.src, '', 'iframe src is cleared');
  assert.equal(elements['plan-viewer'].style.display, 'none', 'viewer is hidden');
});

// ---------------------------------------------------------------------------
// Session-switch residue: a session switch must leave no visible trace of the
// previous session's plans in the panel (empty state + cleared selectors +
// cleared tab dot), with the next tab-open retrying until the new session
// loads. P1–P4 pin the new behavior; P5 guards the same-session fast path.
// ---------------------------------------------------------------------------

test('P1: hook refresh fails on switch, then onTabShown shows empty state before re-fetch and new plans after', async () => {
  const fetchQueue = [];
  const fetch = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1) {
      return new Promise((resolve, reject) => { fetchQueue.push({url, resolve, reject}); });
    }
    return {ok: true, text: async () => ''};
  };
  const elements = makePanelElements();
  const {viewer} = makeRecordingViewer();
  elements['plan-viewer'] = viewer;
  const {context, planPanel} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch,
    sessionId: 'A',
  });

  // Load session A with a plan so the registry has content to clear.
  const refreshA = planPanel.refresh();
  assert.equal(fetchQueue.length, 1, 'refresh fetched A registry');
  fetchQueue[0].resolve({ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'Plan A'})]})});
  await refreshA;
  assert.ok(elements['plan-selector'].innerHTML.indexOf('Plan A') !== -1, 'A rendered in the selector');

  // SPA switch to B; the hook's refresh() fails.
  context.SESSION_ID = 'B';
  const hookPromise = planPanel.onActiveSessionChanged();
  assert.equal(fetchQueue.length, 2, 'hook fetched B registry');
  fetchQueue[1].reject(new Error('boom'));
  await hookPromise;
  // The failed refresh skipped the commit, so A's stale registry remains.
  assert.equal(planPanel.getRegistrySnapshot().plans.length, 1, 'failed refresh left the previous session registry in place');

  // Open the plans tab: onTabShown resets first (empty state), then re-fetches.
  const tabPromise = planPanel.onTabShown();
  assert.equal(planPanel.getRegistrySnapshot().plans.length, 0, 'panel reset to empty state before the re-fetch resolves');
  assert.equal(elements['plan-empty-state'].style.display, '', 'empty state shown before the re-fetch resolves');
  assert.equal(elements['plan-selector'].innerHTML, '', 'selector cleared before the re-fetch resolves');
  assert.equal(fetchQueue.length, 3, 'onTabShown re-fetched the new session registry');
  fetchQueue[2].resolve({ok: true, json: async () => ({plans: [makePlan(9, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'Plan B'})]})});
  await tabPromise;
  const snap = planPanel.getRegistrySnapshot();
  assert.equal(snap.plans.length, 1, 'new session plans rendered after the re-fetch');
  assert.equal(snap.plans[0].id, 9, 'the new session plan is shown, not the previous session');
});

test('P2: SESSION_ID = null then onTabShown clears plan-selector and issues no fetch', async () => {
  const registries = {
    A: {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'Plan A'})]},
  };
  const fetchCalls = [];
  const fetch = async (url) => {
    fetchCalls.push(url);
    if (String(url).indexOf('/api/sessions/') !== -1) {
      const plansFetch = makeSessionFetch(registries);
      return plansFetch(url);
    }
    return {ok: true, text: async () => ''};
  };
  const elements = makePanelElements();
  const {viewer} = makeRecordingViewer();
  elements['plan-viewer'] = viewer;
  const {context, planPanel} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch,
    sessionId: 'A',
  });

  await planPanel.refresh();
  assert.ok(elements['plan-selector'].innerHTML.indexOf('Plan A') !== -1, 'plan-selector populated for A');
  const callsBefore = fetchCalls.length;

  // Session deleted: SESSION_ID goes null. Opening the plans tab must show the
  // empty state, clear the selector, and issue no HTTP request.
  context.SESSION_ID = null;
  await planPanel.onTabShown();
  assert.equal(planPanel.getRegistrySnapshot().plans.length, 0, 'empty registry after session deletion');
  assert.equal(elements['plan-selector'].innerHTML, '', 'plan-selector cleared');
  assert.equal(elements['plan-empty-state'].style.display, '', 'empty state shown');
  assert.equal(fetchCalls.length, callsBefore, 'onTabShown issued no fetch when SESSION_ID is null');
});

test('P3: switch to a session with no plans clears #plan-selector', async () => {
  const registries = {
    A: {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'Plan A'})]},
    B: {plans: []},
  };
  const elements = makePanelElements();
  const {viewer} = makeRecordingViewer();
  elements['plan-viewer'] = viewer;
  const {context, planPanel} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch: makeSessionFetchWithFiles(registries),
    sessionId: 'A',
  });

  await planPanel.refresh();
  assert.ok(elements['plan-selector'].innerHTML.indexOf('Plan A') !== -1, 'plan-selector populated for A');

  context.SESSION_ID = 'B';
  await planPanel.onActiveSessionChanged();
  assert.equal(planPanel.getRegistrySnapshot().plans.length, 0, 'empty registry for plan-less B');
  assert.equal(elements['plan-empty-state'].style.display, '', 'empty state shown for B');
  assert.equal(elements['plan-selector'].innerHTML, '', 'plan-selector cleared for plan-less B');
});

test('P4: tab dot cleared on a session change (hook-success via Edit 2 and hook-skipped via Edit 1 fallback)', async () => {
  const dot = {style: {display: ''}, className: ''};
  const elements = makePanelElements();
  elements['btn-chat-plans'] = {
    querySelector: () => dot,
    appendChild() {},
  };
  const {viewer} = makeRecordingViewer();
  elements['plan-viewer'] = viewer;

  const registries = {
    A: {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'Plan A'})]},
    B: {plans: [makePlan(9, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'Plan B'})]},
  };

  // --- Part A: hook succeeds — Edit 2 clears the dot ---
  dot.style.display = '';
  const {context, planPanel} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch: makeSessionFetchWithFiles(registries),
    sessionId: 'A',
  });
  await planPanel.refresh();
  context.SESSION_ID = 'B';
  await planPanel.onActiveSessionChanged();
  assert.equal(dot.style.display, 'none', 'hook-success path clears the dot (Edit 2)');

  // --- Part B: a path that skips the hook — Edit 1 fallback clears the dot ---
  dot.style.display = '';
  const fetchQueue = [];
  const fetchB = async (url) => {
    if (String(url).indexOf('/api/sessions/') !== -1) {
      return new Promise((resolve, reject) => { fetchQueue.push({url, resolve, reject}); });
    }
    return {ok: true, text: async () => ''};
  };
  const {context: ctxB, planPanel: ppB} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch: fetchB,
    sessionId: 'A',
  });
  // The session changes but onActiveSessionChanged is NOT called (a path that
  // skips the hook). onTabShown detects the mismatch and resets.
  ctxB.SESSION_ID = 'B';
  const tabPromise = ppB.onTabShown();
  assert.equal(dot.style.display, 'none', 'hook-skipped path clears the dot via onTabShown (Edit 1 fallback)');
  assert.equal(fetchQueue.length, 1, 'onTabShown re-fetches after the reset');
  fetchQueue[0].resolve({ok: true, json: async () => ({plans: [makePlan(9, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'Plan B'})]})});
  await tabPromise;
});

test('P5: same session, onTabShown twice — the second call issues no fetch', async () => {
  const fetchCalls = [];
  const fetch = async (url) => {
    fetchCalls.push(url);
    if (String(url).indexOf('/api/sessions/') !== -1) {
      return {ok: true, json: async () => ({plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')])]})};
    }
    return {ok: true, text: async () => ''};
  };
  const elements = makePanelElements();
  const {viewer} = makeRecordingViewer();
  elements['plan-viewer'] = viewer;
  const {planPanel} = loadPlanPanelScript({
    document: makePlanDocument(elements),
    fetch,
    sessionId: 'A',
  });

  // Load A so _loadedSessionId === 'A' (no session mismatch on later tab shows).
  await planPanel.refresh();
  const callsAfterLoad = fetchCalls.length;

  // First onTabShown: _stale is still true (initial), so it re-fetches.
  await planPanel.onTabShown();
  assert.ok(fetchCalls.length > callsAfterLoad, 'first onTabShown fetches while stale');

  // Second onTabShown: _stale now false, same session → fast path, no fetch.
  const callsAfterFirst = fetchCalls.length;
  await planPanel.onTabShown();
  assert.equal(fetchCalls.length, callsAfterFirst, 'second onTabShown issues no fetch (stale fast path survives)');
});
