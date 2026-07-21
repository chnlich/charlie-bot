const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const PLAN_PANEL_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'plan-panel.js'),
  'utf8'
);

function loadPlanPanelScript(opts = {}) {
  const noop = () => {};
  const document = opts.document || {
    readyState: 'loading',
    addEventListener: noop,
    getElementById: () => null,
  };
  const context = {
    SESSION_ID: opts.sessionId || 'test-session',
    USER_HOME: opts.userHome || '/home/user',
    console: {error: noop, log: noop},
    document,
    window: {USER_HOME: opts.userHome || '/home/user'},
    localStorage: {getItem: () => null, setItem: noop},
    fetch: opts.fetch || (async () => {
      throw new Error('fetch should not run during script load');
    }),
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
  };
  const fetch = async () => ({
    ok: true,
    json: async () => ({plans: [makePlan(1, [makeVersion(1), makeVersion(2)])]}),
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
  return {
    querySelector(selector) {
      if (selector === 'span.fn') return o.fn === false ? null : {textContent: String(o.fn != null ? o.fn : '1')};
      if (selector === 'p.q') return o.q === false ? null : {textContent: o.q || 'Question?'};
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
  const {planPanel} = loadPlanPanelScript({sessionId: 'sess-42', userHome: '/home/alice'});
  const url = planPanel.buildIframeUrl('artifacts/plan_01.html', 'sess-42', '/home/alice');
  const expectedPath = '/files/home/alice/.charliebot/sessions/sess-42/artifacts/plan_01.html';
  assert.ok(url.startsWith(expectedPath), 'URL starts with the real /files path');
  assert.ok(url.indexOf('#cbsession=sess-42') !== -1, 'URL carries the cbsession fragment');
  assert.ok(url.indexOf('&cbpanel=1') !== -1, 'URL carries the cbpanel=1 marker');
});

test('buildIframeUrlFromVersion resolves the correct version file', () => {
  const {planPanel} = loadPlanPanelScript({sessionId: 's1', userHome: '/home/u'});
  const plan = makePlan(1, [
    makeVersion(1, 'artifacts/plan_01.html'),
    makeVersion(2, 'artifacts/plan_02.html'),
  ]);
  const url = planPanel.buildIframeUrlFromVersion(plan, 2, 's1', '/home/u');
  assert.ok(url.indexOf('artifacts/plan_02.html') !== -1, 'URL points to v2 file');
  assert.ok(url.indexOf('&cbpanel=1') !== -1, 'URL carries the panel marker');
});

test('buildIframeUrlFromVersion returns null for unknown version', () => {
  const {planPanel} = loadPlanPanelScript();
  const plan = makePlan(1, [makeVersion(1)]);
  assert.equal(planPanel.buildIframeUrlFromVersion(plan, 99, 's1', '/home/u'), null);
  assert.equal(planPanel.buildIframeUrlFromVersion(null, 1, 's1', '/home/u'), null);
});

// ---------------------------------------------------------------------------
// buildStandaloneUrl / buildStandaloneUrlFromVersion (Open in tab path)
// ---------------------------------------------------------------------------

test('buildStandaloneUrl builds a real /files URL with cbsession and NO cbpanel marker', () => {
  const {planPanel} = loadPlanPanelScript({sessionId: 'sess-42', userHome: '/home/alice'});
  const url = planPanel.buildStandaloneUrl('artifacts/plan_01.html', 'sess-42', '/home/alice');
  const expectedPath = '/files/home/alice/.charliebot/sessions/sess-42/artifacts/plan_01.html';
  assert.ok(url.startsWith(expectedPath), 'URL starts with the real /files path');
  assert.ok(url.indexOf('#cbsession=sess-42') !== -1, 'URL carries the cbsession fragment');
  assert.equal(url.indexOf('cbpanel'), -1, 'standalone URL must NOT carry the cbpanel marker');
});

test('buildStandaloneUrlFromVersion resolves the version file without cbpanel', () => {
  const {planPanel} = loadPlanPanelScript({sessionId: 's1', userHome: '/home/u'});
  const plan = makePlan(1, [
    makeVersion(1, 'artifacts/plan_01.html'),
    makeVersion(2, 'artifacts/plan_02.html'),
  ]);
  const url = planPanel.buildStandaloneUrlFromVersion(plan, 2, 's1', '/home/u');
  assert.ok(url.indexOf('artifacts/plan_02.html') !== -1, 'URL points to v2 file');
  assert.ok(url.indexOf('#cbsession=s1') !== -1, 'URL carries the cbsession fragment');
  assert.equal(url.indexOf('cbpanel'), -1, 'standalone URL must NOT carry the cbpanel marker');
});

test('buildStandaloneUrlFromVersion returns null for unknown version', () => {
  const {planPanel} = loadPlanPanelScript();
  const plan = makePlan(1, [makeVersion(1)]);
  assert.equal(planPanel.buildStandaloneUrlFromVersion(plan, 99, 's1', '/home/u'), null);
  assert.equal(planPanel.buildStandaloneUrlFromVersion(null, 1, 's1', '/home/u'), null);
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
