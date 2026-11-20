// ---------------------------------------------------------------------------
// The composer of a session page in a headless Chrome at 1280 px and at a 390 px phone width. The session is bound to
// a scheduled task whose prompt runs longer than the viewport, so the read-only cron panel is taller than the screen.
// The panel shows the task's settings and the whole prompt. The composer must still sit inside the viewport, reachable
// by a point on it, and the conversation area keeps a minimum height. A session with no bound task keeps the same
// composer. The page is built from its own files by session_page_fixture.js.
//
// CHARLIEBOT_TEST_CHROME_BIN names the headless Chrome binary. The test fails when it is unset or names no file.
// ---------------------------------------------------------------------------
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test, {after, before} from 'node:test';
import {createRequire} from 'node:module';

const require = createRequire(import.meta.url);
const {launchChrome, openPage, startFixtureServer, stopChrome, until} = require('./session_page_fixture');

// One Chrome serves the file's cases; each case opens its own page and closes it.
let profileDir = null;
let chrome = null;
before(async () => {
  profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'session-composer-chrome-'));
  chrome = await launchChrome(profileDir);
});
after(() => {
  stopChrome(chrome);
  fs.rmSync(profileDir, {recursive: true, force: true});
});

const SESSION_ID = '8dcbbccc-f725-5790-a59b-fe0a859c324f';

const LONG_PROMPT = Array.from({length: 80}, (_, i) =>
  `Step ${i + 1}: measure one topic and write its result to the round report.`).join('\n');

const BOUND_TASK = {
  name: 'improve-perf',
  session_id: SESSION_ID,
  cron: '31 * * * *',
  timezone: 'America/Los_Angeles',
  enabled: true,
  prompt_file: 'dev/improve/perf.md',
  prompt: LONG_PROMPT,
  repo: '/work/repo',
  allow_failure: false,
  mode: null,
  handler: null,
  improve_host: 'host3',
  steps: [],
};

// The conversation area keeps at least this height whatever the prompt length: several message rows.
const CHAT_LOG_MIN_PX = 200;

const VIEWPORTS = [
  {width: 1280, height: 800},
  {width: 390, height: 844},
];

// The composer, the chat log and the cron panel, read after the panel's fetch has rendered.
const MEASURE = `(() => {
  const input = document.getElementById('msg-input');
  const r = input.getBoundingClientRect();
  const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
  return {
    viewportHeight: window.innerHeight,
    composerHidden: document.getElementById('input-area').classList.contains('hidden'),
    composerTop: r.top,
    composerBottom: r.bottom,
    composerHit: !!hit && (hit === input || input.contains(hit)),
    chatLogHeight: document.getElementById('messages').getBoundingClientRect().height,
    panelShown: !document.getElementById('cron-session-panel').classList.contains('hidden'),
    panelScrolls: document.getElementById('cron-session-panel').scrollHeight
      > document.getElementById('cron-session-panel').clientHeight,
    panelPrompts: [...document.querySelectorAll('#cron-session-panel pre')].map((pre) => pre.textContent),
  };
})()`;

async function measureSessionPage({tasks, width, height}) {
  const {server, served, base} = await startFixtureServer({sessionId: SESSION_ID, tasks});
  try {
    const page = await openPage(chrome.cdp, `${base}/?session=${SESSION_ID}`, {width, height});
    try {
      await until(() => served.cronTasks > 0, 'the cron panel fetch');
      // cron.js draws the panel after the response's JSON parses; the wait lets that draw land before the measurement.
      await new Promise((resolve) => setTimeout(resolve, 300));
      return await page.evaluate(MEASURE);
    } finally {
      await chrome.cdp.send('Target.closeTarget', {targetId: page.targetId});
    }
  } finally {
    server.close();
  }
}

function assertComposerInView(m) {
  assert.equal(m.composerHidden, false, 'the composer is not hidden');
  assert.ok(m.composerTop >= 0 && m.composerBottom <= m.viewportHeight,
    `composer spans ${m.composerTop}–${m.composerBottom} px in a ${m.viewportHeight} px viewport`);
  assert.equal(m.composerHit, true, 'another element covers the composer');
  assert.ok(m.chatLogHeight >= CHAT_LOG_MIN_PX,
    `conversation area is ${m.chatLogHeight} px tall, under the ${CHAT_LOG_MIN_PX} px minimum`);
}

for (const {width, height} of VIEWPORTS) {
  test(`a cron-bound session keeps its conversation and composer in view beside a long panel at ${width} px`,
    {timeout: 120000}, async () => {
      const m = await measureSessionPage({tasks: [BOUND_TASK], width, height});
      assert.equal(m.panelShown, true, 'the bound task shows in the cron panel');
      assert.deepEqual(m.panelPrompts, [LONG_PROMPT], 'the panel carries the whole prompt');
      assertComposerInView(m);
      assert.equal(m.panelScrolls, true, 'the panel scrolls, so the rest of the prompt is reachable');
    });

  test(`a session with no bound task keeps its conversation and composer in view at ${width} px`,
    {timeout: 120000}, async () => {
      const m = await measureSessionPage({tasks: [], width, height});
      assert.equal(m.panelShown, false, 'no cron panel shows without a bound task');
      assertComposerInView(m);
    });
}
