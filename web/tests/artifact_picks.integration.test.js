// ---------------------------------------------------------------------------
// Option picks on a plan page in a headless Chrome. The test serves
// prompts/plan_template.html as a plan of session OWNER, with the script tags the
// server injects into an artifact page, and opens it with a viewing hash that
// names another session, the way a page opened from another session's chat is.
//
// Checks:
// - the page's own parser reads the shared fixture (rust/.../tests/data/plan_template_forks.json),
//   the browser side of the fixture that artifact_check.rs asserts on the Rust side;
// - each option is a button, and no text box stands beside the options;
// - Send posts one message to the page owner, not the viewing session, after the undo window;
// - Undo inside the window posts nothing.
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

const WEB_DIR = path.join(__dirname, '..');
const REPO_DIR = path.join(WEB_DIR, '..');
const TEMPLATE_PATH = path.join(REPO_DIR, 'prompts', 'plan_template.html');
const FIXTURE_PATH = path.join(REPO_DIR, 'rust', 'crates', 'features', 'artifacts', 'tests', 'data',
  'plan_template_forks.json');
const OWNER = 'owner-session';
const VIEWER = 'viewer-session';
const PAGE_PATH = `/sessions/${OWNER}/artifacts/plan_picks.html`;
// The undo window is five seconds; the wait leaves room for the post.
const SEND_WAIT_MS = 9000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function serve(sent) {
  const template = fs.readFileSync(TEMPLATE_PATH, 'utf8');
  const tags = `<script>window.__cbcServerSessionId=${JSON.stringify(OWNER)};</script>\n` +
    '<script src="/static/js/comment_post.js"></script>\n' +
    '<script src="/static/features/artifacts/artifact-comments.js"></script>';
  const page = template.replace('</body>', `${tags}\n</body>`);
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1');
    if (req.method === 'POST' && url.pathname.startsWith('/api/chat/') && url.pathname.endsWith('/message')) {
      let body = '';
      req.on('data', (chunk) => { body += chunk; });
      req.on('end', () => {
        const session = url.pathname.split('/')[3];
        sent.push({session, body: JSON.parse(body)});
        res.writeHead(202, {'Content-Type': 'application/json'});
        res.end(JSON.stringify({status: 'accepted'}));
      });
      return;
    }
    const statics = {
      '/static/js/comment_post.js': path.join(WEB_DIR, 'static', 'js', 'comment_post.js'),
      '/static/features/artifacts/artifact-comments.js':
        path.join(WEB_DIR, 'features', 'artifacts', 'static', 'artifact-comments.js'),
    };
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
  while (!(await page.evaluate('typeof window.__cbcPageForks === "function" && !!document.querySelector(".__cbc-pickbar")'))) {
    if (Date.now() > deadline) throw new Error('timed out waiting for the pick bar');
    await sleep(50);
  }
  return page;
}

// Runs one check against a served plan page opened from the viewing session, then tears everything down.
async function withPlanPage(check) {
  const sent = [];
  const {server, base} = await serve(sent);
  const profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'artifact-picks-chrome-'));
  let chrome = null;
  try {
    chrome = await launchChrome(profileDir);
    const page = await openPage(chrome.cdp, `${base}${PAGE_PATH}#cbsession=${VIEWER}`);
    await check(page, sent);
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

test('the browser parser reads the shared fixture and offers a button per option, no text box', async () => {
  await withPlanPage(async (page) => {
    const fixture = JSON.parse(fs.readFileSync(FIXTURE_PATH, 'utf8'));
    assert.deepEqual(await page.evaluate('window.__cbcPageForks()'), fixture);

    const buttons = await page.evaluate(
      'Array.from(document.querySelectorAll(".__cbc-picks button")).map((button) => button.textContent)');
    assert.deepEqual(buttons, fixture[0].options.map((option) => option.name));
    assert.equal(await page.evaluate('document.querySelectorAll(".__cbc-picks textarea, .__cbc-picks input").length'), 0);
  });
});

test('Send posts one message to the page owner after the undo window, option text verbatim', async () => {
  await withPlanPage(async (page, sent) => {
    const fixture = JSON.parse(fs.readFileSync(FIXTURE_PATH, 'utf8'));
    await page.evaluate('document.querySelector(".__cbc-pick").click()');
    await page.evaluate('document.querySelector(".__cbc-pickbar-send").click()');
    await sleep(SEND_WAIT_MS);

    assert.equal(sent.length, 1, 'one message for the page');
    assert.equal(sent[0].session, OWNER, 'the message goes to the page owner, not the viewing session');
    const option = fixture[0].options[0];
    assert.equal(sent[0].body.content, [
      `[Artifact picks · ${PAGE_PATH}] (1)`,
      `1. ▸ Q${fixture[0].n}`,
      `   ↳ ${option.text}`,
    ].join('\n'));
  });
});

test('Undo inside the undo window posts nothing', async () => {
  await withPlanPage(async (page, sent) => {
    await page.evaluate('document.querySelector(".__cbc-pick").click()');
    await page.evaluate('document.querySelector(".__cbc-pickbar-send").click()');
    await page.evaluate(
      'Array.from(document.querySelectorAll(".__cbc-pickbar-btn")).find((button) => button.textContent === "Undo").click()');
    await sleep(SEND_WAIT_MS);

    assert.deepEqual(sent, []);
    assert.equal(await page.evaluate('document.querySelector(".__cbc-pickbar-label").textContent'), 'Picked 1/1');
  });
});
