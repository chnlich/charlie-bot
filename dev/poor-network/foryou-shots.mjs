// The For-you screenshots: the three states the For-you change adds or alters, each at the CSS sizes the web UI rule
// names, taken on the after side once its measurement ends. Every capture runs in a fresh profile behind the link, the
// way a person's first load of the session runs.
//
//   switch: a click on the large target's row; the For-you view shows the rows the bootstrap's older_for_you carries.
//   retry: the same switch with every older-page request for the target failed; the page holds its retry state at the
//     top of the message list, where the page draws it.
//   offline: the large target opened online, then reopened with the network off; its For-you rows come from the copy.

import fs from 'node:fs';
import path from 'node:path';

import { deliverReplyOnline } from './open.mjs';
import { triggerOlderPageUntilFailure } from './foryou.mjs';
import { withLink } from './scenarios.mjs';
import { pollFor, sleep } from './tabs.mjs';

export const SHOT_SIZES = Object.freeze([
  Object.freeze({ width: 1536, height: 900 }),
  Object.freeze({ width: 1400, height: 1000 }),
  Object.freeze({ width: 390, height: 844 }),
]);

// The wait after a state's trigger, so the rows the page paints settle before the capture.
const SHOT_SETTLE_MS = 5000;
const SHOT_IDLE_TIMEOUT_MS = 300000;
const SHOT_SHOW_TIMEOUT_MS = 180000;
const RETRY_STATE_TIMEOUT_MS = 60000;

// Background requests that do not block the For-you screenshot click flow are left out of the pre-click idle wait.
const SHOT_IDLE_IGNORE_RE = /\/(?:api\/(?:sessions\/status|ext-usage|diag\/client-events)|favicon\.ico)(?:$|\?)/;
const isBackgroundShotRequest = (request) => SHOT_IDLE_IGNORE_RE.test(request.url);

const SENTINEL_FAILED = `document.getElementById('load-more-sentinel')?.dataset.state === 'failed' ? true : null`;
// The page's phone breakpoint (platform.js): at or below it the session list is a drawer, closed until its hamburger
// opens it, and a row inside a closed drawer is not painted.
const PHONE_MAX_WIDTH = 768;

async function setViewport(tab, size) {
  await tab.page.send('Emulation.setDeviceMetricsOverride', {
    width: size.width,
    height: size.height,
    deviceScaleFactor: 1,
    mobile: size.width <= PHONE_MAX_WIDTH,
  });
}

async function openPhoneDrawer(tab, size) {
  if (size.width > PHONE_MAX_WIDTH) return;
  await tab.evaluate(`document.querySelector('.hamburger-btn')?.click()`);
}

async function capture(tab, file) {
  const { data } = await tab.page.send('Page.captureScreenshot', { format: 'png' });
  fs.writeFileSync(file, Buffer.from(data, 'base64'));
  return file;
}

// Fails every older-page request for the target with a 500, so the page keeps its failed state and backs off.
function failEveryOlderPage(sessionId, marker) {
  return `(() => {
    const original = window.fetch;
    window.fetch = (input, init) => {
      const url = typeof input === 'string' ? input : (input && input.url) || '';
      const markerShown = (document.getElementById('messages')?.textContent || '')
        .includes(${JSON.stringify(marker)});
      if (markerShown && url.includes('/api/sessions/${sessionId}/events')) {
        return Promise.resolve(new Response('{"error":"injected"}', { status: 500 }));
      }
      return original.call(window, input, init);
    };
  })()`;
}

async function switchShot(ctx, size, target, file) {
  const name = `shot-switch-${size.width}`;
  return withLink(ctx, name, [], async ({ tabs }) => {
    const tab = await tabs.open(name);
    await setViewport(tab, size);
    await tabs.navigate(tab, `/?session=${ctx.sessions.open.id}`);
    await tab.ready();
    await tabs.show(tab);
    await tabs.waitIdle(SHOT_IDLE_TIMEOUT_MS, { ignore: isBackgroundShotRequest });
    await openPhoneDrawer(tab, size);
    await tab.showRow(target);
    await tab.watchChat(name, target.marker);
    const clickedAt = await tab.clickSession(target.id);
    const shown = await tab.shownBy(name, clickedAt, SHOT_SHOW_TIMEOUT_MS);
    if (shown.capped) {
      throw new Error(`${name}: the target's For-you row did not show within ${SHOT_SHOW_TIMEOUT_MS} ms`);
    }
    await sleep(SHOT_SETTLE_MS);
    return capture(tab, file);
  });
}

async function retryShot(ctx, size, target, file) {
  const name = `shot-retry-${size.width}`;
  return withLink(ctx, name, [], async ({ tabs }) => {
    const tab = await tabs.open(name);
    await setViewport(tab, size);
    await tabs.navigate(tab, `/?session=${ctx.sessions.open.id}`);
    await tab.ready();
    await tabs.show(tab);
    await tabs.waitIdle(SHOT_IDLE_TIMEOUT_MS, { ignore: isBackgroundShotRequest });
    await openPhoneDrawer(tab, size);
    await tab.showRow(target);
    await tab.watchChat(name, target.marker);
    await tab.evaluate(failEveryOlderPage(target.id, target.marker));
    const clickedAt = await tab.clickSession(target.id);
    const shown = await tab.shownBy(name, clickedAt, SHOT_SHOW_TIMEOUT_MS);
    if (shown.capped) {
      throw new Error(`${name}: the target's For-you row did not show within ${SHOT_SHOW_TIMEOUT_MS} ms`);
    }
    const failedAt = await triggerOlderPageUntilFailure(tab, RETRY_STATE_TIMEOUT_MS);
    if (failedAt === null) {
      throw new Error(`${name}: no older-page request failed after the first For-you row appeared`);
    }
    const failed = await pollFor(() => tab.evaluate(SENTINEL_FAILED), RETRY_STATE_TIMEOUT_MS);
    if (failed === null) {
      throw new Error(`${name}: no failed older-page state within ${RETRY_STATE_TIMEOUT_MS} ms`);
    }
    // The retry state sits at the top of the message list; a scroll there brings it into the capture's frame.
    await tab.evaluate(`document.getElementById('load-more-sentinel')?.scrollIntoView({block: 'center'})`);
    await pollFor(() => tab.evaluate(SENTINEL_FAILED), RETRY_STATE_TIMEOUT_MS);
    await sleep(SHOT_SETTLE_MS);
    return capture(tab, file);
  });
}

async function offlineShot(ctx, size, target, file) {
  const name = `shot-offline-${size.width}`;
  return withLink(ctx, name, [], async ({ tabs }) => {
    await deliverReplyOnline(tabs, target, `shot-${size.width}`, { waitForStoredCopy: true });
    const tab = await tabs.open(name);
    await setViewport(tab, size);
    await tabs.show(tab);
    await tab.page.send('Network.emulateNetworkConditions', {
      offline: true, latency: 0, downloadThroughput: -1, uploadThroughput: -1,
    });
    await tabs.navigate(tab, `/?session=${target.id}`);
    const shownMarker = `(document.getElementById('messages')?.textContent || '')`
      + `.includes(${JSON.stringify(target.marker)}) ? true : null`;
    const shown = await pollFor(() => tab.evaluate(shownMarker), SHOT_SHOW_TIMEOUT_MS);
    if (shown === null) {
      throw new Error(`${name}: the offline reopen did not show the For-you rows within ${SHOT_SHOW_TIMEOUT_MS} ms`);
    }
    await sleep(SHOT_SETTLE_MS);
    return capture(tab, file);
  });
}

// Writes the For-you states at every size into `dir`, one PNG per state and size, and returns the paths. Each size's
// three states run in order, and each state's failure stops the run with its message.
export async function forYouShots(ctx, dir) {
  fs.mkdirSync(dir, { recursive: true });
  const target = ctx.sessions.foryou.large;
  const written = [];
  for (const size of SHOT_SIZES) {
    const tag = `${size.width}x${size.height}`;
    written.push(await switchShot(ctx, size, target, path.join(dir, `switch-${tag}.png`)));
    written.push(await retryShot(ctx, size, target, path.join(dir, `retry-${tag}.png`)));
    written.push(await offlineShot(ctx, size, target, path.join(dir, `offline-${tag}.png`)));
  }
  return written;
}
