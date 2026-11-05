// The six scenarios of the README. Each repetition starts from a fresh browser profile behind a fresh proxy, so
// the page's own cache and state never carry from one repetition to the next. A repetition returns the raw numbers of
// its scenario; report.mjs turns them into the medians, the targets and the verdicts.
//
// Scenarios 1 to 3 share one repetition: the background load runs, the proxy counts its bytes for one window, and then
// one switch and one send happen in that same background. Scenarios 4, 5 and 6 each have their own repetition.

import crypto from 'node:crypto';
import fs from 'node:fs';

import { launchBrowser, chromeBinary } from './chrome.mjs';
import { startTaskTreeDriver } from './api.mjs';
import { ShapingProxy } from './proxy.mjs';
import { LINK, TASK_TREE_LOAD } from './profile.mjs';
import { Tabs, sleep, waitFor } from './tabs.mjs';

// Bytes per millisecond of the link: 12.5 KB/s.
const BYTES_PER_MS = LINK.bytesPerSecond / 1000;
// The one-way share of the round trip that a request's first byte always pays, plus the server's answer time.
const ROUND_TRIP_MS = LINK.roundTripMs;
const SETTLE_TIMEOUT_MS = 300000;
const WARM_UP_MS = 10000;
const SHOW_TIMEOUT_MS = 180000;

export function floorMs(bytes) {
  return ROUND_TRIP_MS + bytes / BYTES_PER_MS;
}

function randomTag() {
  return crypto.randomBytes(6).toString('hex');
}

// Runs `body`, then `cleanup`, on every path. When the body fails and the cleanup fails too, the body's error is the one
// thrown, with the cleanup's error named in its message, so the first failure is the one reported.
async function withCleanup(body, cleanup) {
  let value;
  try {
    value = await body();
  } catch (caught) {
    try {
      await cleanup();
    } catch (cleanupError) {
      caught.message = `${caught.message}; the cleanup failed too: ${cleanupError.message}`;
    }
    throw caught;
  }
  await cleanup();
  return value;
}

// One repetition's browser, proxy and tabs, torn down on every path.
async function withLink(ctx, label, tabNames, body) {
  const proxy = new ShapingProxy({ target: { host: '127.0.0.1', port: ctx.frontPort } });
  const proxyPort = await proxy.start();
  const profile = `${ctx.runDir}/profile-${label}-${randomTag()}`;
  const browser = await launchBrowser({
    binary: chromeBinary(),
    proxyPort,
    userDataDir: profile,
    logPath: `${ctx.runDir}/chrome.log`,
  });
  ctx.chromeVersion = browser.version;
  try {
    const tabs = new Tabs({ browser, assets: ctx.assets, accessKey: ctx.api.accessKey, origin: ctx.origin });
    const opened = [];
    for (const name of tabNames) {
      opened.push(await tabs.open(name));
    }
    return await body({ proxy, tabs, tab: opened[0], hidden: opened.slice(1) });
  } finally {
    await browser.close();
    await proxy.stop();
    fs.rmSync(profile, { recursive: true, force: true });
  }
}

// The probe answers: the server's time for one message, measured on the direct path, not through the link. The floor
// of scenario 3 adds it, so the floor names the server's own work.
async function serverProcessingMs(ctx, probes) {
  const times = [];
  for (const session of probes) {
    times.push(await ctx.api.sendMessage(session.id, `pn-probe ${randomTag()}`));
  }
  times.sort((left, right) => left - right);
  return times[Math.floor(times.length / 2)];
}

// Scenarios 1 to 3: one visible and two hidden tabs, a session running in the background.
export async function backgroundRep(ctx, repetition) {
  const { api, sessions } = ctx;
  const processing = await serverProcessingMs(ctx, sessions.probes);
  return withLink(ctx, 'background', ['visible', 'hidden-1', 'hidden-2'], async ({ proxy, tabs, tab, hidden }) => {
    const open = sessions.open;
    await tabs.navigate(tab, `/?session=${open.id}`);
    for (const other of hidden) {
      await tabs.navigate(other, `/?session=${open.id}`);
    }
    for (const page of [tab, ...hidden]) {
      await page.ready();
    }
    await tabs.show(tab);
    await tabs.waitIdle(SETTLE_TIMEOUT_MS);

    const running = sessions.background.id;
    await api.sendMessage(running, `bench-hold background ${randomTag()}`);
    const driver = startTaskTreeDriver(api, running);
    return withCleanup(async () => {
      await sleep(WARM_UP_MS);

      // Scenario 1: bytes per minute at the proxy, each direction, over one task-tree window.
      const beforeWindow = proxy.counters();
      const windowStart = Date.now();
      await sleep(TASK_TREE_LOAD.windowMs);
      const afterWindow = proxy.counters();
      const minutes = (Date.now() - windowStart) / 60000;
      const received = ((afterWindow.received - beforeWindow.received) / 1000) / minutes;
      const sent = ((afterWindow.sent - beforeWindow.sent) / 1000) / minutes;

      // Scenario 2: click to rendered, for a session that this page has not loaded.
      const target = sessions.switchTarget;
      await tab.watchChat('switch', target.marker);
      const clickedAt = await tab.clickSession(target.id);
      const shown = await tab.shownBy('switch', clickedAt, SHOW_TIMEOUT_MS);
      // The page's own bootstrap bytes when its request finished; otherwise the size the same body has on the wire.
      const pageBytes = tab.bytesOf(new RegExp(`/api/sessions/${target.id}/bootstrap$`), clickedAt, Infinity);
      const bootstrapBytes = pageBytes > 0 ? pageBytes : await api.bootstrapWireBytes(target.id);
      const switchState = shown.capped ? await tab.diagnose(clickedAt) : null;

      // Scenario 3: click to echo, in the session the page now shows.
      const echo = `pn-echo ${repetition} ${randomTag()}`;
      await tab.watchChat('echo', echo);
      const sentAt = await tab.sendText(echo);
      const echoed = await tab.shownBy('echo', sentAt, SHOW_TIMEOUT_MS);
      const sendState = echoed.capped ? await tab.diagnose(sentAt) : null;

      return {
        receivedKbPerMin: received,
        sentKbPerMin: sent,
        switchMs: shown.at - clickedAt,
        switchCapped: shown.capped,
        switchFloorMs: floorMs(bootstrapBytes),
        switchBootstrapBytes: bootstrapBytes,
        switchState,
        sendMs: echoed.at - sentAt,
        sendCapped: echoed.capped,
        sendFloorMs: ROUND_TRIP_MS + processing,
        sendState,
        serverProcessingMs: processing,
      };
    }, async () => {
      await driver.stop();
      await api.cancelRuns(running);
    });
  });
}

// Scenario 4: a 30 s held blackout with five switch clicks inside it.
export async function heldBlackoutRep(ctx) {
  const { api, sessions } = ctx;
  return withLink(ctx, 'held', ['visible'], async ({ proxy, tabs, tab }) => {
    await tabs.navigate(tab, `/?session=${sessions.open.id}`);
    await tab.ready();
    await tabs.show(tab);
    await tabs.waitIdle(SETTLE_TIMEOUT_MS);

    const clicks = sessions.held;
    const restoreAt = Date.now() + LINK.blackoutSeconds * 1000;
    proxy.blackout({ seconds: LINK.blackoutSeconds, mode: 'hold' });
    for (const [index, target] of clicks.entries()) {
      await tab.watchChat(`held-${index}`, target.marker);
      await sleep(index === 0 ? 2000 : 3000);
      await tab.clickSession(target.id);
    }
    const last = clicks[clicks.length - 1];
    const shownAt = await tab.whenShown(`held-${clicks.length - 1}`, SHOW_TIMEOUT_MS);
    const socket = await socketOpenedAfter(tab, last.id, restoreAt);
    const catchupAt = await catchupAfter(tab, socket, restoreAt);
    const lastBytes = tab.bytesOf(new RegExp(`/api/sessions/${last.id}/bootstrap$`), 0, Infinity);
    if (lastBytes === 0) {
      throw new Error(`no bootstrap bytes recorded for the last switch to ${last.id}`);
    }
    const superseded = clicks.slice(0, -1).reduce(
      (sum, target) => sum + tab.bytesOf(new RegExp(`/api/sessions/${target.id}/bootstrap$`), restoreAt, Infinity),
      0,
    );
    return {
      restoreToRenderedMs: Math.max(shownAt, catchupAt) - restoreAt,
      lastFloorMs: floorMs(lastBytes),
      lastBootstrapBytes: lastBytes,
      supersededDownBytes: superseded,
      blackoutProxyBytes: proxy.counters(),
    };
  });
}

// Scenario 5: a 30 s blackout that resets every connection when it starts.
export async function resetBlackoutRep(ctx) {
  const { sessions } = ctx;
  return withLink(ctx, 'reset', ['visible'], async ({ proxy, tabs, tab }) => {
    await tabs.navigate(tab, `/?session=${sessions.open.id}`);
    await tab.ready();
    await tabs.show(tab);
    await tabs.waitIdle(SETTLE_TIMEOUT_MS);

    const restoreAt = Date.now() + LINK.blackoutSeconds * 1000;
    proxy.blackout({ seconds: LINK.blackoutSeconds, mode: 'reset' });
    const handshake = await waitFor(
      () => tab.handshakes.find((entry) => entry.at >= restoreAt && entry.status === 101) ?? null,
      SHOW_TIMEOUT_MS,
      'the socket to reconnect',
    );
    const catchupAt = await catchupAfter(tab, { requestId: handshake.requestId, at: handshake.at }, restoreAt);
    const catchupBytes = tab
      .framesBetween(handshake.at, catchupAt)
      .filter((frame) => frame.requestId === handshake.requestId)
      .reduce((sum, frame) => sum + frame.bytes, 0);
    return {
      reconnectMs: handshake.at - restoreAt,
      restoreToCatchupMs: catchupAt - restoreAt,
      catchupBytes,
    };
  });
}

// Scenario 6: the socket bytes of one reply of about 8 KB.
export async function longStreamRep(ctx) {
  const { sessions } = ctx;
  return withLink(ctx, 'stream', ['visible'], async ({ tabs, tab }) => {
    await tabs.navigate(tab, `/?session=${sessions.open.id}`);
    await tab.ready();
    await tabs.show(tab);
    await tabs.waitIdle(SETTLE_TIMEOUT_MS);

    const sentAt = await tab.sendText(`pn-long ${randomTag()} answer with the reply`);
    const finished = await waitFor(
      () => tab.frames.find((frame) => frame.at >= sentAt && frame.type === 'master_done') ?? null,
      SHOW_TIMEOUT_MS,
      'the reply to finish',
    ).catch(async (caught) => {
      throw new Error(`${caught.message}; seen since the send: ${await tab.diagnose(sentAt)}`);
    });
    const socketBytes = tab.framesBetween(sentAt, finished.at).reduce((sum, frame) => sum + frame.bytes, 0);
    return { socketBytes, replyTextBytes: ctx.replyBytes, frameBreakdown: tab.frameBreakdown(sentAt, finished.at) };
  });
}

// The socket the page opened for `sessionId` after `after`: the last socket created whose URL names that session.
async function socketOpenedAfter(tab, sessionId, after) {
  const socket = await waitFor(
    () => tab.sockets.filter((entry) => entry.at >= after && entry.url.includes(sessionId)).pop() ?? null,
    SHOW_TIMEOUT_MS,
    `a socket for ${sessionId}`,
  );
  return socket;
}

// The time of the catch-up frame on `socket`, the first one at or after `after`.
async function catchupAfter(tab, socket, after) {
  const frame = await waitFor(
    () =>
      tab.frames.find(
        (entry) =>
          entry.type === 'catchup_complete' && entry.at >= after && (socket.requestId === undefined || entry.requestId === socket.requestId),
      ) ?? null,
    SHOW_TIMEOUT_MS,
    'catchup_complete',
  );
  return frame.at;
}
