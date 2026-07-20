const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const ARTIFACTS_JS = fs.readFileSync(
  path.join(__dirname, '..', 'web', 'static', 'js', 'chat', 'artifacts.js'),
  'utf8'
);

function escapeHtml(value) {
  return String(value)
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;')
    .replaceAll('"', '&quot;')
    .replaceAll("'", '&#39;');
}

function loadArtifactsScript() {
  const context = {
    SESSION_ID: 'test-session',
    escapeHtml,
    hljs: {highlight: (value) => ({value: escapeHtml(value)})},
    localStorage: {getItem: () => null, setItem: () => {}},
    window: {addEventListener: () => {}, USER_HOME: '/home/user'},
    console,
    URL: globalThis.URL,
  };
  vm.createContext(context);
  vm.runInContext(
    'globalThis.Chat = globalThis.Chat || {};' +
    'globalThis.Chat.expose = function expose(names) {' +
    '  for (var i = 0; i < names.length; i++) globalThis[names[i]] = globalThis.Chat[names[i]];' +
    '};',
    context,
    {filename: 'chat-namespace-stub.js'}
  );
  vm.runInContext(ARTIFACTS_JS, context, {filename: 'artifacts.js'});
  return context;
}

const USER_HOME = '/home/user';
const SESSION_ID = 'sess-42';
const SESSION_DIR = USER_HOME + '/.charliebot/sessions/' + SESSION_ID;

function makeVersion(v, file, verifyState) {
  return {
    v: v,
    file: file || ('artifacts/plan_' + String(v).padStart(2, '0') + '.html'),
    created_at: '2026-07-20T00:00:00+00:00',
    trigger: v === 1 ? 'initial' : 'feedback',
    verify_thread: 'th_' + v,
    verify_state: verifyState || 'pending',
    base: null,
  };
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

// ---------------------------------------------------------------------------
// lookupRegisteredPlanVersion (registered-version lookup)
// ---------------------------------------------------------------------------

test('lookupRegisteredPlanVersion returns the plan+version when absPath matches a registry version file', () => {
  const ctx = loadArtifactsScript();
  const plan = makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')], {title: 'My Plan', state: 'awaiting approval'});
  const snapshot = {plans: [plan]};
  const absPath = SESSION_DIR + '/artifacts/plan_01.html';
  const result = ctx.lookupRegisteredPlanVersion(snapshot, absPath, SESSION_ID, USER_HOME);
  assert.equal(result.planId, 1);
  assert.equal(result.v, 1);
  assert.equal(result.title, 'My Plan');
  assert.equal(result.state, 'awaiting approval');
  assert.equal(result.file, 'artifacts/plan_01.html');
});

test('lookupRegisteredPlanVersion matches the latest of multiple versions by file', () => {
  const ctx = loadArtifactsScript();
  const plan = makePlan(2, [
    makeVersion(1, 'artifacts/plan_01.html'),
    makeVersion(2, 'artifacts/plan_02.html'),
  ], {state: 'in flight'});
  const snapshot = {plans: [plan]};
  const absPath = SESSION_DIR + '/artifacts/plan_02.html';
  const result = ctx.lookupRegisteredPlanVersion(snapshot, absPath, SESSION_ID, USER_HOME);
  assert.equal(result.planId, 2);
  assert.equal(result.v, 2);
  assert.equal(result.file, 'artifacts/plan_02.html');
});

test('lookupRegisteredPlanVersion returns null when the absPath is not in the registry', () => {
  const ctx = loadArtifactsScript();
  const plan = makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')]);
  const snapshot = {plans: [plan]};
  const unregistered = SESSION_DIR + '/artifacts/other_report.html';
  assert.equal(ctx.lookupRegisteredPlanVersion(snapshot, unregistered, SESSION_ID, USER_HOME), null);
});

test('lookupRegisteredPlanVersion returns null for a link to another session dir (never in this registry)', () => {
  const ctx = loadArtifactsScript();
  const plan = makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')]);
  const snapshot = {plans: [plan]};
  const otherSessionDir = USER_HOME + '/.charliebot/sessions/other-sess/artifacts/plan_01.html';
  assert.equal(ctx.lookupRegisteredPlanVersion(snapshot, otherSessionDir, SESSION_ID, USER_HOME), null);
});

test('lookupRegisteredPlanVersion returns null for an empty or missing snapshot', () => {
  const ctx = loadArtifactsScript();
  const absPath = SESSION_DIR + '/artifacts/plan_01.html';
  assert.equal(ctx.lookupRegisteredPlanVersion(null, absPath, SESSION_ID, USER_HOME), null);
  assert.equal(ctx.lookupRegisteredPlanVersion({plans: []}, absPath, SESSION_ID, USER_HOME), null);
  assert.equal(ctx.lookupRegisteredPlanVersion({}, absPath, SESSION_ID, USER_HOME), null);
});

// ---------------------------------------------------------------------------
// decidePlanCardRender (render decision: compact vs legacy)
// ---------------------------------------------------------------------------

test('decidePlanCardRender returns compact when the absPath is a registered plan version', () => {
  const ctx = loadArtifactsScript();
  const snapshot = {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')])]};
  const absPath = SESSION_DIR + '/artifacts/plan_01.html';
  assert.equal(ctx.decidePlanCardRender(snapshot, absPath, SESSION_ID, USER_HOME), 'compact');
});

test('decidePlanCardRender returns legacy when the absPath is not registered', () => {
  const ctx = loadArtifactsScript();
  const snapshot = {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html')])]};
  const absPath = SESSION_DIR + '/artifacts/other.html';
  assert.equal(ctx.decidePlanCardRender(snapshot, absPath, SESSION_ID, USER_HOME), 'legacy');
});

test('decidePlanCardRender returns legacy when there is no snapshot (planPanel unavailable)', () => {
  const ctx = loadArtifactsScript();
  const absPath = SESSION_DIR + '/artifacts/plan_01.html';
  assert.equal(ctx.decidePlanCardRender(null, absPath, SESSION_ID, USER_HOME), 'legacy');
});

// ---------------------------------------------------------------------------
// lookupPlanVersionState (in-place badge update on a registry refresh)
// ---------------------------------------------------------------------------

test('lookupPlanVersionState returns the live state for a plan+version from the snapshot', () => {
  const ctx = loadArtifactsScript();
  const before = {plans: [makePlan(1, [makeVersion(1)], {state: 'in flight'})]};
  const info = ctx.lookupPlanVersionState(before, 1, 1);
  assert.equal(info.planId, 1);
  assert.equal(info.v, 1);
  assert.equal(info.title, 'Plan 1');
  assert.equal(info.state, 'in flight');
  assert.equal(info.file, 'artifacts/plan_01.html');
});

test('lookupPlanVersionState returns null for an unknown plan or version', () => {
  const ctx = loadArtifactsScript();
  const snapshot = {plans: [makePlan(1, [makeVersion(1)])]};
  assert.equal(ctx.lookupPlanVersionState(snapshot, 99, 1), null);
  assert.equal(ctx.lookupPlanVersionState(snapshot, 1, 99), null);
  assert.equal(ctx.lookupPlanVersionState(null, 1, 1), null);
});

test('lookupPlanVersionState reflects the refreshed state after a plan_updated refresh (badge update)', () => {
  const ctx = loadArtifactsScript();
  // Before refresh: verify_state pending → derived "in flight".
  const before = {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html', 'pending')], {state: 'in flight'})]};
  // After refresh: verify_state clean → derived "awaiting approval".
  const after = {plans: [makePlan(1, [makeVersion(1, 'artifacts/plan_01.html', 'clean')], {state: 'awaiting approval'})]};

  const beforeInfo = ctx.lookupPlanVersionState(before, 1, 1);
  assert.equal(beforeInfo.state, 'in flight');

  const afterInfo = ctx.lookupPlanVersionState(after, 1, 1);
  assert.equal(afterInfo.state, 'awaiting approval');

  // The badge update reads the server string verbatim from the refreshed snapshot.
  assert.notEqual(beforeInfo.state, afterInfo.state);
});

// ---------------------------------------------------------------------------
// buildPlanCompactCardHtml (compact card content)
// ---------------------------------------------------------------------------

test('buildPlanCompactCardHtml includes the title, vN, verbatim state, and Open panel button', () => {
  const ctx = loadArtifactsScript();
  const html = ctx.buildPlanCompactCardHtml(3, 2, 'Refactor the registry', 'awaiting approval', '/abs/path/plan.html');
  assert.match(html, /class="plan-compact-card html-artifact"/, 'card carries plan-compact-card and html-artifact classes');
  assert.match(html, /data-artifact-path="\/abs\/path\/plan\.html"/, 'card carries the artifact abs path for dedup');
  assert.match(html, /data-plan-card-plan="3"/, 'card carries the plan id');
  assert.match(html, /data-plan-card-version="2"/, 'card carries the version number');
  assert.match(html, /data-plan-card-abs-path="\/abs\/path\/plan\.html"/, 'card carries the abs path for badge updates');
  assert.match(html, /<span class="filename">Refactor the registry<\/span>/, 'title rendered');
  assert.match(html, /<span class="plan-compact-version">v2<\/span>/, 'version rendered as vN');
  assert.match(html, /<span class="plan-compact-state">awaiting approval<\/span>/, 'state rendered verbatim');
  assert.match(html, /<button[^>]*onclick="openPlanFromCard\(this\)"[^>]*>Open panel<\/button>/, 'Open panel button present');
});

test('buildPlanCompactCardHtml includes an Open in tab anchor whose href carries #cbsession= and no cbpanel', () => {
  const ctx = loadArtifactsScript();
  const absPath = '/home/user/.charliebot/sessions/sess-42/artifacts/plan_03.html';
  const html = ctx.buildPlanCompactCardHtml(3, 2, 'Refactor the registry', 'awaiting approval', absPath);
  // Anchor is present, opens in a new tab with the standard rel attributes.
  const anchorMatch = html.match(/<a[^>]*href="([^"]+)"[^>]*target="_blank"[^>]*rel="noopener noreferrer"[^>]*>Open in tab<\/a>/);
  assert.ok(anchorMatch, 'Open in tab anchor present with target=_blank and rel=noopener noreferrer');
  const href = anchorMatch[1];
  // stampViewingSessionFragment reads SESSION_ID from the script context.
  assert.match(href, /#cbsession=test-session/, 'href carries the #cbsession= fragment for the comment tray');
  assert.equal(href.indexOf('cbpanel'), -1, 'href must NOT carry the cbpanel marker (standalone page path)');
  assert.ok(href.indexOf('/files' + absPath) === 0, 'href targets the real /files URL of the card version file');
});

test('buildPlanCompactCardHtml renders the derived state string verbatim with no client-side derivation', () => {
  const ctx = loadArtifactsScript();
  const states = ['in flight', 'awaiting approval', 'needs amendment', 'approved', 'approved \u00B7 awaiting clean verify', 'superseded'];
  for (const state of states) {
    const html = ctx.buildPlanCompactCardHtml(1, 1, 'P', state, '/x.html');
    const badgeMatch = html.match(/<span class="plan-compact-state">([^<]*)<\/span>/);
    assert.ok(badgeMatch, 'badge span present for state ' + state);
    assert.equal(badgeMatch[1], state, 'state string rendered verbatim for ' + state);
  }
});

test('buildPlanCompactCardHtml falls back to (untitled) when the title is missing', () => {
  const ctx = loadArtifactsScript();
  const html = ctx.buildPlanCompactCardHtml(1, 1, null, 'in flight', '/x.html');
  assert.match(html, /<span class="filename">\(untitled\)<\/span>/, 'missing title renders as (untitled)');
});
