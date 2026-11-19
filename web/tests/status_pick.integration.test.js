// ---------------------------------------------------------------------------
// An option pick on a Waiting on you item of a child session, clicked in the parent's Status view in a headless
// Chrome. The page loads the real chat/status-panel.js, comment_post.js, view-requests.js and icons.js with a stubbed
// Chat namespace and fetch, the way the chat page loads them, and answers GET /api/internal/web/status with a parent
// whose one open descendant child holds the item.
//
// Checks:
// - the child item's control row offers one button per option of its page's one open question, and no done mark;
// - a click sends, after the undo window, one click message to the child session in the landed format: the header,
//   the reference with the child's list version, and Q<n> with the option text verbatim;
// - the message carries neither the item's text nor its context.
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
const CHILD_VERSION = 'cccccccccccccccc';
const PAGE_PATH = '/chat';
// The undo window is five seconds; the wait leaves room for the post.
const SEND_WAIT_MS = 9000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// The status read's answer: the parent has no items of its own, and its one child holds an item whose page shows one
// open question with two options.
const STATUS = {
  session_id: PARENT,
  own_task_state: 'open',
  web_status: {
    goal: null,
    now: null,
    remaining: [],
    waiting: [],
    waiting_version: '1111111111111111',
    updated_at: '2026-11-19T08:05:00Z',
  },
  descendants: [{
    session_id: CHILD,
    name: 'Child',
    updated_at: '2026-11-19T08:05:00Z',
    waiting_version: CHILD_VERSION,
    waiting: [{
      text: 'Child decision',
      context: 'Child context',
      link: null,
      since: SINCE,
      answered: false,
      forks: [{
        n: '1',
        options: [
          {name: 'keep', text: '「keep」: keep the file.'},
          {name: 'drop', text: '「drop」: drop the file.'},
        ],
        rec: 'keep',
      }],
    }],
  }],
  children: [],
  derived_waiting: [],
};

function pageHtml() {
  return `<!doctype html>
<html><head><meta charset="utf-8"><title>Status picks</title></head><body>
<div id="status-panel"></div>
<script>${iconTableScript()}</script>
<script src="/static/js/icons.js"></script>
<script src="/static/js/comment_post.js"></script>
<script src="/static/js/view-requests.js"></script>
<script>
  window.__posts = [];
  const status = ${JSON.stringify(STATUS)};
  window.platform = {onChange() {}};
  function relativeTime() { return 'now'; }
  window.Chat = {
    wire(fns) { window.__status = fns; },
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
<script src="/static/js/chat/status-panel.js"></script>
</body></html>`;
}

function serve() {
  const page = pageHtml();
  const statics = {
    '/static/js/icons.js': path.join(WEB_DIR, 'static', 'js', 'icons.js'),
    '/static/js/comment_post.js': path.join(WEB_DIR, 'static', 'js', 'comment_post.js'),
    '/static/js/view-requests.js': path.join(WEB_DIR, 'static', 'js', 'view-requests.js'),
    '/static/js/chat/status-panel.js': path.join(WEB_DIR, 'static', 'js', 'chat', 'status-panel.js'),
  };
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1');
    if (req.method === 'GET' && url.pathname === PAGE_PATH) {
      res.writeHead(200, {'Content-Type': 'text/html; charset=utf-8'});
      res.end(page);
      return;
    }
    if (req.method === 'GET' && statics[url.pathname]) {
      res.writeHead(200, {'Content-Type': 'text/javascript; charset=utf-8'});
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

// Waits until the status read has answered and the child item's two picks are on the page.
async function waitForPicks(page) {
  const deadline = Date.now() + 15000;
  const ready = 'document.querySelectorAll("[data-child-item] [data-pick-choice]").length === 2';
  while (!(await page.evaluate(ready))) {
    if (Date.now() > deadline) throw new Error('timed out waiting for the child item picks');
    await sleep(50);
  }
}

// Runs one check against the served Status view of the parent, then tears everything down.
async function withStatusPanel(check) {
  const {server, base} = await serve();
  const profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'status-pick-chrome-'));
  let chrome = null;
  try {
    chrome = await launchChrome(profileDir);
    const page = await openPage(chrome.cdp, `${base}${PAGE_PATH}`);
    await page.evaluate('window.__status.openSessionStatusPanel({id: ' + JSON.stringify(PARENT) + '})');
    await waitForPicks(page);
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

test('a child item option click in the parent Status view posts to the child in the landed format', async () => {
  await withStatusPanel(async (page) => {
    const labels = await page.evaluate(
      'Array.from(document.querySelectorAll("[data-child-item] [data-pick-choice]"))' +
        '.map((button) => button.textContent)');
    assert.deepEqual(labels, ['keep', 'drop']);
    assert.equal(
      await page.evaluate('Array.from(document.querySelectorAll("[data-child-item] button"))' +
        '.some((button) => button.title === "Mark done")'),
      false, 'a child item has no done control');

    await page.evaluate(`(() => {
      const button = Array.from(document.querySelectorAll('[data-child-item] [data-pick-choice]'))
        .find((candidate) => candidate.textContent === 'drop');
      if (!button) throw new Error('no drop pick button');
      button.click();
      return true;
    })()`);
    await sleep(SEND_WAIT_MS);

    const posts = await page.evaluate('window.__posts');
    assert.equal(posts.length, 1, 'one pick sends one message');
    assert.equal(posts[0].url, '/api/chat/' + CHILD + '/message', 'the message goes to the item owner');
    assert.equal(posts[0].body.content, [
      '[Status · Waiting on you]',
      '',
      `1. ▸ Waiting on you · session ${CHILD} · list ${CHILD_VERSION} · since ${SINCE} · #1`,
      '   ↳ Q1 「drop」: drop the file.',
    ].join('\n'));
    assert.equal(posts[0].body.content.includes('Child decision'), false, 'the reference quotes no item text');
    assert.equal(posts[0].body.content.includes('Child context'), false, 'the reference quotes no item context');
  });
});
