// ---------------------------------------------------------------------------
// The session page in a headless Chrome at 1280 px and at a 390 px phone width. The page renders no scheduled-task
// panel. Under the header sits the usage quota strip, which fills the page's below_header slot: when the server
// reports usage the strip shows there and the conversation starts under it; with no usage the strip stays hidden and
// the conversation starts directly under the header. The composer stays inside the viewport, reachable by a point on
// it, with the conversation area at a minimum height. The page is built from its own files by session_page_fixture.js.
// A bound session's main area, outside its message list, is the same DOM as an unbound session's. The bound task's
// settings and prompt sit in the Task & context dialog, which the last cases open with the real markdown renderer
// (cron_broken_ui.test.js covers the section's fields).
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
const {SESSION_ID, launchChrome, openPage, startFixtureServer, stopChrome, until} = require('./session_page_fixture');

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

// Opens the session page at one viewport, in the bound or the unbound mode, once its modules have loaded, and hands the
// page to `use`.
async function withSessionPage({width, height, usage, bound}, use) {
  const {server, served, base} = await startFixtureServer({usage, bound});
  try {
    const page = await openPage(chrome.cdp, `${base}/`, {width, height});
    try {
      await until(() => served.cronModule > 0 && served.extUsage > 0, 'the cron and usage module fetches');
      // The modules run after their fetches land; the wait lets their first runs finish before the measurement.
      await new Promise((resolve) => setTimeout(resolve, 300));
      return await use(page);
    } finally {
      await chrome.cdp.send('Target.closeTarget', {targetId: page.targetId});
    }
  } finally {
    server.close();
  }
}

async function measureSessionPage({width, height, usage}) {
  return withSessionPage({width, height, usage, bound: false}, (page) => page.evaluate(MEASURE));
}

// The page's main area with its message list and usage strip rows emptied. The bound and unbound sessions must match
// outside those two. The strip's rows carry relative reset times, so the strip is compared by its visibility and its
// row count.
const MAIN_AREA = `(() => {
  const strip = document.getElementById('ext-usage-strip');
  const main = document.querySelector('main').cloneNode(true);
  main.querySelector('#messages').replaceChildren();
  main.querySelector('#ext-usage-strip').replaceChildren();
  return {
    html: main.outerHTML,
    stripShown: !strip.classList.contains('hidden'),
    stripRows: strip.children.length,
    panelRendered: !!document.getElementById('cron-session-panel'),
  };
})()`;

// The Scheduled task section of the open dialog. Its code blocks are compared by the element classes they carry, against
// the Goal's own code block, which the dialog paints with the same renderer.
const DIALOG_SECTION = `(() => {
  const body = document.getElementById('task-context-modal-body');
  const label = [...body.querySelectorAll('div')].find((el) => el.textContent.trim() === 'Scheduled task');
  if (!label) return {found: false};
  const section = label.parentElement;
  const shape = (el) => [...el.querySelectorAll('*')].map((node) => node.tagName + '.' + node.className).join(' ');
  const goal = body.querySelector('.prose-msg');
  const heading = section.querySelector('h1');
  return {
    found: true,
    controls: section.querySelectorAll('input, select, textarea').length,
    buttons: [...section.querySelectorAll('button')].map((button) => button.className),
    codeBlocks: [...section.querySelectorAll('.code-block')].map(shape),
    goalCodeBlocks: [...goal.querySelectorAll('.code-block')].map(shape),
    headingText: heading ? heading.textContent : null,
    literalHeading: section.textContent.includes('# Weekly'),
  };
})()`;

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

for (const {width, height} of VIEWPORTS) {
  test(`a bound session shows the same main area as an unbound one outside its message list at ${width} px`,
    {timeout: 120000}, async () => {
      const unbound = await withSessionPage({width, height, usage: USAGE, bound: false},
        (page) => page.evaluate(MAIN_AREA));
      const bound = await withSessionPage({width, height, usage: USAGE, bound: true},
        (page) => page.evaluate(MAIN_AREA));
      assert.equal(bound.html, unbound.html, 'the main area outside the message list is the same for both sessions');
      for (const m of [bound, unbound]) {
        assert.equal(m.panelRendered, false, 'no cron panel is rendered');
        assert.equal(m.stripShown, true, 'the usage strip is shown');
        assert.ok(m.stripRows > 0, 'the usage strip paints rows');
      }
      assert.equal(bound.stripRows, unbound.stripRows, 'the usage strip has the same rows for both sessions');
    });
}

for (const {width, height} of VIEWPORTS) {
  test(`a bound task's Task & context dialog paints its four-fence prompt like the Goal, with no form or edit control, `
    + `at ${width} px`, {timeout: 120000}, async () => {
    const m = await withSessionPage({width, height, usage: USAGE, bound: true}, async (page) => {
      await page.evaluate(`openTaskContextModal(${JSON.stringify(SESSION_ID)}), true`);
      await until(async () => page.evaluate(
        "document.getElementById('task-context-modal-body').textContent.includes('Scheduled task')",
      ), 'the Scheduled task section in the dialog');
      return page.evaluate(DIALOG_SECTION);
    });
    assert.equal(m.found, true, 'the dialog paints the Scheduled task section');
    assert.equal(m.controls, 0, 'the section holds no input, select or textarea');
    assert.equal(m.buttons.length, 4, `the section's only buttons are its four code-block copy buttons: ${m.buttons}`);
    assert.ok(m.buttons.every((className) => className.includes('copy-btn')), `copy buttons only: ${m.buttons}`);
    assert.equal(m.codeBlocks.length, 4, 'each fenced block of the prompt renders as a code block');
    assert.equal(m.goalCodeBlocks.length, 1, 'the Goal renders its own code block as the reference');
    for (const shape of m.codeBlocks) {
      assert.equal(shape, m.goalCodeBlocks[0], 'a prompt code block is painted like the Goal code block');
    }
    assert.equal(m.headingText, 'Weekly attention review', 'the prompt heading renders as an element');
    assert.equal(m.literalHeading, false, 'no literal heading marker paints');
  });
}
