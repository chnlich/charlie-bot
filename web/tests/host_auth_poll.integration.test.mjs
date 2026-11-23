// ---------------------------------------------------------------------------
// The host-auth page's poll in a headless Chrome. The page is the template's own markup and script, rendered for the
// fixture rows with the page's shared head and base styles; the template's script runs as written. A stub server
// answers the row routes, counts the row polls, and holds a check open for `checkMs`.
//
// The checks run across poll cycles in which no fact changes: the paste input of an active login keeps its value,
// focus and selection, and its row's buttons keep their nodes; a poll that changes no fact touches no node of the
// table, and a selection in a row's output stays selected; a check keeps its row's controls in place and reads
// "Checking." on its button while it runs. The active login and a running check are also captured at the three window
// sizes the landing names, into web/target/host-auth-poll/, and each capture checks that the row's controls fit and do
// not overlap.
//
// CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome binary. The test fails when it is unset or names no file.
// ---------------------------------------------------------------------------

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import {createRequire} from 'node:module';
import {fileURLToPath} from 'node:url';

const require = createRequire(import.meta.url);
const {launchChrome, openPage, stopChrome, until} = require('./session_page_fixture');
const {iconTable} = require('./read_static');

const WEB_DIR = path.join(path.dirname(fileURLToPath(import.meta.url)), '..');
const TEMPLATES_DIR = path.join(WEB_DIR, 'templates');
const TEMPLATE = fs.readFileSync(path.join(WEB_DIR, 'features', 'host_auth', 'templates', 'host_auth.html'), 'utf8');
const SHOTS_DIR = path.join(WEB_DIR, 'target', 'host-auth-poll');
// The window sizes the landing captures, as `charliebot land --help` lists them.
const WINDOW_SIZES = [
  {width: 1536, height: 900},
  {width: 1400, height: 1000},
  {width: 390, height: 844},
];
const INPUT_VALUE = 'https://github.com/callback?code=0f3c9a71b2d84e65&state=4f9e2c1b7a6d5e4f3a2b1c0d9e8f7a6b';

// ---------------------------------------------------------------------------
// The template rendered for a context. Only the tags the page uses are handled: includes of the shared head and base
// styles, comments and set tags, the row loop with its else, `if` blocks (nested ones first), `{{ }}` values whose
// expressions are dotted names, `not`, `and`, `or`, `==` and string literals, and the one icon call the page makes.
// ---------------------------------------------------------------------------
const INCLUDE = /{%-?\s*include\s+'([^']+)'\s*-?%}/g;
const COMMENT = /{#-?[\s\S]*?-?#}/g;
const SET_TAG = /{%-?\s*set\b[^%]*%}/g;
const ROW_LOOP = /{%-?\s*for row in rows\s*-?%}([\s\S]*?){%-?\s*else\s*-?%}([\s\S]*?){%-?\s*endfor\s*-?%}/;
const IF_BLOCK = /{%-?\s*if\s+([^%]*?)\s*-?%}((?:(?!{%-?\s*if\s)[\s\S])*?){%-?\s*endif\s*-?%}/g;
const VARIABLE = /{{-?\s*([\s\S]*?)\s*-?}}/g;
const ICON_CALL = /^icon\('([^']+)',\s*(\d+),\s*'([^']*)'\)$/;
const ICONS = iconTable();

// The page's head and base styles come from their partials. The concurrency badge strip is left out: this change does
// not touch it, and its own poll needs routes the fixture does not serve. Any other include fails the render.
function inlineInclude(name) {
  if (name === 'partials/concurrency_badge.html') return '';
  if (name === 'partials/prologue.html' || name === 'partials/standalone_base_styles.html') {
    return fs.readFileSync(path.join(TEMPLATES_DIR, name), 'utf8');
  }
  throw new Error(`host_auth.html includes ${name}, which the fixture does not render`);
}

// An icon as the page's icon call draws it: the icon table's shapes in a Lucide frame of the given size and class.
function icon(name, size, cls) {
  const shape = ICONS[name];
  if (!shape) throw new Error(`the icon table has no ${name}`);
  const fill = shape.fill ? 'currentColor' : 'none';
  const frame = `xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 24 24"`;
  const stroke = 'stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"';
  return `<svg ${frame} fill="${fill}" ${stroke} class="${cls}" aria-hidden="true">${shape.shapes}</svg>`;
}

function escapeHtml(value) {
  const entities = {'&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;'};
  return String(value ?? '').replace(/[&<>"]/g, (ch) => entities[ch]);
}

// The expression in JavaScript: `not`, `and` and `or` outside string literals become `!`, `&&` and `||`.
function evaluate(expression, scope) {
  const toJs = (part) => part.replace(/\bnot\b/g, '!').replace(/\band\b/g, '&&').replace(/\bor\b/g, '||');
  const js = expression
    .split(/("[^"]*")/)
    .map((part, index) => (index % 2 ? part : toJs(part)))
    .join('');
  const names = Object.keys(scope);
  return new Function(...names, `return (${js});`)(...names.map((name) => scope[name]));
}

function renderValue(expression, scope) {
  const call = expression.match(ICON_CALL);
  if (call) return icon(call[1], Number(call[2]), call[3]);
  return escapeHtml(evaluate(expression, scope));
}

function renderBlock(text, scope) {
  let out = text;
  for (let previous = null; previous !== out; ) {
    previous = out;
    out = out.replace(IF_BLOCK, (_, condition, body) => (evaluate(condition, scope) ? body : ''));
  }
  return out.replace(VARIABLE, (_, expression) => renderValue(expression, scope));
}

function renderPage(context) {
  const loop = (_, rowBody, emptyBody) =>
    context.rows.length
      ? context.rows.map((row) => renderBlock(rowBody, {...context, row})).join('')
      : renderBlock(emptyBody, context);
  const markup = TEMPLATE.replace(INCLUDE, (_, name) => inlineInclude(name))
    .replace(COMMENT, '')
    .replace(SET_TAG, '')
    .replace(ROW_LOOP, loop);
  return renderBlock(markup, context);
}

// ---------------------------------------------------------------------------
// The fixture rows: each carries the fields the row route answers. The first login has an active attempt, with its
// paste input open and its last output; the second is an idle login that renews on its check.
// ---------------------------------------------------------------------------
function loginRow(id, fields) {
  return {
    id,
    kind: 'login',
    name: id,
    alias: null,
    hostname: null,
    label: id,
    state: 'ok',
    state_label: 'Renewed',
    detail: '',
    checked_text: 'Checked Nov 21, 2026 10:00',
    expiry_text: 'Last approved Nov 20, 2026',
    expiry_soon: false,
    next: '',
    live: false,
    live_text: '',
    live_link: '',
    input_open: false,
    input_prompt: '',
    last: false,
    last_text: '',
    last_output: '',
    busy: '',
    busy_text: '',
    check_url: `/api/host-auth/rows/${id}/check`,
    row_url: `/api/host-auth/rows/${id}`,
    start_url: `/api/host-auth/logins/${id}/start`,
    input_url: null,
    ...fields,
  };
}

const ACTIVE_LOGIN = loginRow('gh', {
  label: 'GitHub SSH certificate',
  state: 'needs_user',
  state_label: 'Needs you',
  detail: 'The approval page opened in a new tab. Approve there, then paste the callback URL it shows.',
  next: 'Start login, approve it in the new tab, and this row follows.',
  live: true,
  live_text: 'In progress: waiting for the callback URL. User code: not in the link yet.',
  live_link: 'https://github.com/login/device?user_code=ABCD-EFGH&client_id=0a1b2c3d4e5f60718293&state=4f9e2c1b7a6d',
  input_open: true,
  input_prompt: 'Callback URL',
  last: true,
  last_text: 'Last attempt: failed, finished Nov 21, 2026 09:59.',
  last_output: 'readback: the certificate is not in the agent yet; the check exited with status 255 after 12 s.',
  busy: 'login',
  busy_text: 'A login is in progress.',
  input_url: '/api/host-auth/logins/gh/input',
});
const IDLE_LOGIN = loginRow('aws', {label: 'AWS CLI SSO', expiry_text: 'Expires Nov 28, 2026 · 6 days left'});

// ---------------------------------------------------------------------------
// The stub server: the page, and the row routes. A check holds its row busy for `state.checkMs`, as the server does.
// `close` releases the server on every path: the check timers still pending, the responses still open and the
// listening socket.
// ---------------------------------------------------------------------------
function startServer(rows, checkMs) {
  const state = {rows: new Map(rows.map((row) => [row.id, row])), checking: new Set(), checks: 0, polls: 0, checkMs};
  const timers = new Set();
  const pageHtml = renderPage({rows, last_probe: 'Nov 21, 2026 09:30', interval_label: '27 min', backoff_label: '6 h'});
  const reply = (res, status, type, body) => {
    res.writeHead(status, {'Content-Type': type});
    res.end(body);
  };
  const answer = (res, id) => {
    const row = state.rows.get(id);
    const current = state.checking.has(id) ? {...row, busy: 'check', busy_text: ''} : row;
    reply(res, 200, 'application/json', JSON.stringify(current));
  };
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://127.0.0.1');
    if (req.method === 'GET' && url.pathname === '/') {
      reply(res, 200, 'text/html; charset=utf-8', pageHtml);
      return;
    }
    const check = url.pathname.match(/^\/api\/host-auth\/rows\/([^/]+)\/check$/);
    if (req.method === 'POST' && check) {
      const id = check[1];
      state.checking.add(id);
      const timer = setTimeout(() => {
        timers.delete(timer);
        state.checking.delete(id);
        state.checks += 1;
        answer(res, id);
      }, state.checkMs);
      timers.add(timer);
      return;
    }
    const row = url.pathname.match(/^\/api\/host-auth\/rows\/([^/]+)$/);
    if (req.method === 'GET' && row && state.rows.has(row[1])) {
      state.polls += 1;
      answer(res, row[1]);
      return;
    }
    reply(res, 404, 'text/plain; charset=utf-8', 'not found');
  });
  const close = () => {
    for (const timer of timers) clearTimeout(timer);
    timers.clear();
    server.closeAllConnections();
    server.close();
  };
  return new Promise((resolve) => {
    server.listen(0, '127.0.0.1', () => {
      resolve({state, close, url: `http://127.0.0.1:${server.address().port}/`});
    });
  });
}

// The page's load-time check of the idle login is over: the stub counted it past `after`, and the page applied the
// answer, so the row is no longer busy. The stub's count can come first, before the answer reaches the page.
async function awaitLoadCheck(page, state, after) {
  await until(
    async () => state.checks > after && state.checking.size === 0 &&
      (await page.evaluate(`document.querySelector('[data-row="aws"]').dataset.busy === ''`)),
    'the load-time check',
  );
}

// The stub server, a Chrome profile directory and Chrome, released together on every path: a launch that throws, a
// failed assertion and a passing run alike. A server or a profile directory that was made is always released.
async function withChrome(body, {checkMs = 50} = {}) {
  const fixture = await startServer([ACTIVE_LOGIN, IDLE_LOGIN], checkMs);
  let profileDir = null;
  let chrome = null;
  try {
    profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'host-auth-poll-'));
    chrome = await launchChrome(profileDir);
    await body(chrome, fixture);
  } finally {
    if (chrome) stopChrome(chrome);
    fixture.close();
    if (profileDir) fs.rmSync(profileDir, {recursive: true, force: true});
  }
}

// One page in Chrome at 1280x900, loaded from the stub, with the load-time check of the idle login finished.
async function withPage({checkMs = 50}, body) {
  await withChrome(async (chrome, fixture) => {
    const page = await openPage(chrome.cdp, fixture.url, {width: 1280, height: 900});
    await page.send('Emulation.setFocusEmulationEnabled', {enabled: true});
    await awaitLoadCheck(page, fixture.state, 0);
    await body(page, fixture.state);
  }, {checkMs});
}

async function waitForPolls(state, count) {
  const target = state.polls + count;
  await until(async () => state.polls >= target, `${count} poll cycles`);
}

// The active row's controls and the check's: each is shown, inside the window, and none overlaps another.
const LAYOUT_PROBE = `(() => {
  const controls = [...document.querySelectorAll(
    '[data-row="gh"] form[data-input] input, [data-row="gh"] form[data-input] button, ' +
    '[data-row="gh"] form[data-start] button, [data-row="gh"] form[data-check] button')]
    .filter((el) => el.offsetParent !== null)
    .map((el) => ({label: el.textContent || el.name, box: el.getBoundingClientRect()}));
  const inside = controls.every((c) => c.box.left >= 0 && c.box.right <= innerWidth);
  const overlapping = [];
  for (let i = 0; i < controls.length; i++) {
    for (let j = i + 1; j < controls.length; j++) {
      const a = controls[i].box;
      const b = controls[j].box;
      if (a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom) {
        overlapping.push([controls[i].label, controls[j].label]);
      }
    }
  }
  return {
    controls: controls.length,
    inside,
    overlapping,
    noHorizontalScroll: document.documentElement.scrollWidth <= innerWidth,
  };
})()`;

async function saveScreenshot(page, name) {
  const {data} = await page.send('Page.captureScreenshot', {format: 'png'});
  fs.mkdirSync(SHOTS_DIR, {recursive: true});
  fs.writeFileSync(path.join(SHOTS_DIR, name), Buffer.from(data, 'base64'));
}

test('a poll cycle keeps the paste input and the row buttons in place', async () => {
  await withPage({}, async (page, state) => {
    await page.evaluate(`(() => {
      const row = document.querySelector('[data-row="gh"]');
      const input = row.querySelector('form[data-input] input');
      input.focus();
      input.value = ${JSON.stringify(INPUT_VALUE)};
      input.setSelectionRange(8, 13);
      window.kept = {
        input,
        send: row.querySelector('form[data-input] button'),
        start: row.querySelector('form[data-start] button'),
        check: row.querySelector('form[data-check] button'),
      };
    })()`);

    await waitForPolls(state, 2);
    const kept = await page.evaluate(`(() => {
      const row = document.querySelector('[data-row="gh"]');
      const {input, send, start, check} = window.kept;
      return {
        inputSameNode: row.querySelector('form[data-input] input') === input,
        focused: document.activeElement === input,
        value: input.value,
        selection: [input.selectionStart, input.selectionEnd],
        sendSameNode: row.querySelector('form[data-input] button') === send,
        sendEnabled: !send.disabled,
        startSameNode: row.querySelector('form[data-start] button') === start,
        checkSameNode: row.querySelector('form[data-check] button') === check,
      };
    })()`);
    assert.deepEqual(kept, {
      inputSameNode: true,
      focused: true,
      value: INPUT_VALUE,
      selection: [8, 13],
      sendSameNode: true,
      sendEnabled: true,
      startSameNode: true,
      checkSameNode: true,
    });
  });
});

test('a poll that changes no fact touches no node of the table, and a selected output stays selected', async () => {
  await withPage({}, async (page, state) => {
    await page.evaluate(`(() => {
      const output = document.querySelector('[data-row="gh"] pre');
      const range = document.createRange();
      range.selectNodeContents(output);
      getSelection().removeAllRanges();
      getSelection().addRange(range);
      window.tableChanges = 0;
      new MutationObserver((changes) => {
        window.tableChanges += changes.length;
      }).observe(document.querySelector('tbody'), {childList: true, characterData: true, subtree: true});
    })()`);

    await waitForPolls(state, 2);
    const quiet = await page.evaluate(`({
      changes: window.tableChanges,
      selected: getSelection().toString(),
      output: document.querySelector('[data-row="gh"] pre').textContent,
    })`);
    assert.equal(quiet.changes, 0, 'a poll with no changed fact changes no node of the table');
    assert.ok(quiet.output.length > 0, 'the active row carries an output');
    assert.equal(quiet.selected, quiet.output, 'the selected output stays selected');

    state.rows.set('gh', {...state.rows.get('gh'), live_text: 'In progress: approved. User code: ABCD-1234.'});
    await until(
      () => page.evaluate(`document.querySelector('[data-row="gh"] [data-f="live_text"]')
        .textContent.includes('ABCD-1234')`),
      'the changed live text',
    );
  });
});

test('a check keeps its row controls in place and reads Checking. on its button while it runs', async () => {
  await withPage({checkMs: 1000}, async (page, state) => {
    await page.evaluate(`(() => {
      const row = document.querySelector('[data-row="aws"]');
      const button = row.querySelector('form[data-check] button');
      const start = row.querySelector('form[data-start] button');
      window.kept = {
        button,
        start,
        top: button.getBoundingClientRect().top,
        startTop: start.getBoundingClientRect().top,
      };
      button.click();
    })()`);
    await until(async () => state.checking.has('aws'), 'the check to start');

    const during = await page.evaluate(`(() => {
      const row = document.querySelector('[data-row="aws"]');
      const button = row.querySelector('form[data-check] button');
      const start = row.querySelector('form[data-start] button');
      return {
        checkSameNode: button === window.kept.button,
        checkTop: button.getBoundingClientRect().top,
        checkText: button.textContent,
        checkDisabled: button.disabled,
        startSameNode: start === window.kept.start,
        startTop: start.getBoundingClientRect().top,
        startDisabled: start.disabled,
        busy: row.dataset.busy,
        keptTop: window.kept.top,
        keptStartTop: window.kept.startTop,
      };
    })()`);
    assert.equal(during.checkSameNode, true, 'the check button is the same node');
    assert.equal(during.checkTop, during.keptTop, 'the check button keeps its place while the check runs');
    assert.equal(during.checkText, 'Checking.');
    assert.equal(during.checkDisabled, true);
    assert.equal(during.startSameNode, true, 'the start button is the same node');
    assert.equal(during.startTop, during.keptStartTop, 'the start button keeps its place while the check runs');
    assert.equal(during.startDisabled, true);
    assert.equal(during.busy, 'check');

    await until(
      () => page.evaluate(`document.querySelector('[data-row="aws"] form[data-check] button')
        .textContent === 'Check now'`),
      'the check to end',
    );
    const after = await page.evaluate(`(() => {
      const button = document.querySelector('[data-row="aws"] form[data-check] button');
      return {
        sameNode: button === window.kept.button,
        top: button.getBoundingClientRect().top,
        disabled: button.disabled,
        keptTop: window.kept.top,
      };
    })()`);
    assert.equal(after.sameNode, true, 'the check button is the same node after the check');
    assert.equal(after.top, after.keptTop, 'the check button keeps its place after the check');
    assert.equal(after.disabled, false);
  });
});

test('the active login and a running check fit each window size the landing captures', async () => {
  await withChrome(async (chrome, fixture) => {
    for (const size of WINDOW_SIZES) {
      const label = `${size.width}x${size.height}`;
      const loadChecks = fixture.state.checks;
      const page = await openPage(chrome.cdp, fixture.url, size);
      await page.send('Emulation.setFocusEmulationEnabled', {enabled: true});
      await awaitLoadCheck(page, fixture.state, loadChecks);

      await page.evaluate(`(() => {
        const input = document.querySelector('[data-row="gh"] form[data-input] input');
        input.value = ${JSON.stringify(INPUT_VALUE)};
        input.focus();
      })()`);
      const active = await page.evaluate(LAYOUT_PROBE);
      assert.ok(active.controls >= 4, `${label}: the active row shows its input, send, start and check`);
      assert.deepEqual(active.overlapping, [], `${label}: no control of the active row overlaps another`);
      assert.equal(active.inside, true, `${label}: every control of the active row is inside the window`);
      assert.equal(active.noHorizontalScroll, true, `${label}: the page does not scroll sideways`);
      await saveScreenshot(page, `active-${label}.png`);

      fixture.state.checkMs = 4000;
      await page.evaluate(`document.querySelector('[data-row="aws"] form[data-check] button').click()`);
      await until(async () => fixture.state.checking.has('aws'), `${label}: the check to start`);
      // The idle row's check is the state this capture shows, so its button is brought into the window.
      await page.evaluate(`document.querySelector('[data-row="aws"] form[data-check] button')
        .scrollIntoView({block: 'center'})`);
      const checking = await page.evaluate(LAYOUT_PROBE);
      assert.deepEqual(
        checking.overlapping,
        [],
        `${label}: no control of the active row overlaps another while a check runs`,
      );
      await saveScreenshot(page, `checking-${label}.png`);
      await until(async () => fixture.state.checking.size === 0, `${label}: the check to end`);
      fixture.state.checkMs = 50;
    }
  });
});
