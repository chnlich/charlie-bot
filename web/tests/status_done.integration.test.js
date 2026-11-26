// ---------------------------------------------------------------------------
// The done checkbox of a Waiting on you item, clicked in its owner's Status view in a headless Chrome. The page loads
// the real chat/status-panel.js, comment_post.js, view-requests.js and icons.js with a stubbed Chat namespace and
// fetch, the way the chat page loads them, and answers GET /api/internal/web/status with a parent whose own stored
// item has no answer and whose one open child holds an item.
//
// Checks:
// - the own item's row holds one checkbox, and the child's item holds none;
// - checking the box and unchecking it inside the undo window sends nothing;
// - checking it again sends, after the undo window, one click message to the owner in the landed format: the header,
//   the reference with the list version and the position, and the answer Done. The message carries neither the item's
//   text nor its context;
// - at desktop width the Waiting on you label is drawn in the label column the Now row uses, level with the first
//   item's line and before it, and the heading does not stretch to the row's height;
// - at mobile width the label is drawn above the first item, at the row's left edge.
// The page loads the built tailwind.css, so the layout is the app's.
//
// CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome binary. The test fails when it is unset or names no file.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const {spawn} = require('node:child_process');
const test = require('node:test');

const {iconTableScript} = require('./read_static');

const WEB_DIR = path.join(__dirname, '..');
const PARENT = 'parent-session';
const CHILD = 'child-session';
const SINCE = '2026-11-19T08:00:00Z';
const OWN_VERSION = 'aaaaaaaaaaaaaaaa';
const CHILD_VERSION = 'cccccccccccccccc';
const PAGE_PATH = '/chat';
// The undo window is five seconds; the wait leaves room for the post.
const SEND_WAIT_MS = 9000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// The status read's answer: the parent's own stored item, and its one child's item.
const STATUS = {
  session_id: PARENT,
  own_task_state: 'open',
  web_status: {
    goal: null,
    now: null,
    remaining: [],
    waiting: [{text: 'Own decision', context: 'Own context', link: null, since: SINCE, answered: false}],
    waiting_version: OWN_VERSION,
    updated_at: '2026-11-19T08:05:00Z',
  },
  descendants: [{
    session_id: CHILD,
    name: 'Child',
    updated_at: '2026-11-19T08:05:00Z',
    waiting_version: CHILD_VERSION,
    waiting: [{text: 'Child decision', context: 'Child context', link: null, since: SINCE, answered: false}],
  }],
  children: [],
  derived_waiting: [],
};

// The page's platform stub: mobile is the platform.isMobile the status panel reads for its layout.
function pageHtml(mobile) {
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>Status done</title>
<link rel="stylesheet" href="/static/css/tailwind.css"></head><body>
<div id="status-panel"></div>
<script>${iconTableScript()}</script>
<script src="/static/js/icons.js"></script>
<script src="/static/js/comment_post.js"></script>
<script src="/static/js/view-requests.js"></script>
<script>
  window.__posts = [];
  const status = ${JSON.stringify(STATUS)};
  window.platform = {onChange() {}, isMobile: ${mobile}};
  function relativeTime() { return 'now'; }
  window.Chat = {
    // As the page's chat namespace does, a wired module's functions become globals: shared.js's label function
    // is read by the status panel's child rows.
    wire(fns) { window.__status = fns; Object.assign(window, fns); },
    ForYou: {isWebSession: () => true},
    disclosureIcon: () => '',
  };
  window.fetch = async (url, options = {}) => {
    if (String(url).startsWith('/api/internal/web/status')) {
      return {ok: true, status: 200, json: async () => status};
    }
    if (String(url).startsWith('/api/chat/')) {
      window.__posts.push({url: String(url), body: JSON.parse(options.body)});
      return {ok: true, status: 202, json: async () => ({})};
    }
    return {ok: true, status: 200, json: async () => []};
  };
</script>
<script src="/static/js/chat/shared.js"></script>
<script src="/static/js/chat/status-panel.js"></script>
</body></html>`;
}

function serve(mobile) {
  const page = pageHtml(mobile);
  const statics = {
    '/static/js/icons.js': path.join(WEB_DIR, 'static', 'js', 'icons.js'),
    '/static/js/comment_post.js': path.join(WEB_DIR, 'static', 'js', 'comment_post.js'),
    '/static/js/view-requests.js': path.join(WEB_DIR, 'static', 'js', 'view-requests.js'),
    '/static/js/chat/shared.js': path.join(WEB_DIR, 'static', 'js', 'chat', 'shared.js'),
    '/static/js/chat/status-panel.js': path.join(WEB_DIR, 'static', 'js', 'chat', 'status-panel.js'),
    '/static/css/tailwind.css': path.join(WEB_DIR, 'static', 'css', 'tailwind.css'),
  };
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1');
    if (req.method === 'GET' && url.pathname === PAGE_PATH) {
      res.writeHead(200, {'Content-Type': 'text/html; charset=utf-8'});
      res.end(page);
      return;
    }
    if (req.method === 'GET' && statics[url.pathname]) {
      // A stylesheet served as text/javascript is ignored by Chrome, so the type follows the file.
      const type = url.pathname.endsWith('.css') ? 'text/css; charset=utf-8' : 'text/javascript; charset=utf-8';
      res.writeHead(200, {'Content-Type': type});
      res.end(fs.readFileSync(statics[url.pathname], 'utf8'));
      return;
    }
    res.writeHead(404, {'Content-Type': 'text/plain; charset=utf-8'});
    res.end('not found');
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({server, base: `http://127.0.0.1:${server.address().port}`}));
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
    'about:blank',
  ], {detached: true, stdio: ['ignore', 'ignore', 'ignore', 'pipe', 'pipe']});
  const exited = new Promise((_, reject) => {
    child.once('exit', (code, signal) => reject(new Error(`Chrome exited (${code ?? signal}) before it answered`)));
  });
  const cdp = new CdpConnection(child.stdio[3], child.stdio[4]);
  await Promise.race([cdp.send('Browser.getVersion'), exited]);
  return {child, cdp};
}

async function openPage(cdp, url) {
  const {Page} = await import('../../dev/poor-network/chrome.mjs');
  const {targetId} = await cdp.send('Target.createTarget', {url: 'about:blank'});
  const {sessionId} = await cdp.send('Target.attachToTarget', {targetId, flatten: true});
  const page = new Page(cdp, sessionId, targetId);
  await page.send('Page.enable');
  await page.send('Runtime.enable');
  await page.send('Page.navigate', {url});
  const deadline = Date.now() + 15000;
  while (!(await page.evaluate('typeof window.__status === "object"'))) {
    if (Date.now() > deadline) throw new Error('timed out waiting for the status panel module');
    await sleep(50);
  }
  return page;
}

// Waits until the status read has answered and the own item's checkbox is on the page.
async function waitForCheckbox(page) {
  const deadline = Date.now() + 15000;
  const ready = 'document.querySelectorAll("[data-waiting-item] [data-done-check]").length === 1';
  while (!(await page.evaluate(ready))) {
    if (Date.now() > deadline) throw new Error('timed out waiting for the own done checkbox');
    await sleep(50);
  }
}

// Runs one check against the served Status view of the parent, then tears everything down. mobile selects the
// platform stub's layout; the check sets the viewport.
async function withStatusPanel(check, {mobile = false} = {}) {
  const {server, base} = await serve(mobile);
  const profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'status-done-chrome-'));
  let chrome = null;
  try {
    chrome = await launchChrome(profileDir);
    const page = await openPage(chrome.cdp, `${base}${PAGE_PATH}`);
    await page.evaluate('window.__status.openSessionStatusPanel({id: ' + JSON.stringify(PARENT) + '})');
    await waitForCheckbox(page);
    await check(page);
  } finally {
    if (chrome) {
      chrome.cdp.close();
      try {
        process.kill(-chrome.child.pid, 'SIGKILL');
      } catch (error) {
        if (error.code !== 'ESRCH') throw error;
      }
    }
    server.closeAllConnections();
    server.close();
    fs.rmSync(profileDir, {recursive: true, force: true});
  }
}

test('a done checkbox sends one click after its undo window; unchecking inside the window sends nothing', async () => {
  await withStatusPanel(async (page) => {
    assert.equal(
      await page.evaluate('document.querySelectorAll("[data-child-item] input").length'), 0,
      'a child item has no checkbox');

    // Check, then uncheck inside the window: the click is cancelled.
    await page.evaluate('document.querySelector("[data-done-check]").click()');
    await sleep(1000);
    await page.evaluate('document.querySelector("[data-done-check]").click()');
    await sleep(SEND_WAIT_MS - 1000);
    assert.equal(await page.evaluate('window.__posts.length'), 0, 'an unchecked click sends nothing');

    // Check again and let the window close: one message goes to the owner.
    await page.evaluate('document.querySelector("[data-done-check]").click()');
    await sleep(SEND_WAIT_MS);

    const posts = await page.evaluate('window.__posts');
    assert.equal(posts.length, 1, 'one checked click sends one message');
    assert.equal(posts[0].url, '/api/chat/' + PARENT + '/message', 'the message goes to the item owner');
    assert.equal(posts[0].body.content, [
      '[Status · Waiting on you]',
      '',
      `1. ▸ Waiting on you · session ${PARENT} · list ${OWN_VERSION} · since ${SINCE} · #1`,
      '   ↳ Done',
    ].join('\n'));
    assert.equal(posts[0].body.content.includes('Own decision'), false, 'the reference quotes no item text');
    assert.equal(posts[0].body.content.includes('Own context'), false, 'the reference quotes no item context');
  });
});

test('at desktop width the Waiting on you label sits in the Now label column, level with the first item', async () => {
  await withStatusPanel(async (page) => {
    await page.send('Emulation.setDeviceMetricsOverride',
      {width: 1280, height: 900, deviceScaleFactor: 1, mobile: false});
    // The heading is the row's first child and its label the heading's first child. The own item's open Context keeps
    // the row taller than one line, so a stretched heading would show here.
    const layout = await page.evaluate(`(() => {
      const row = document.querySelector('[data-status-row="waiting"]');
      const heading = row.firstElementChild;
      const label = heading.firstElementChild;
      const first = row.querySelector('[data-waiting-item]');
      const nowRow = document.querySelector('[data-status-row="now"]');
      const nowLabel = nowRow.firstElementChild;
      const nowValue = nowRow.lastElementChild;
      const nowUpdated = nowRow.querySelector('[data-status-updated]');
      const box = (el) => el.getBoundingClientRect();
      return {
        labelDrawn: getComputedStyle(label).visibility === 'visible' && label.textContent.trim().length > 0,
        labelLeft: box(label).left,
        labelWidth: box(label).width,
        labelHeight: box(label).height,
        labelTop: box(label).top,
        nowLabelLeft: box(nowLabel).left,
        nowLabelWidth: box(nowLabel).width,
        nowValueLeft: box(nowValue).left,
        nowUpdatedLeft: box(nowUpdated).left,
        precedesFirst: Boolean(label.compareDocumentPosition(first) & Node.DOCUMENT_POSITION_FOLLOWING),
        firstTop: box(first).top,
        headingHeight: box(heading).height,
        rowHeight: box(row).height,
      };
    })()`);
    assert.ok(layout.labelDrawn, 'the label is visible and carries text');
    assert.ok(layout.labelWidth > 0 && layout.labelHeight > 0, 'the label takes a box');
    assert.equal(layout.labelLeft, layout.nowLabelLeft, "the label starts in the Now row's label column");
    assert.equal(layout.labelWidth, layout.nowLabelWidth, "the label is as wide as the Now row's label column");
    assert.equal(layout.nowUpdatedLeft, layout.nowValueLeft,
      'an empty Now keeps its update time flush with the value column');
    assert.ok(layout.precedesFirst, 'the label precedes the first item in the document');
    assert.ok(Math.abs(layout.labelTop - layout.firstTop) <= 4,
      `the label's top ${layout.labelTop}px is level with the first item's top ${layout.firstTop}px`);
    assert.ok(layout.headingHeight < layout.rowHeight,
      `the heading is ${layout.headingHeight}px tall, not stretched to the row's ${layout.rowHeight}px`);
  });
});

test('at mobile width the Waiting on you label is drawn above the first item', async () => {
  await withStatusPanel(async (page) => {
    await page.send('Emulation.setDeviceMetricsOverride',
      {width: 390, height: 900, deviceScaleFactor: 1, mobile: false});
    const layout = await page.evaluate(`(() => {
      const row = document.querySelector('[data-status-row="waiting"]');
      const label = row.firstElementChild.firstElementChild;
      const first = row.querySelector('[data-waiting-item]');
      const nowUpdated = document.querySelector('[data-status-row="now"] [data-status-updated]');
      const box = (el) => el.getBoundingClientRect();
      return {
        labelDrawn: getComputedStyle(label).visibility === 'visible' && label.textContent.trim().length > 0
          && box(label).height > 0,
        labelLeft: box(label).left,
        labelBottom: box(label).bottom,
        rowLeft: box(row).left,
        nowUpdatedLeft: box(nowUpdated).left,
        firstTop: box(first).top,
        precedesFirst: Boolean(label.compareDocumentPosition(first) & Node.DOCUMENT_POSITION_FOLLOWING),
      };
    })()`);
    assert.ok(layout.labelDrawn, 'the label is visible and carries text');
    assert.ok(layout.precedesFirst, 'the label precedes the first item in the document');
    assert.ok(layout.labelBottom <= layout.firstTop,
      `the label's bottom ${layout.labelBottom}px sits above the first item's top ${layout.firstTop}px`);
    assert.equal(layout.labelLeft, layout.rowLeft, "the label starts at the row's left edge");
    assert.equal(layout.nowUpdatedLeft, layout.rowLeft,
      "an empty Now keeps its update time flush at the row's left edge");
  }, {mobile: true});
});
