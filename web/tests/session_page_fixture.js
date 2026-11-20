// ---------------------------------------------------------------------------
// The session page's chat column in a headless Chrome, built from the page's own files: the header, the tab column and
// the Task & context dialog of web/templates/index.html, the usage quota strip the page's below_header slot carries, the
// inline styles.css and the tailwind.css the page links, and the scripts the dialog and its Scheduled task section run
// on: the markdown renderer, the sidebar namespace, the dialog's script, the cron module and the usage module. Jinja tags
// are removed, and an icon call leaves a box of its size. GET /api/ext-usage answers the payload the caller gives.
//
// The fixture serves two session modes. A bound session is SESSION_ID, and the page's task list holds one prompt task
// bound to it, whose prompt carries four fenced code blocks. An unbound session is the same page with an empty task list.
//
// CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome binary. launchChrome throws when it is unset or names no file.
// ---------------------------------------------------------------------------
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const {spawn} = require('node:child_process');

const {readPackageStatic} = require('./read_static');
const {MARKED_URL, fetchUrl} = require('./marked_renderer_harness');

const WEB_DIR = path.join(__dirname, '..');
const INDEX_HTML = fs.readFileSync(path.join(WEB_DIR, 'templates', 'index.html'), 'utf8');
// The page loads marked from MARKED_URL, the build the renderer's Goal and prompt prose both need. The fixture serves
// that same build from MARKED_PATH.
const MARKED_PATH = '/vendor/marked.min.js';
const MATH_SCANNER_PATH = '/static/js/math-scanner.js';
const MARKDOWN_RENDERER_PATH = '/static/js/markdown-renderer.js';
const NAMESPACE_PATH = '/static/js/sidebar/namespace.js';
const MODALS_PATH = '/static/js/sidebar/modals.js';
const CRON_MODULE_PATH = '/features/cron/static/cron.js';
const USAGE_MODULE_PATH = '/features/usage/static/ext_usage.js';
const USAGE_STYLE_PATH = '/features/usage/static/usage.css';

const SESSION_ID = 'session-under-test';
const BOUND_PROMPT = [
  '# Weekly attention review',
  '',
  'Read the notes from the week first.',
  '',
  '```bash',
  'ls notes/',
  '```',
  '',
  'List the open items:',
  '',
  '```text',
  '- item one',
  '```',
  '',
  'Summarize each item:',
  '',
  '```python',
  'print("summary")',
  '```',
  '',
  'End with the result:',
  '',
  '```json',
  '{"done": true}',
  '```',
].join('\n');
const BOUND_TASK = {
  name: 'weekly-attention-review', cron: '0 7 * * 1', timezone: 'UTC', enabled: true, session_id: SESSION_ID,
  prompt_file: 'dev/weekly_attention_review.md', prompt: BOUND_PROMPT, repo: null, allow_failure: false, mode: 'master',
  handler: null, improve_host: null, steps: null, broken: false,
};
// The Goal carries one code block, the reference each of the prompt's code blocks must paint like.
const SESSION_DETAIL = {
  id: SESSION_ID, name: 'Session under test', profile: 'manager',
  web_status: {goal: 'Check the build:\n\n```text\nnpm test\n```', now: null, waiting: []},
};
const SESSION_PREVIEW = {
  session_id: SESSION_ID, kind: 'manager_turn', mode: 'preview', overlay: {declared: true, error: null}, blocks: [],
  char_count: 0,
};

// The minimal hljs surface markdown-renderer.js reads when it paints a code block. The fixture highlights nothing.
const HLJS_STUB = `window.hljs = {
  getLanguage: () => null,
  highlightAuto: (s) => ({value: String(s)}),
  highlight: (s) => ({value: String(s)}),
};`;

// The text from the start marker through the end marker, both included.
function section(text, startMarker, endMarker) {
  const start = text.indexOf(startMarker);
  if (start < 0) throw new Error(`index.html has no ${startMarker}`);
  const end = text.indexOf(endMarker, start);
  if (end < 0) throw new Error(`index.html has no ${endMarker} after ${startMarker}`);
  return text.slice(start, end + endMarker.length);
}

// The markup Jinja renders: comments and tags go, and an icon call becomes an empty box of its size.
function renderJinja(markup) {
  return markup
    .replace(/\{#[\s\S]*?#\}/g, '')
    .replace(/\{\{\s*icon\('[^']*',\s*(\d+)[^}]*\}\}/g,
      (_, size) => `<span style="display:inline-block;width:${size}px;height:${size}px"></span>`)
    .replace(/\{\{[\s\S]*?\}\}/g, '')
    .replace(/\{%[\s\S]*?%\}/g, '');
}

// The Task & context dialog's overlay: from its opening tag to the blank line that ends its block in index.html.
function dialogHtml() {
  const start = INDEX_HTML.indexOf('<div id="task-context-modal-overlay"');
  if (start < 0) throw new Error('index.html has no task-context-modal-overlay');
  const end = INDEX_HTML.indexOf('\n\n', start);
  return renderJinja(INDEX_HTML.slice(start, end));
}

// The page: its header and tab column, in the order index.html places them, then the dialog and its scripts.
function sessionPageHtml() {
  const styles = fs.readFileSync(path.join(WEB_DIR, 'static', 'css', 'styles.css'), 'utf8');
  const header = renderJinja(section(INDEX_HTML, '<header class="flex items-center border-b', '</header>'));
  // The page's below_header slot: the usage quota strip is the part that fills it.
  const usageStrip = renderJinja(readPackageStatic('features', 'usage', 'page_below_header.html'));
  const column = renderJinja(section(INDEX_HTML, '<div class="flex-1 flex min-h-0">',
    '<!-- end tab content wrapper -->'));
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>${styles}</style>
<link rel="stylesheet" href="/static/css/tailwind.css">
<link rel="stylesheet" href="${USAGE_STYLE_PATH}">
</head>
<body class="dark bg-slate-900 text-slate-100 overflow-hidden" style="height:var(--app-h,100dvh)">
<div class="flex" style="height:var(--app-h,100dvh)">
<main class="flex-1 flex flex-col min-w-0">
${header}
${usageStrip}
${column}
</main>
</div>
${dialogHtml()}
<script>
// The sidebar's row registrations. This page has no sidebar rows, so each registration is a no-op.
window.registerWorkspaceBadge = () => {};
window.registerGroupPreviewLimitExemption = () => {};
window.registerSessionRowBadge = () => {};
window.registerSessionRowLine = () => {};
window.SESSION_ID = ${JSON.stringify(SESSION_ID)};
// The page's websocket and timer helpers, and the icon call the usage strip's chip draws, which this page lacks. The
// meter-fill class is config.js's constant; the fixture leaves config.js out because it reads server-rendered values.
window.registerWsMessageHandler = () => {};
window.startPageTimer = () => {};
window.PROGRESS_BAR_FILL_CLASS = 'h-full rounded-full transition-all duration-300';
window.cbIcon = (key, size) => '<span style="display:inline-block;width:' + size + 'px;height:' + size + 'px"></span>';
// The page's escapeHtml (web/static/js/chat/shared.js), which cron.js and the dialog call.
function escapeHtml(str) {
  const d = document.createElement('div');
  d.textContent = str;
  return d.innerHTML;
}
${HLJS_STUB}
</script>
<script src="${MARKED_PATH}"></script>
<script src="${MATH_SCANNER_PATH}"></script>
<script src="${MARKDOWN_RENDERER_PATH}"></script>
<script src="${NAMESPACE_PATH}"></script>
<script src="${MODALS_PATH}"></script>
<script src="${USAGE_MODULE_PATH}"></script>
<script src="${CRON_MODULE_PATH}"></script>
</body></html>`;
}

function reply(res, status, contentType, body) {
  res.writeHead(status, {'content-type': contentType});
  res.end(body);
}

// The marked build the page loads, fetched once per test process from MARKED_URL.
let markedSource = null;
async function loadMarkedSource() {
  if (!markedSource) markedSource = await fetchUrl(MARKED_URL);
  return markedSource;
}

// Serves the session page at /, GET /api/ext-usage answered with `usage`, the session's task list and detail (the bound
// mode serves one prompt task for SESSION_ID, the unbound mode none), and the files the page links under web/static and
// web/features. `served.cronModule` counts the cron module's fetches and `served.extUsage` the usage reads, so a caller
// waits for the modules to load before it measures.
async function startFixtureServer({usage = {providers: {}}, bound = false} = {}) {
  const page = sessionPageHtml();
  const marked = await loadMarkedSource();
  const cronTasks = bound ? [BOUND_TASK] : [];
  const served = {cronModule: 0, extUsage: 0};
  const server = http.createServer((req, res) => {
    const {pathname} = new URL(req.url, 'http://127.0.0.1');
    if (req.method !== 'GET') return reply(res, 405, 'text/plain; charset=utf-8', 'method not allowed');
    if (pathname === '/') return reply(res, 200, 'text/html; charset=utf-8', page);
    if (pathname === MARKED_PATH) return reply(res, 200, 'application/javascript; charset=utf-8', marked);
    if (pathname === '/api/ext-usage') {
      served.extUsage += 1;
      return reply(res, 200, 'application/json; charset=utf-8', JSON.stringify(usage));
    }
    if (pathname === '/api/cron/tasks') {
      return reply(res, 200, 'application/json; charset=utf-8', JSON.stringify(cronTasks));
    }
    if (pathname === `/api/sessions/${SESSION_ID}`) {
      return reply(res, 200, 'application/json; charset=utf-8', JSON.stringify(SESSION_DETAIL));
    }
    if (pathname === `/api/sessions/${SESSION_ID}/effective-prompt`) {
      return reply(res, 200, 'application/json; charset=utf-8', JSON.stringify(SESSION_PREVIEW));
    }
    const file = path.normalize(path.join(WEB_DIR, pathname));
    const roots = [path.join(WEB_DIR, 'static'), path.join(WEB_DIR, 'features')];
    if (!roots.some((root) => file.startsWith(root + path.sep)) || !fs.existsSync(file)) {
      return reply(res, 404, 'text/plain; charset=utf-8', 'not found');
    }
    if (pathname === CRON_MODULE_PATH) served.cronModule += 1;
    const type = file.endsWith('.css') ? 'text/css; charset=utf-8' : 'application/javascript; charset=utf-8';
    return reply(res, 200, type, fs.readFileSync(file));
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({server, served, base: `http://127.0.0.1:${server.address().port}`}));
  });
}

async function launchChrome(profileDir) {
  const binary = process.env.CHARLIEBOT_TEST_CHROME_BIN;
  if (!binary) throw new Error('CHARLIEBOT_TEST_CHROME_BIN must name a headless Chrome binary');
  if (!fs.existsSync(binary)) throw new Error(`CHARLIEBOT_TEST_CHROME_BIN names a missing file: ${binary}`);
  const {CdpConnection} = await import('../../dev/poor-network/chrome.mjs');
  const child = spawn(binary, [
    '--headless=new',
    '--no-sandbox',
    '--disable-gpu',
    '--no-first-run',
    '--no-default-browser-check',
    `--user-data-dir=${profileDir}`,
    '--remote-debugging-pipe',
    '--window-size=1280,900',
    'about:blank',
  ], {detached: true, stdio: ['ignore', 'ignore', 'ignore', 'pipe', 'pipe']});
  const exited = new Promise((_, reject) => {
    child.once('exit', (code, signal) => reject(new Error(`Chrome exited (${code ?? signal}) before it answered`)));
  });
  const cdp = new CdpConnection(child.stdio[3], child.stdio[4]);
  await Promise.race([cdp.send('Browser.getVersion'), exited]);
  return {child, cdp};
}

function stopChrome({child, cdp}) {
  cdp.close();
  try {
    process.kill(-child.pid, 'SIGKILL');
  } catch (err) {
    if (err.code !== 'ESRCH') throw err;
  }
}

// A page at one viewport, loaded at `url`. The metrics are set before the navigation, so the page lays out at them.
async function openPage(cdp, url, {width, height}) {
  const {targetId} = await cdp.send('Target.createTarget', {url: 'about:blank'});
  const {sessionId} = await cdp.send('Target.attachToTarget', {targetId, flatten: true});
  const {Page} = await import('../../dev/poor-network/chrome.mjs');
  const page = new Page(cdp, sessionId, targetId);
  await page.send('Page.enable');
  await page.send('Runtime.enable');
  await page.send('Emulation.setDeviceMetricsOverride',
    {width, height, deviceScaleFactor: 1, mobile: width < 500});
  await page.send('Page.navigate', {url});
  return page;
}

async function until(check, what) {
  const deadline = Date.now() + 20000;
  while (!(await check())) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

module.exports = {
  SESSION_ID,
  sessionPageHtml,
  startFixtureServer,
  launchChrome,
  stopChrome,
  openPage,
  until,
};
