// ---------------------------------------------------------------------------
// The HTML artifact card's resize grips (features/artifacts/static/artifact-cards.js) in a headless Chrome. The page
// loads the chat modules in the order the chat page includes them, then builds one card through the page's own
// expandArtifactCard. A drag on a grip moves the frame by the dragged distance: the right grip changes the width,
// the bottom grip the height, the corner grip both. The right grip is dragged in first to narrow the card, because
// the wrapper's max-width caps it at its column and a card at full column width cannot widen.
//
// The frame's height follows its own document through a ResizeObserver, so a width change can nudge the height by a
// few pixels before it settles. The checks read the frame after it settles, and assert the height only on the axis
// the grip drags.
//
// CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome binary. The test fails when it is unset or names no file.
// ---------------------------------------------------------------------------

import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import {createRequire} from 'node:module';
import {fileURLToPath} from 'node:url';

import {CdpConnection, Page} from '../../dev/poor-network/chrome.mjs';

const require = createRequire(import.meta.url);
const {indexScriptPaths, iconTableScript} = require('./read_static');

const WEB_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');

// The chat page's scripts that the card's module needs are the ones before it in the page. The sockets, the sidebar
// and the input wire the live app, which this page does not run.
const SKIPPED_SCRIPTS = new Set(['app.js', 'websocket.js', 'websocket-handlers.js', 'input.js', 'usage.js',
  'file-upload.js', 'tabs.js', 'icons.js']);

// The grips to drag, each step a drag of (dx, dy) in CSS pixels, in order. The right and corner grips narrow the card
// first, then widen it back, so the second drag has room within the column.
const STEPS = [
  {axis: 'e', drags: [[-200, 0], [60, 0]]},
  {axis: 's', drags: [[0, 40]]},
  {axis: 'se', drags: [[-200, 0], [60, 40]]},
];

const PAGE_GLOBALS = `let SESSION_ID = null; let DRAFT_KEY = null; let THINKING_SINCE = null;
const PAGE_HOSTNAME = 'test';
const SESSION_NAME = null; const SESSION_PROFILE = ''; const SESSION_BOOTSTRAP = null;
const INITIAL_SESSIONS = []; const INITIAL_LOAD_ERRORS = []; const STATIC_ASSET_VERSION = 'test';`;

// The page's probe: one card per mount, built by the page's expandArtifactCard. The highlighter the card's source
// view uses is the one piece of the page this test stubs, because it loads from a CDN.
const PROBE_JS = `
window.hljs = {highlight: (code) => ({value: code.replace(/&/g, '&amp;').replace(/</g, '&lt;')})};
window.gripProbe = {
  async mount(axis) {
    const old = document.getElementById('grip-section');
    if (old) old.remove();
    const section = document.createElement('section');
    section.id = 'grip-section';
    section.style.width = '560px';
    section.style.padding = '12px';
    const card = document.createElement('div');
    card.className = 'html-artifact';
    // A unique file name per mount: the frame restores the size saved under a file name, so a reused name would
    // carry the last drag into this card.
    const absPath = '/fixture/reports/grip-' + Math.random().toString(36).slice(2, 10) + '.html';
    card.dataset.artifactPath = absPath;
    card.dataset.artifactFetchUrl = '/fixture/not-served';
    Chat.htmlArtifactFetchCache.set(absPath, Promise.resolve('<p>Grip</p>'));
    section.appendChild(card);
    document.getElementById('fixture-root').prepend(section);
    window.scrollTo(0, 0);
    await Chat.expandArtifactCard(card);
    return section.querySelector('.html-artifact-resize-' + axis) ? 'mounted' : 'no handle for ' + axis;
  },
  point(axis) {
    const handle = document.querySelector('#grip-section .html-artifact-resize-' + axis);
    const r = handle.getBoundingClientRect();
    const x = r.left + r.width / 2;
    const y = r.top + r.height / 2;
    const hit = document.elementFromPoint(x, y);
    return {x, y, hitClass: hit ? String(hit.className.baseVal ?? hit.className) : ''};
  },
  size() {
    const section = document.getElementById('grip-section');
    const wrap = section.querySelector('.html-artifact-frame-wrap').getBoundingClientRect();
    const frame = section.querySelector('.html-artifact-frame').getBoundingClientRect();
    return {wrapWidth: wrap.width, frameHeight: frame.height};
  },
};
`;

// The chat page's own markup the card's module reads, and the page's stylesheets, in the order the page loads them.
function pageHtml() {
  const scripts = indexScriptPaths().filter((p) => !SKIPPED_SCRIPTS.has(p)
    && !(p.startsWith('sidebar/') && p !== 'sidebar/namespace.js'));
  return `<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<link rel="stylesheet" href="/static/css/styles.css">
<link rel="stylesheet" href="/features/artifacts/static/artifacts.css">
<link rel="stylesheet" href="/static/css/tailwind.css">
</head>
<body class="dark bg-slate-900 text-slate-100">
<div id="fixture-root" style="max-width:760px;margin:0 auto"></div>
<script>${PAGE_GLOBALS}</script>
<script>${iconTableScript()}</script>
<script src="/static/js/icons.js"></script>
${scripts.map((p) => `<script src="/static/js/${p}"></script>`).join('\n')}
<script src="/features/artifacts/static/artifact-cards.js"></script>
<script>${PROBE_JS}</script>
</body></html>`;
}

function reply(res, status, contentType, body) {
  res.writeHead(status, {'content-type': contentType});
  res.end(body);
}

// The page at /, and the static files the page names: /static/ is web/static and /features/ is web/features.
function startServer() {
  const server = http.createServer((req, res) => {
    const {pathname} = new URL(req.url, 'http://127.0.0.1');
    if (req.method !== 'GET') return reply(res, 405, 'text/plain; charset=utf-8', 'method not allowed');
    if (pathname === '/') return reply(res, 200, 'text/html; charset=utf-8', pageHtml());
    const file = path.normalize(path.join(WEB_DIR, pathname));
    const roots = [path.join(WEB_DIR, 'static'), path.join(WEB_DIR, 'features')];
    if (!roots.some((root) => file.startsWith(root + path.sep)) || !fs.existsSync(file)) {
      return reply(res, 404, 'text/plain; charset=utf-8', 'not found');
    }
    const type = file.endsWith('.css') ? 'text/css; charset=utf-8' : 'application/javascript; charset=utf-8';
    return reply(res, 200, type, fs.readFileSync(file));
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({server, base: `http://127.0.0.1:${server.address().port}`}));
  });
}

async function launchChrome(profileDir) {
  const binary = process.env.CHARLIEBOT_TEST_CHROME_BIN;
  if (!binary) throw new Error('CHARLIEBOT_TEST_CHROME_BIN must name a headless Chrome binary');
  if (!fs.existsSync(binary)) throw new Error(`CHARLIEBOT_TEST_CHROME_BIN names a missing file: ${binary}`);
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

async function openPage(cdp) {
  const {targetId} = await cdp.send('Target.createTarget', {url: 'about:blank'});
  const {sessionId} = await cdp.send('Target.attachToTarget', {targetId, flatten: true});
  const page = new Page(cdp, sessionId, targetId);
  await page.send('Page.enable');
  await page.send('Runtime.enable');
  return page;
}

async function until(page, expression, what) {
  const deadline = Date.now() + 20000;
  while (!(await page.evaluate(expression))) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

// A mouse event of the left button. The button is down from the press until the release.
async function mouse(page, type, x, y, pressed) {
  await page.send('Input.dispatchMouseEvent', {type, x, y, button: 'left', buttons: pressed ? 1 : 0,
    clickCount: type === 'mousePressed' ? 1 : 0});
}

// Reads the frame until three samples in a row agree: the frame sizes itself after it loads and after each width
// change.
async function settledSize(page) {
  await new Promise((resolve) => setTimeout(resolve, 1200));
  let last = await page.evaluate('gripProbe.size()');
  let stable = 0;
  for (let i = 0; i < 60 && stable < 3; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 200));
    const next = await page.evaluate('gripProbe.size()');
    stable = next.frameHeight === last.frameHeight && next.wrapWidth === last.wrapWidth ? stable + 1 : 0;
    last = next;
  }
  assert.equal(stable, 3, 'the frame did not settle after a resize');
  return last;
}

test('each resize grip drags the artifact frame by the dragged distance', {timeout: 180000}, async () => {
  const profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'grip-drag-'));
  const {server, base} = await startServer();
  let chrome = null;
  try {
    chrome = await launchChrome(profileDir);
    const page = await openPage(chrome.cdp);
    await page.send('Page.navigate', {url: `${base}/`});
    await until(page, 'typeof window.gripProbe === "object"', 'the chat modules and the grip probe to load');
    for (const {axis, drags} of STEPS) {
      const mounted = await page.evaluate(`gripProbe.mount(${JSON.stringify(axis)})`);
      assert.equal(mounted, 'mounted', `the card has no ${axis} grip`);
      for (const [dx, dy] of drags) {
        const before = await settledSize(page);
        const point = await page.evaluate(`gripProbe.point(${JSON.stringify(axis)})`);
        assert.ok(point.hitClass.includes(`html-artifact-resize-${axis}`),
          `a press on the ${axis} grip lands on ${JSON.stringify(point.hitClass)}`);
        await mouse(page, 'mouseMoved', point.x, point.y, false);
        await mouse(page, 'mousePressed', point.x, point.y, true);
        for (let i = 1; i <= 10; i += 1) {
          await mouse(page, 'mouseMoved', point.x + (dx * i) / 10, point.y + (dy * i) / 10, true);
        }
        await mouse(page, 'mouseReleased', point.x + dx, point.y + dy, false);
        const after = await settledSize(page);
        const label = `${axis} drag by (${dx}, ${dy})`;
        if (dx !== 0) {
          assert.equal(after.wrapWidth - before.wrapWidth, dx, `${label} changes the width by the dragged distance`);
        }
        if (dy !== 0) {
          assert.equal(after.frameHeight - before.frameHeight, dy,
            `${label} changes the height by the dragged distance`);
        }
      }
    }
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
});
