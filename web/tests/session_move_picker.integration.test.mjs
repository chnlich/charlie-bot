// ---------------------------------------------------------------------------
// The sidebar's "Move under…" picker (sidebar/session-move.js) in a headless
// Chrome. The page loads the app's stylesheets and the picker's scripts
// from web/static. A stub server answers the two session lists and refuses the
// move with two blockers. A page-side probe measures the choice rows, the
// list that holds them and each block of the dialog, at a desktop width and a
// phone width. The checks: a row keeps its natural height and its label is not
// clipped, the list scrolls inside the dialog, and no block overlaps another
// or leaves the dialog. Screenshots of the open picker and of the refused move
// go to web/target/session-move-picker/.
//
// CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome binary. The test fails
// when it is unset or names no file.
// ---------------------------------------------------------------------------

import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import {fileURLToPath} from 'node:url';

import {CdpConnection, Page} from '../../dev/poor-network/chrome.mjs';

const WEB_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const STATIC_DIR = path.join(WEB_DIR, 'static');
const SHOTS_DIR = path.join(WEB_DIR, 'target', 'session-move-picker');

// The stylesheets and scripts the picker's page loads, in page order. The page's config.js
// runs auth code on load, so the page defines the one global the picker reads from it.
const STYLESHEETS = ['css/styles.css', 'css/tailwind.css'];
// The picker reads its rows through the sidebar's own column expander (utils.js, which groups.js re-exports), so the
// page loads both, in the page's order.
const PICKER_SCRIPTS = [
  'utils.js',
  'sidebar/namespace.js',
  'sidebar/groups.js',
  'sidebar/session-actions.js',
  'sidebar/session-move.js',
];

// The dialog's blocks in order: the header, the search box, the choice list, the error list and the footer.
const LIST_BLOCK = 2;
const ERROR_BLOCK = 3;
const DIALOG_BLOCKS = 5;

const VIEWPORTS = [
  {name: 'desktop', width: 1280, height: 800, mobile: false},
  {name: 'mobile', width: 390, height: 844, mobile: true},
];

const MOVED_ID = 'moved';
const MOVED_NAME = 'Move a session under another session';
const CURRENT_PARENT_ID = 'parent';
// Fourteen manager rows the moved session can go under. A long label and a CJK label test the one-line rule.
const MANAGER_NAMES = [
  'Quarterly planning notes',
  'Release checklist for the build host',
  'Review queue for open pull requests',
  'Data pipeline timing and retries',
  'Sidebar: keep the open session row inside the list',
  'Seeded reproducibility checklist',
  'Dashboard alert routing and escalation',
  'Weekly review of scheduled tasks',
  '一个很长的中文会话名称用来检查行高和省略号是否正常显示在一行之内',
  'A very long session name that keeps going well past the width of the picker so it must end in an ellipsis',
  'Gateway model choice and spend notes',
  'Access and measurement semantics',
  'Integration and delivery branches',
  'Install guide rewrite',
];
const BLOCKERS = [
  'Session “Sidebar: keep the open session row inside the list” is running a turn; stop it before the move.',
  'Session “A very long session name that keeps going well past the width of the picker” has a delayed trigger ' +
    'that would fire under its old parent.',
];

// The probe runs in the page. It returns plain numbers, and the test asserts them in Node.
const PROBE_JS = `
(() => {
  const lineHeight = (el) => parseFloat(getComputedStyle(el).lineHeight);
  const verticalPadding = (el) => {
    const style = getComputedStyle(el);
    return parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);
  };
  const box = (el) => {
    const r = el.getBoundingClientRect();
    return {top: r.top, bottom: r.bottom, left: r.left, right: r.right, height: r.height};
  };
  const sized = (el) => ({
    label: el.textContent,
    rect: box(el),
    line: lineHeight(el),
    natural: lineHeight(el) + verticalPadding(el),
    flexShrink: getComputedStyle(el).flexShrink,
    minHeight: getComputedStyle(el).minHeight,
    overflowY: getComputedStyle(el).overflowY,
    scrollHeight: el.scrollHeight,
    clientHeight: el.clientHeight,
    scrollWidth: el.scrollWidth,
    clientWidth: el.clientWidth,
  });
  window.pickerProbe = {
    measure() {
      const dialog = document.getElementById('session-move-overlay').firstElementChild;
      const blocks = [...dialog.children];
      const line = (selector) => {
        const el = dialog.querySelector(selector);
        return el ? sized(el) : null;
      };
      return {
        viewport: {
          width: innerWidth,
          height: innerHeight,
          pageScrollWidth: document.documentElement.scrollWidth,
        },
        dialog: box(dialog),
        blocks: blocks.map((el, index) => ({index, ...sized(el)})),
        header: {
          heading: line('.session-move-heading'),
          subject: line('.session-move-subject'),
        },
        rows: [...blocks[${LIST_BLOCK}].children].map(sized),
        errors: [...blocks[${ERROR_BLOCK}].children].map(sized),
      };
    },
    choose(index) {
      const dialog = document.getElementById('session-move-overlay').firstElementChild;
      dialog.children[${LIST_BLOCK}].children[index].click();
    },
  };
})();
`;

function sessionRows() {
  return [
    {id: MOVED_ID, name: MOVED_NAME, profile: 'manager', task_parent_id: CURRENT_PARENT_ID},
    {id: CURRENT_PARENT_ID, name: 'Current parent manager', profile: 'manager', task_parent_id: null},
    ...MANAGER_NAMES.map((name, index) => ({id: `mgr-${index}`, name, profile: 'manager', task_parent_id: null})),
    {id: 'worker', name: 'A worker leaf', profile: 'worker', task_parent_id: CURRENT_PARENT_ID},
  ];
}

// The rows as the route's `compact=move` column form: one cell array per field, in row order. These ids are not
// canonical UUIDs, so each id is a `~` cell of its own text, which is the form the route writes for such a home.
function columnForm(rows) {
  return {
    $encoded: {id: 'uuid-b64'},
    id: rows.map((row) => `~${row.id}`),
    name: rows.map((row) => row.name),
    profile: rows.map((row) => row.profile),
    task_parent_id: rows.map((row) => row.task_parent_id ?? null),
  };
}

function pageHtml() {
  const links = STYLESHEETS.map((href) => `<link rel="stylesheet" href="/static/${href}">`).join('\n');
  const scripts = PICKER_SCRIPTS.map((src) => `<script src="/static/js/${src}"></script>`).join('\n');
  return `<!doctype html>
<html lang="en" class="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
${links}
</head>
<body class="dark bg-slate-900 text-slate-100">
<script>const JSON_HEADERS = {'Content-Type': 'application/json'};</script>
${scripts}
<script>${PROBE_JS}</script>
</body>
</html>`;
}

function reply(res, status, contentType, body) {
  res.writeHead(status, {'Content-Type': contentType});
  res.end(body);
}

function serveStatic(res, pathname) {
  const file = path.join(STATIC_DIR, pathname.slice('/static/'.length));
  if (!file.startsWith(STATIC_DIR + path.sep) || !fs.existsSync(file)) {
    reply(res, 404, 'text/plain; charset=utf-8', 'not found');
    return;
  }
  const type = file.endsWith('.css') ? 'text/css; charset=utf-8' : 'application/javascript; charset=utf-8';
  reply(res, 200, type, fs.readFileSync(file));
}

// The stub server: the picker's page, the static files, the two session lists, and a PATCH that is refused with
// blockers. Each PATCH body is recorded in state.patches.
function startStubServer() {
  const state = {patches: [], listingUrls: []};
  const server = http.createServer((req, res) => {
    const {pathname} = new URL(req.url, 'http://127.0.0.1');
    const sessionPath = /^\/api\/sessions\/([^/]+)$/.exec(pathname);
    if (req.method === 'GET' && pathname === '/picker') {
      reply(res, 200, 'text/html; charset=utf-8', pageHtml());
    } else if (req.method === 'GET' && pathname.startsWith('/static/')) {
      serveStatic(res, pathname);
    } else if (req.method === 'GET' && pathname === '/api/sessions/') {
      state.listingUrls.push(req.url);
      reply(res, 200, 'application/json', JSON.stringify(columnForm(sessionRows())));
    } else if (req.method === 'GET' && pathname === '/api/sessions/chat-threads') {
      reply(res, 200, 'application/json', '[]');
    } else if (req.method === 'PATCH' && sessionPath) {
      let body = '';
      req.on('data', (chunk) => { body += chunk; });
      req.on('end', () => {
        state.patches.push({id: sessionPath[1], body: JSON.parse(body)});
        reply(res, 409, 'application/json', JSON.stringify({detail: {blockers: BLOCKERS}}));
      });
    } else {
      reply(res, 404, 'text/plain; charset=utf-8', 'not found');
    }
  });
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => resolve({server, state, base: `http://127.0.0.1:${server.address().port}`}));
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
  const deadline = Date.now() + 10000;
  while (!(await page.evaluate(expression))) {
    if (Date.now() > deadline) throw new Error(`timed out waiting for ${what}`);
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
}

async function saveScreenshot(page, name) {
  const {data} = await page.send('Page.captureScreenshot', {format: 'png'});
  fs.mkdirSync(SHOTS_DIR, {recursive: true});
  fs.writeFileSync(path.join(SHOTS_DIR, name), Buffer.from(data, 'base64'));
}

// The dialog's layout: every block inside the dialog, no block over the one above it, and no block clipping its text
// vertically except the list, which scrolls. The page itself must not scroll sideways.
function assertDialogLayout(m, label) {
  const {dialog, viewport} = m;
  assert.equal(m.blocks.length, DIALOG_BLOCKS, `${label}: the dialog has ${m.blocks.length} blocks`);
  assert.ok(
    dialog.left >= 0 && dialog.right <= viewport.width && dialog.top >= 0 && dialog.bottom <= viewport.height,
    `${label}: the dialog leaves the viewport: ${JSON.stringify({dialog, viewport})}`,
  );
  assert.ok(
    viewport.pageScrollWidth <= viewport.width,
    `${label}: the page scrolls sideways (${viewport.pageScrollWidth} > ${viewport.width})`,
  );
  let above = -Infinity;
  for (const block of m.blocks) {
    assert.ok(
      block.rect.left >= dialog.left - 0.5 && block.rect.right <= dialog.right + 0.5,
      `${label}: block ${block.index} leaves the dialog sideways`,
    );
    assert.ok(block.rect.top >= above - 0.5, `${label}: block ${block.index} overlaps the block above it`);
    above = block.rect.bottom;
    if (block.index !== LIST_BLOCK) {
      assert.ok(
        block.scrollHeight <= block.clientHeight,
        `${label}: block ${block.index} clips its text (${block.scrollHeight} > ${block.clientHeight})`,
      );
    }
  }
}

// Each choice row is one line: its height is its line height plus its vertical padding, and its label is not clipped.
// The message names the computed flex-shrink and min-height, which decide how a flex item gives up height.
function assertRows(rows, label) {
  for (const row of rows) {
    const style = `flex-shrink ${row.flexShrink}, min-height ${row.minHeight}`;
    assert.ok(
      row.rect.height >= row.natural,
      `${label}: “${row.label}” is ${row.rect.height}px, under its natural ${row.natural}px (${style})`,
    );
    assert.ok(
      row.rect.height <= row.natural + 1,
      `${label}: “${row.label}” is ${row.rect.height}px, over its natural ${row.natural}px (${style})`,
    );
    assert.ok(
      row.scrollHeight <= row.clientHeight,
      `${label}: “${row.label}” clips its label (${row.scrollHeight} > ${row.clientHeight})`,
    );
  }
}

// An error line wraps when its text is long, so it is at least one line high; its text is not clipped.
function assertErrors(lines, label) {
  for (const line of lines) {
    assert.ok(
      line.rect.height >= line.line,
      `${label}: error line is ${line.rect.height}px, under one line (${line.line}px): “${line.label}”`,
    );
    assert.ok(
      line.scrollHeight <= line.clientHeight,
      `${label}: error line clips its text (${line.scrollHeight} > ${line.clientHeight}): “${line.label}”`,
    );
  }
}

// The header names the operation and, under it, the session that moves. A session name sets no bound, so the name's
// line is the one that elides: the operation's words print whole whatever the name's length.
function assertHeader(m, label) {
  const {heading, subject} = m.header;
  assert.ok(heading, `${label}: the header names the operation`);
  assert.ok(
    heading.scrollWidth <= heading.clientWidth + 1,
    `${label}: the operation's words are clipped (${heading.scrollWidth} > ${heading.clientWidth})`,
  );
  assert.ok(subject, `${label}: the header names the session that moves`);
  assert.ok(
    subject.label.includes(MOVED_NAME),
    `${label}: the header names ${JSON.stringify(MOVED_NAME)}, not ${JSON.stringify(subject.label)}`,
  );
}

function assertPicker(m, label) {
  assertDialogLayout(m, label);
  assertHeader(m, label);
  assertRows(m.rows, label);
  const list = m.blocks[LIST_BLOCK];
  assert.equal(list.overflowY, 'auto', `${label}: the choice list does not scroll`);
  assert.ok(
    list.scrollHeight > list.clientHeight,
    `${label}: the choice list does not overflow (${list.scrollHeight} <= ${list.clientHeight})`,
  );
}

async function checkViewport(page, base, viewport, state) {
  const label = viewport.name;
  await page.send('Emulation.setDeviceMetricsOverride', {
    width: viewport.width,
    height: viewport.height,
    deviceScaleFactor: 1,
    mobile: viewport.mobile,
  });
  await page.send('Page.navigate', {url: `${base}/picker`});
  await until(page, "document.readyState === 'complete' && typeof openSessionMovePicker === 'function'", 'the page');

  await page.evaluate(`openSessionMovePicker(${JSON.stringify(MOVED_ID)})`);
  const open = await page.evaluate('pickerProbe.measure()');
  // The moved session's row and its parent are out of the list; the list keeps the Top level row and every manager.
  assert.equal(open.rows.length, MANAGER_NAMES.length + 1, `${label}, open: the choice rows`);
  assertPicker(open, `${label}, open`);
  assert.equal(
    state.listingUrls.at(-1),
    '/api/sessions/?compact=move',
    `${label}, open: the picker reads the four-column form`,
  );
  await saveScreenshot(page, `${label}-open.png`);

  await page.evaluate('pickerProbe.choose(1)');
  await until(page, `pickerProbe.measure().errors.length === ${BLOCKERS.length}`, 'the refused move to list blockers');
  const refused = await page.evaluate('pickerProbe.measure()');
  assertPicker(refused, `${label}, refused`);
  assert.deepEqual(refused.errors.map((line) => line.label), BLOCKERS, `${label}, refused: the error lines`);
  assertErrors(refused.errors, `${label}, refused`);
  assert.deepEqual(state.patches.at(-1), {id: MOVED_ID, body: {task_parent_id: 'mgr-0'}});
  await saveScreenshot(page, `${label}-refused.png`);
}

test('the Move under picker keeps each row at its natural height and scrolls its list', {timeout: 120000}, async () => {
  const {server, state, base} = await startStubServer();
  const profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'session-move-chrome-'));
  let chrome = null;
  try {
    chrome = await launchChrome(profileDir);
    const page = await openPage(chrome.cdp);
    for (const viewport of VIEWPORTS) {
      await checkViewport(page, base, viewport, state);
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
