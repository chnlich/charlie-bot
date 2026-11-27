// ---------------------------------------------------------------------------
// The Plans tab in a headless Chrome at 1280 px and a 390 px phone width. The page uses the Plans tab's own markup
// (web/features/artifacts/templates/artifacts/page_tab_panels.html), plan-panel.js, view-requests, and artifact styles.
// The viewer opens prompts/plan_template.html, which has open trade-offs (div.fork blocks without p.resolved).
//
// The check: no visible button of the Plans tab sits outside its toolbar, so a plan page's open forks draw no question
// button under the viewer.
//
// CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome binary. The test fails when it is unset or names no file.
// CHARLIEBOT_PLANS_SHOTS names a file prefix: each case saves <prefix>_<width>px.png before it asserts.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const {after, before, test} = require('node:test');

const {launchChrome, openPage, stopChrome, until} = require('./session_page_fixture');

const WEB_DIR = path.join(__dirname, '..');
const REPO_DIR = path.join(WEB_DIR, '..');
const PANEL_TEMPLATE_PATH = path.join(WEB_DIR, 'features', 'artifacts', 'templates', 'artifacts',
  'page_tab_panels.html');
const PLAN_PAGE_PATH = path.join(REPO_DIR, 'prompts', 'plan_template.html');
const SESSION_ID = 'plans-session';
const SESSIONS_ROOT_PATH = '/home/tester/.charliebot/sessions';
const PLAN_FILE = 'artifacts/plan_01_v1.html';
const VIEWPORTS = [{width: 1280, height: 800}, {width: 390, height: 844}];
const SHOTS_PREFIX = process.env.CHARLIEBOT_PLANS_SHOTS || null;

// The URLs the page loads, mapped to the files the app serves them from.
const STATIC_FILES = {
  '/static/css/styles.css': path.join(WEB_DIR, 'static', 'css', 'styles.css'),
  '/static/css/tailwind.css': path.join(WEB_DIR, 'static', 'css', 'tailwind.css'),
  '/static/js/view-requests.js': path.join(WEB_DIR, 'static', 'js', 'view-requests.js'),
  '/static/features/artifacts/artifacts.css': path.join(WEB_DIR, 'features', 'artifacts', 'static', 'artifacts.css'),
  '/static/features/artifacts/plan-panel.js': path.join(WEB_DIR, 'features', 'artifacts', 'static', 'plan-panel.js'),
};

// The panel's Jinja: the secondary-button class is set once in the template, and each button reads it.
function renderPanel(markup) {
  const secondary = /\{%\s*set plan_secondary = "([^"]*)"\s*%\}/.exec(markup);
  if (!secondary) throw new Error('page_tab_panels.html sets no plan_secondary class');
  return markup
    .replace(/\{%\s*set plan_secondary = "[^"]*"\s*%\}/, '')
    .replace(/\{\{\s*plan_secondary\s*\}\}/g, secondary[1]);
}

const PANEL_HTML = renderPanel(fs.readFileSync(PANEL_TEMPLATE_PATH, 'utf8'));
const PLAN_PAGE = fs.readFileSync(PLAN_PAGE_PATH, 'utf8');

// The registry row the session's plans endpoint answers: one lineage with one version, whose page has open forks.
const REGISTRY = {
  plans: [{
    id: 1,
    title: 'Plan with open forks',
    state: 'awaiting approval',
    takeoff: null,
    closed: null,
    versions: [{v: 1, file: PLAN_FILE, created_at: '2026-11-19T00:00:00+00:00', trigger: 'initial', base: null}],
  }],
};

// The tab and the viewer are the app's own: the page registers the Plans tab the way the app's tab registry does,
// and the test shows the tab the way a tab switch does.
function pageHtml() {
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="stylesheet" href="/static/css/styles.css">
<link rel="stylesheet" href="/static/css/tailwind.css">
<link rel="stylesheet" href="/static/features/artifacts/artifacts.css">
</head>
<body class="dark bg-slate-900 text-slate-100" style="margin:0">
<div style="height:100vh;display:flex;flex-direction:column">
${PANEL_HTML}
</div>
<script>
window.SESSION_ID = ${JSON.stringify(SESSION_ID)};
window.SESSIONS_ROOT = ${JSON.stringify(SESSIONS_ROOT_PATH)};
const tabs = {};
function registerTab(tab) { tabs[tab.id] = tab; }
function switchTab(id) {
  document.getElementById('tab-plans').classList.toggle('hidden', id !== 'chat-plans');
  tabs[id].onShow();
}
function registerWsConnectHandler() {}
function escapeHtmlAttr(value) {
  return String(value).replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;');
}
</script>
<script src="/static/js/view-requests.js"></script>
<script src="/static/features/artifacts/plan-panel.js"></script>
</body></html>`;
}

function reply(res, status, contentType, body) {
  res.writeHead(status, {'content-type': contentType});
  res.end(body);
}

// Serves the page at /, the session's plan registry, the plan page at the viewer's address, and the static files.
function startServer() {
  const planUrl = `/absolute_filepath${SESSIONS_ROOT_PATH}/${SESSION_ID}/${PLAN_FILE}`;
  const server = http.createServer((req, res) => {
    const {pathname} = new URL(req.url, 'http://127.0.0.1');
    if (req.method !== 'GET') return reply(res, 405, 'text/plain; charset=utf-8', 'method not allowed');
    if (pathname === '/') return reply(res, 200, 'text/html; charset=utf-8', pageHtml());
    if (pathname === `/api/sessions/${SESSION_ID}/plans`) {
      return reply(res, 200, 'application/json; charset=utf-8', JSON.stringify(REGISTRY));
    }
    if (pathname === planUrl) return reply(res, 200, 'text/html; charset=utf-8', PLAN_PAGE);
    const file = STATIC_FILES[pathname];
    if (!file) return reply(res, 404, 'text/plain; charset=utf-8', 'not found');
    const type = file.endsWith('.css') ? 'text/css; charset=utf-8' : 'application/javascript; charset=utf-8';
    return reply(res, 200, type, fs.readFileSync(file));
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({server, base: `http://127.0.0.1:${server.address().port}`}));
  });
}

// Null until the viewer's plan page has loaded; then the number of its open forks.
const VIEWER_STATE = `(() => {
  const viewer = document.getElementById('plan-viewer');
  const doc = viewer.contentDocument;
  if (!viewer.getAttribute('src') || !doc || doc.readyState !== 'complete') return null;
  const forks = [...doc.querySelectorAll('div.fork')];
  return {openForks: forks.filter((fork) => !fork.querySelector('p.resolved')).length};
})()`;

// The Plans tab's visible buttons, split by whether the toolbar holds them.
const TAB_BUTTONS = `(() => {
  const tab = document.getElementById('tab-plans');
  const toolbar = document.getElementById('plan-toolbar');
  const visible = [...tab.querySelectorAll('button')].filter((button) => button.getClientRects().length > 0);
  return {
    tabShown: tab.getClientRects().length > 0,
    toolbarButtons: visible.filter((button) => toolbar.contains(button)).length,
    outsideToolbar: visible.filter((button) => !toolbar.contains(button)).map((button) => button.textContent.trim()),
  };
})()`;

let profileDir = null;
let chrome = null;
let fixture = null;
before(async () => {
  profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'plans-tab-chrome-'));
  chrome = await launchChrome(profileDir);
  fixture = await startServer();
});
after(() => {
  if (chrome) stopChrome(chrome);
  if (fixture) fixture.server.close();
  if (profileDir) fs.rmSync(profileDir, {recursive: true, force: true});
});

async function checkPlansTab({width, height}) {
  const page = await openPage(chrome.cdp, `${fixture.base}/`, {width, height});
  try {
    await until(async () => page.evaluate(`typeof tabs !== 'undefined' && !!tabs['chat-plans']`),
      'the Plans tab to register');
    await page.evaluate(`switchTab('chat-plans')`);
    await until(async () => (await page.evaluate(VIEWER_STATE)) !== null, 'the plan page in the viewer');
    // The panel's load hook runs after the viewer's document loads; the wait lets it finish before the measurement.
    await new Promise((resolve) => setTimeout(resolve, 300));
    const viewer = await page.evaluate(VIEWER_STATE);
    const tab = await page.evaluate(TAB_BUTTONS);
    const {data} = await page.send('Page.captureScreenshot', {format: 'png'});
    if (SHOTS_PREFIX) {
      const file = `${SHOTS_PREFIX}_${width}px.png`;
      fs.mkdirSync(path.dirname(file), {recursive: true});
      fs.writeFileSync(file, Buffer.from(data, 'base64'));
    }
    assert.ok(viewer.openForks >= 1, 'the plan page has an open fork');
    assert.equal(tab.tabShown, true, 'the Plans tab is not shown');
    assert.ok(tab.toolbarButtons >= 1, 'the toolbar draws no button');
    assert.deepEqual(tab.outsideToolbar, [], 'the Plans tab draws a button outside its toolbar');
  } finally {
    await chrome.cdp.send('Target.closeTarget', {targetId: page.targetId});
  }
}

for (const viewport of VIEWPORTS) {
  test(`the Plans tab draws no question button for a plan page with open forks at ${viewport.width} px`,
    {timeout: 120000}, () => checkPlansTab(viewport));
}
