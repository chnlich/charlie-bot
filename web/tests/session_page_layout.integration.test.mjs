// ---------------------------------------------------------------------------
// The session page in a headless Chrome at 1280 px and at a 390 px phone width. The page renders no scheduled-task
// panel. Under the header sits the usage quota strip, which fills the page's below_header slot: when the server
// reports usage the strip shows there and the conversation starts under it; with no usage the strip stays hidden and
// the conversation starts directly under the header. The composer stays inside the viewport, reachable by a point on
// it, with the conversation area at a minimum height. The page is built from its own files by session_page_fixture.js.
// The bound task's settings and prompt sit in the Task & context dialog (cron_broken_ui.test.js covers the section).
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
  profileDir = fs.mkdtempSync(path.join(os.tmpdir(), 'session-page-chrome-'));
  chrome = await launchChrome(profileDir);
});
after(() => {
  stopChrome(chrome);
  fs.rmSync(profileDir, {recursive: true, force: true});
});

// The conversation area keeps at least this height: several message rows.
const CHAT_LOG_MIN_PX = 200;

const VIEWPORTS = [
  {width: 1280, height: 800},
  {width: 390, height: 844},
];

const HOUR = 60 * 60 * 1000;
const DAY = 24 * HOUR;
const iso = (offsetMs) => new Date(Date.now() + offsetMs).toISOString();

// One Claude account with its 5 h and 7 d windows, the shape /api/ext-usage answers with.
const USAGE = {
  providers: {
    'claude:main': {
      provider: 'claude',
      provider_label: 'Claude',
      account: 'main',
      windows: [
        {window_minutes: 300, utilization: 42.0, resets_at: iso(2 * HOUR)},
        {window_minutes: 10080, utilization: 10.0, resets_at: iso(3 * DAY)},
      ],
      fetched_at: iso(-60 * 1000),
    },
  },
};

// The header, the usage strip and its wrapper, the conversation, the composer and any cron panel, read after the
// modules have loaded.
const MEASURE = `(() => {
  const input = document.getElementById('msg-input');
  const r = input.getBoundingClientRect();
  const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
  const strip = document.getElementById('ext-usage-strip');
  const wrapper = strip.parentElement.getBoundingClientRect();
  return {
    viewportHeight: window.innerHeight,
    composerHidden: document.getElementById('input-area').classList.contains('hidden'),
    composerTop: r.top,
    composerBottom: r.bottom,
    composerHit: !!hit && (hit === input || input.contains(hit)),
    chatLogHeight: document.getElementById('messages').getBoundingClientRect().height,
    headerBottom: document.querySelector('main > header').getBoundingClientRect().bottom,
    stripShown: !strip.classList.contains('hidden'),
    stripRows: strip.children.length,
    stripHeight: strip.getBoundingClientRect().height,
    wrapperTop: wrapper.top,
    wrapperBottom: wrapper.bottom,
    conversationTop: document.getElementById('tab-chat').getBoundingClientRect().top,
    panelRendered: !!document.getElementById('cron-session-panel'),
  };
})()`;

async function measureSessionPage({width, height, usage}) {
  const {server, served, base} = await startFixtureServer({usage});
  try {
    const page = await openPage(chrome.cdp, `${base}/`, {width, height});
    try {
      await until(() => served.cronModule > 0 && served.extUsage > 0, 'the cron and usage module fetches');
      // The modules run after their fetches land; the wait lets their first runs finish before the measurement.
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
  test(`the usage strip shows under the header and the conversation starts under the strip at ${width} px`,
    {timeout: 120000}, async () => {
      const m = await measureSessionPage({width, height, usage: USAGE});
      assert.equal(m.panelRendered, false, 'no cron panel is rendered');
      assert.equal(m.stripShown, true, 'the usage strip is shown for a reported account');
      assert.ok(m.stripRows > 0 && m.stripHeight > 0, `the strip paints rows (${m.stripRows} rows, ${m.stripHeight} px)`);
      assert.ok(Math.abs(m.wrapperTop - m.headerBottom) < 1,
        `the strip starts at ${m.wrapperTop} px, under a header that ends at ${m.headerBottom} px`);
      assert.ok(Math.abs(m.conversationTop - m.wrapperBottom) < 1,
        `the conversation starts at ${m.conversationTop} px, under a strip that ends at ${m.wrapperBottom} px`);
      assertComposerInView(m);
    });

  test(`with no usage reported the conversation starts directly under the header at ${width} px`,
    {timeout: 120000}, async () => {
      const m = await measureSessionPage({width, height, usage: {providers: {}}});
      assert.equal(m.panelRendered, false, 'no cron panel is rendered');
      assert.equal(m.stripShown, false, 'the usage strip stays hidden without a reported account');
      assert.ok(Math.abs(m.conversationTop - m.headerBottom) < 1,
        `the conversation starts at ${m.conversationTop} px, under a header that ends at ${m.headerBottom} px`);
      assertComposerInView(m);
    });
}
