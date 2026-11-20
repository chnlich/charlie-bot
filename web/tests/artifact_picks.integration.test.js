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
// - with one pick made and one pending comment open, no floating control of the page layer (the pick bar, the
//   pending-comment tray, the shortcut buttons, the anchored comment cards, the comment trigger and the composer)
//   covers a content box at any scroll position, at each viewport width, and every control stays inside the viewport.
//
// CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome binary. The test fails when it is unset or names no file.
// CHARLIEBOT_COVER_PAGE names the plan page the cover checks open, in place of the template.
// CHARLIEBOT_COVER_SHOTS names a file prefix: the cover checks save screenshots as <prefix>_<width>px_<state>_<label>.png.
// ---------------------------------------------------------------------------
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const {spawn} = require('node:child_process');
const test = require('node:test');

const {iconTable, iconTableScript} = require('./read_static');

const WEB_DIR = path.join(__dirname, '..');
const REPO_DIR = path.join(WEB_DIR, '..');
const TEMPLATE_PATH = path.join(REPO_DIR, 'prompts', 'plan_template.html');
// The plan page the cover checks open: the template, or the page CHARLIEBOT_COVER_PAGE names (a real plan on the host).
const COVER_PAGE_PATH = process.env.CHARLIEBOT_COVER_PAGE || TEMPLATE_PATH;
const FIXTURE_PATH = path.join(REPO_DIR, 'rust', 'crates', 'features', 'artifacts', 'tests', 'data',
  'plan_template_forks.json');
const ICON_PARITY_PATH = path.join(WEB_DIR, 'tests', 'icons_parity.json');
const OWNER = 'owner-session';
const VIEWER = 'viewer-session';
const PAGE_PATH = `/sessions/${OWNER}/artifacts/plan_picks.html`;
// The undo window is five seconds; the wait leaves room for the post.
const SEND_WAIT_MS = 9000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function serve(sent, sourcePath) {
  const template = fs.readFileSync(sourcePath, 'utf8');
  const tags = `<script>window.__cbcServerSessionId=${JSON.stringify(OWNER)};</script>\n` +
    `<script>${iconTableScript()}</script>\n` +
    '<script src="/static/js/icons.js"></script>\n' +
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
      '/static/js/icons.js': path.join(WEB_DIR, 'static', 'js', 'icons.js'),
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

async function openPage(cdp, url, viewport) {
  const {Page} = await import('../../dev/poor-network/chrome.mjs');
  const {targetId} = await cdp.send('Target.createTarget', {url: 'about:blank'});
  const {sessionId} = await cdp.send('Target.attachToTarget', {targetId, flatten: true});
  const page = new Page(cdp, sessionId, targetId);
  await page.send('Page.enable');
  await page.send('Runtime.enable');
  if (viewport) {
    await page.send('Emulation.setDeviceMetricsOverride',
      {width: viewport.width, height: viewport.height, deviceScaleFactor: 1, mobile: false});
  }
  await page.send('Page.navigate', {url});
  const deadline = Date.now() + 15000;
  while (!(await page.evaluate('typeof window.__cbcPageForks === "function" && !!document.querySelector(".__cbc-pickbar")'))) {
    if (Date.now() > deadline) throw new Error('timed out waiting for the pick bar');
    await sleep(50);
  }
  return page;
}

// Runs one check against a served plan page opened from the viewing session, then tears everything down.
// The page is the template unless `source` names another file; a viewport, when given, is set before the page loads.
async function withPlanPage(check, {viewport, source = TEMPLATE_PATH} = {}) {
  const sent = [];
  const {server, base} = await serve(sent, source);
  const profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'artifact-picks-chrome-'));
  let chrome = null;
  try {
    chrome = await launchChrome(profileDir);
    const page = await openPage(chrome.cdp, `${base}${PAGE_PATH}#cbsession=${VIEWER}`, viewport);
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

test('the icon table the page embeds is the Rust table for the parity keys', () => {
  const parity = JSON.parse(fs.readFileSync(ICON_PARITY_PATH, 'utf8')).script;
  const expected = JSON.parse(parity.replace(/^window\.CB_ICON_TABLE = /, '').replace(/;\n$/, ''));
  const table = iconTable();
  for (const key of Object.keys(expected)) assert.deepEqual(table[key], expected[key], `icon ${key}`);
});

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
    assert.equal(await page.evaluate('document.querySelectorAll(".__cbc-pick svg").length'), 1,
      'the picked option draws the done icon');
    assert.equal(await page.evaluate('document.querySelector(".__cbc-pickbar-send svg") !== null'), true,
      'Send draws its icon');
    // A pick's Send starts the delayed send that Status clicks start too: the one global startUndoableSend.
    await page.evaluate(`(() => {
      window.__sendStarts = 0;
      const start = window.startUndoableSend;
      window.startUndoableSend = function(...args) {
        window.__sendStarts += 1;
        return start.apply(this, args);
      };
      return true;
    })()`);
    await page.evaluate('document.querySelector(".__cbc-pickbar-send").click()');
    await sleep(SEND_WAIT_MS);

    assert.equal(await page.evaluate('window.__sendStarts'), 1, 'the pick goes through the shared delayed send');
    assert.equal(sent.length, 1, 'one message for the page');
    assert.equal(sent[0].session, OWNER, 'the message goes to the page owner, not the viewing session');
    const option = fixture[0].options[0];
    assert.equal(sent[0].body.content, [
      `[Artifact picks · ${PAGE_PATH}] (1)`,
      '',
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
      'Array.from(document.querySelectorAll(".__cbc-pickbar-btn")).find((button) => button.textContent.trim() === "Undo").click()');
    await sleep(SEND_WAIT_MS);

    assert.deepEqual(sent, []);
    assert.equal(await page.evaluate('document.querySelector(".__cbc-pickbar-label").textContent'), 'Picked 1/1');
  });
});

// The widths and the scroll positions of the cover checks. The shots name the three positions a reader meets.
const COVER_WIDTHS = [1570, 1280, 390];
const COVER_VIEWPORT_HEIGHT = 800;
const COVER_SCROLLS = [0, 0.2, 0.4, 0.5, 0.6, 0.8, 1];
const COVER_SHOT_LABELS = {0: 'top', 0.5: 'mid', 1: 'end'};

// One pick and one pending comment, typed into the composer and sent to the tray, then the trigger shown by a hover.
const PICK_AND_COMMENT = `(() => {
  const paragraphs = Array.from(document.querySelectorAll('p')).filter((p) =>
    !p.closest('.fork, .__cbc-ui') && p.textContent.trim().length > 40 && p.getClientRects().length > 0);
  document.querySelector('.__cbc-pick').click();
  paragraphs[0].click();
  document.querySelector('.__cbc-popover textarea').value = 'Check the tray against the page text.';
  document.querySelector('.__cbc-add').click();
  paragraphs[2].dispatchEvent(new MouseEvent('mouseover', {bubbles: true}));
  return {pickLabel: document.querySelector('.__cbc-pickbar-label').textContent,
    trigger: document.querySelector('.__cbc-trigger').style.display};
})()`;

// The composer open on a block, as a click on page text opens it.
const OPEN_COMPOSER = `(() => {
  const paragraphs = Array.from(document.querySelectorAll('p')).filter((p) =>
    !p.closest('.fork, .__cbc-ui') && p.textContent.trim().length > 40 && p.getClientRects().length > 0);
  paragraphs[4].click();
  return document.querySelector('.__cbc-popover') !== null;
})()`;

// Runs in the page. It reads the page's floating controls and its content boxes from the layout the browser computed,
// clips each content box to the part of the page a reader sees, and reports each control box that covers a content
// box and each control box that lies outside the viewport.
function coverProbe() {
  const area = (r) => Math.max(0, r.right - r.left) * Math.max(0, r.bottom - r.top);
  const clip = (a, b) => ({
    left: Math.max(a.left, b.left),
    top: Math.max(a.top, b.top),
    right: Math.min(a.right, b.right),
    bottom: Math.min(a.bottom, b.bottom),
  });
  const body = document.body;
  const bodyBox = body.getBoundingClientRect();
  const viewport = {left: 0, top: 0, right: window.innerWidth, bottom: window.innerHeight};
  // The body's own box bottom, not clientHeight: clientHeight rounds to whole pixels, so a strip that starts exactly at
  // the body's bottom edge would count as a sub-pixel overlap.
  const reader = clip({
    left: bodyBox.left + body.clientLeft,
    top: bodyBox.top + body.clientTop,
    right: bodyBox.left + body.clientLeft + body.clientWidth,
    bottom: bodyBox.bottom,
  }, viewport);
  const contents = [];
  for (const el of body.querySelectorAll('*')) {
    if (el.closest('.__cbc-ui')) continue;
    if (!Array.from(el.childNodes).some((node) => node.nodeType === 3 && node.textContent.trim())) continue;
    for (const rect of el.getClientRects()) {
      const box = clip(rect, reader);
      if (area(box) > 0) contents.push({box, text: el.textContent.trim().slice(0, 40)});
    }
  }
  // A control's visible box: its own box, cut by each scroll container between it and the fixed box that holds it.
  // A control in the rail's scrolling stack shows only the part of it the rail shows.
  const visibleBox = (el) => {
    let box = el.getBoundingClientRect();
    if (getComputedStyle(el).position === 'fixed') return box;
    for (let node = el.parentElement; node; node = node.parentElement) {
      const style = getComputedStyle(node);
      if (style.overflowX !== 'visible' || style.overflowY !== 'visible') box = clip(box, node.getBoundingClientRect());
      if (style.position === 'fixed') break;
    }
    return box;
  };
  const controls = [];
  const selector = '.__cbc-dock, .__cbc-dock > *, .__cbc-gutter, .__cbc-gutter .__cbc-tray-item, .__cbc-trigger';
  for (const el of document.querySelectorAll(selector)) {
    const box = visibleBox(el);
    if (area(box) > 0) controls.push({el, box});
  }
  const overlaps = [];
  for (const control of controls) {
    for (const content of contents) {
      if (area(clip(control.box, content.box)) > 1) {
        overlaps.push(`${control.el.className} covers "${content.text}"`);
      }
    }
  }
  const offscreen = controls
    .filter((control) => control.box.left < -0.5 || control.box.top < -0.5 ||
      control.box.right > window.innerWidth + 0.5 || control.box.bottom > window.innerHeight + 0.5)
    .map((control) => control.el.className);
  return {overlaps, offscreen, contents: contents.length, controls: controls.length};
}

// Saves the viewport as <CHARLIEBOT_COVER_SHOTS>_<width>px_<state>_<top|mid|end>.png when that prefix is set.
async function saveCoverShot(page, width, state, label) {
  const prefix = process.env.CHARLIEBOT_COVER_SHOTS;
  if (!prefix) return;
  const {data} = await page.send('Page.captureScreenshot', {format: 'png'});
  fs.mkdirSync(path.dirname(prefix), {recursive: true});
  fs.writeFileSync(`${prefix}_${width}px_${state}_${label}.png`, Buffer.from(data, 'base64'));
}

// The cover findings at every scroll position from the top to the end of the page, one line per finding.
async function coverFindings(page, width, state) {
  const end = await page.evaluate(`(() => {
    const body = document.body;
    return Math.max(document.documentElement.scrollHeight - window.innerHeight, body.scrollHeight - body.clientHeight);
  })()`);
  const findings = [];
  for (const fraction of COVER_SCROLLS) {
    const y = Math.round(end * fraction);
    await page.evaluate(`(() => { window.scrollTo(0, ${y}); document.body.scrollTop = ${y}; return true; })()`);
    await sleep(60);
    const report = await page.evaluate(`(${coverProbe})()`);
    assert.ok(report.controls > 0, 'the page layer has floating controls to check');
    assert.ok(report.contents > 0, 'the page has content boxes to check');
    for (const overlap of report.overlaps) findings.push(`${state} at ${y}px: ${overlap}`);
    for (const name of report.offscreen) findings.push(`${state} at ${y}px: ${name} lies outside the viewport`);
    const label = COVER_SHOT_LABELS[fraction];
    if (label) await saveCoverShot(page, width, state, label);
  }
  return findings;
}

for (const width of COVER_WIDTHS) {
  test(`with a pick and a pending comment open, no floating control covers page content at ${width}px`, async () => {
    await withPlanPage(async (page) => {
      const state = await page.evaluate(PICK_AND_COMMENT);
      // The tray's cards are built on the next animation frame, so the count waits for it.
      await sleep(100);
      const pending = await page.evaluate('document.querySelectorAll(".__cbc-tray-item").length');
      assert.match(state.pickLabel, /^Picked 1\//, 'one pick is made');
      assert.equal(pending, 1, 'one pending comment is open');
      assert.equal(state.trigger, 'flex', 'the comment trigger shows over the hovered block');
      const findings = await coverFindings(page, width, 'tray');
      assert.equal(await page.evaluate(OPEN_COMPOSER), true, 'the composer opens');
      findings.push(...await coverFindings(page, width, 'composer'));
      assert.deepEqual(findings, []);
    }, {viewport: {width, height: COVER_VIEWPORT_HEIGHT}, source: COVER_PAGE_PATH});
  });
}
