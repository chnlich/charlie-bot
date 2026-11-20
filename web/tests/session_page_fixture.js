// ---------------------------------------------------------------------------
// The session page's chat column in a headless Chrome, built from the page's own files: the header and the tab column
// of web/templates/index.html, the cron panel part of web/features/cron/templates/cron/page_below_header.html, the
// inline styles.css and the tailwind.css the page links. Jinja tags are removed, and an icon call leaves a box of its
// size. cron.js fills the panel from GET /api/cron/tasks, which this server answers from the tasks the caller gives.
//
// CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome binary. launchChrome throws when it is unset or names no file.
// ---------------------------------------------------------------------------
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const {spawn} = require('node:child_process');

const {readPackageStatic} = require('./read_static');

const WEB_DIR = path.join(__dirname, '..');
const INDEX_HTML = fs.readFileSync(path.join(WEB_DIR, 'templates', 'index.html'), 'utf8');

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

// The page of a session: its header, the cron panel part, and the tab column, in the order index.html places them.
function sessionPageHtml(sessionId) {
  const styles = fs.readFileSync(path.join(WEB_DIR, 'static', 'css', 'styles.css'), 'utf8');
  const header = renderJinja(section(INDEX_HTML, '<header class="flex items-center border-b', '</header>'));
  const panel = renderJinja(readPackageStatic('features', 'cron', 'page_below_header.html'));
  const column = renderJinja(section(INDEX_HTML, '<div class="flex-1 flex min-h-0">',
    '<!-- end tab content wrapper -->'));
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>${styles}</style>
<link rel="stylesheet" href="/static/css/tailwind.css">
</head>
<body class="dark bg-slate-900 text-slate-100 overflow-hidden" style="height:var(--app-h,100dvh)">
<div class="flex" style="height:var(--app-h,100dvh)">
<main class="flex-1 flex flex-col min-w-0">
${header}
${panel}
${column}
</main>
</div>
<script>
let SESSION_ID = ${JSON.stringify(sessionId)};
// The sidebar registers these when it loads. This page has no sidebar, so each registration is a no-op.
window.Sidebar = {wire() {}, repaintRenderedRows() {}, refreshWorkspaceBadges() {}};
window.registerWorkspaceBadge = () => {};
window.registerGroupPreviewLimitExemption = () => {};
window.registerSessionRowBadge = () => {};
window.registerSessionRowLine = () => {};
// The page's escapeHtml (web/static/js/chat/shared.js), which cron.js calls.
function escapeHtml(str) {
  const d = document.createElement('div');
  d.textContent = str;
  return d.innerHTML;
}
</script>
<script src="/features/cron/static/cron.js"></script>
<script>document.dispatchEvent(new Event('charliebot:active-session-changed'));</script>
</body></html>`;
}

function reply(res, status, contentType, body) {
  res.writeHead(status, {'content-type': contentType});
  res.end(body);
}

// Serves the session page at / and GET /api/cron/tasks answered with `tasks`. `served.cronTasks` counts the task
// reads, so a caller waits for the panel's fetch to land before it measures.
function startFixtureServer({sessionId, tasks}) {
  const page = sessionPageHtml(sessionId);
  const served = {cronTasks: 0};
  const server = http.createServer((req, res) => {
    const {pathname} = new URL(req.url, 'http://127.0.0.1');
    if (req.method !== 'GET') return reply(res, 405, 'text/plain; charset=utf-8', 'method not allowed');
    if (pathname === '/') return reply(res, 200, 'text/html; charset=utf-8', page);
    if (pathname === '/api/cron/tasks') {
      served.cronTasks += 1;
      return reply(res, 200, 'application/json; charset=utf-8', JSON.stringify(tasks));
    }
    const file = path.normalize(path.join(WEB_DIR, pathname));
    const roots = [path.join(WEB_DIR, 'static'), path.join(WEB_DIR, 'features')];
    if (!roots.some((root) => file.startsWith(root + path.sep)) || !fs.existsSync(file)) {
      return reply(res, 404, 'text/plain; charset=utf-8', 'not found');
    }
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
  sessionPageHtml,
  startFixtureServer,
  launchChrome,
  stopChrome,
  openPage,
  until,
};
