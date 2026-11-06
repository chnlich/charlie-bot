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
import { LINK, OTHER_VIEWS, TASK_TREE_LOAD } from './profile.mjs';
import { percentile } from './report.mjs';
import { Tabs, pollFor, sleep, waitFor } from './tabs.mjs';

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
    trustedSpki: ctx.trustedSpki,
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

// The endpoints of the page's background traffic, by URL. The status poll, the full list and the row refresh repeat
// while the sidebar is open; the bootstrap is the switch's own request. The row refresh names its rows by `ids`. The
// full list is the shown view's whole list: the Workspace's names no `ids`, and the starred and Threads views have
// their own.
const ENDPOINTS = Object.freeze({
  status: /\/api\/sessions\/status\?/,
  rows: /\/api\/sessions\/\?[^#]*\bids=/,
  fullList: /\/api\/sessions\/((starred|chat-threads)(\?|$)|\?(?![^#]*\bids=)|$)/,
  bootstrap: /\/api\/sessions\/[^/]+\/bootstrap$/,
});

// The window's received bytes of each endpoint, summed over the pages and given in KB per minute. The socket is the
// frames of the pages' websockets. The proxy's own count of the window minus these is `other`.
function receivedByEndpoint(pages, from, to, minutes) {
  const sums = { status: 0, rows: 0, fullList: 0, bootstrap: 0, socket: 0 };
  for (const page of pages) {
    sums.status += page.bytesOf(ENDPOINTS.status, from, to);
    sums.rows += page.bytesOf(ENDPOINTS.rows, from, to);
    sums.fullList += page.bytesOf(ENDPOINTS.fullList, from, to);
    sums.bootstrap += page.bytesOf(ENDPOINTS.bootstrap, from, to);
    sums.socket += page.framesBetween(from, to).reduce((total, frame) => total + frame.bytes, 0);
  }
  return Object.fromEntries(Object.entries(sums).map(([name, bytes]) => [name, bytes / 1000 / minutes]));
}

// The fields that differ between two status bodies, each `{session id: {field: value}}`. A row only one body holds
// counts as `row added` or `row removed`; equal rows in another key order count as `key order`.
function changedStatusFields(beforeText, afterText) {
  const before = JSON.parse(beforeText);
  const after = JSON.parse(afterText);
  const fields = new Set();
  for (const id of new Set([...Object.keys(before), ...Object.keys(after)])) {
    if (!(id in before)) {
      fields.add('row added');
    } else if (!(id in after)) {
      fields.add('row removed');
    } else {
      for (const field of new Set([...Object.keys(before[id]), ...Object.keys(after[id])])) {
        if (JSON.stringify(before[id][field]) !== JSON.stringify(after[id][field])) {
          fields.add(field);
        }
      }
    }
  }
  if (fields.size === 0 && beforeText !== afterText) {
    fields.add('key order');
  }
  return fields;
}

// The tab's status polls that finished in [from, to]: how many, and what the server answered, by wire status. A 304
// carried no body: the page read the cached one. A 200 is compared with the body the tab last held for the same URL,
// before the window included: `changedBodies` differ from it, `unchangedBodies` repeat it (a browser that does not
// revalidate), and `changedFields` counts, per field, the polls in which it changed for any row. `newUrlBodies`
// answered a URL the tab had not polled before, such as an `ids` list in a new order. `noWireStatus` counts polls whose
// wire status Chrome did not report.
function statusPollCounts(tab, from, to, minutes) {
  const counts = {
    perMinute: 0,
    answered200: 0,
    answered304: 0,
    otherStatuses: 0,
    noWireStatus: 0,
    changedBodies: 0,
    unchangedBodies: 0,
    newUrlBodies: 0,
    changedFields: {},
  };
  const lastBody = new Map();
  for (const request of tab.requests.values()) {
    if (!ENDPOINTS.status.test(request.url) || request.finishAt === null) {
      continue;
    }
    const inWindow = request.finishAt >= from && request.finishAt <= to;
    if (inWindow) {
      counts.perMinute += 1 / minutes;
      if (request.wireStatus === 200) counts.answered200 += 1;
      else if (request.wireStatus === 304) counts.answered304 += 1;
      else if (request.wireStatus === null) counts.noWireStatus += 1;
      else counts.otherStatuses += 1;
    }
    // The page's body of the poll: the server's for a 200, the cached one for a 304. A poll that finished before the
    // tab kept bodies carries none, and the comparison starts after it.
    if (request.status !== 200 || request.body === undefined) {
      continue;
    }
    if (request.body === null) {
      throw new Error(`no body read for status poll ${request.url}: ${request.bodyError ?? 'the read never finished'}`);
    }
    const previous = lastBody.get(request.url);
    lastBody.set(request.url, request.body);
    if (!inWindow || request.wireStatus !== 200) {
      continue;
    }
    if (previous === undefined) {
      counts.newUrlBodies += 1;
      continue;
    }
    const fields = changedStatusFields(previous, request.body);
    if (fields.size === 0) {
      counts.unchangedBodies += 1;
      continue;
    }
    counts.changedBodies += 1;
    for (const field of fields) {
      counts.changedFields[field] = (counts.changedFields[field] ?? 0) + 1;
    }
  }
  return counts;
}

// The background load: one visible and two hidden tabs on the open session, the background session running a held turn,
// and the task-tree driver renaming `renamed` at the live rate. The visible tab shows the sidebar view `filter` (null:
// the Workspace); the hidden tabs show the Workspace. `body` runs inside the load, which ends on every path.
async function withBackground(ctx, { label, filter, renamed }, body) {
  const { api, sessions } = ctx;
  return withLink(ctx, label, ['visible', 'hidden-1', 'hidden-2'], async ({ proxy, tabs, tab, hidden }) => {
    const open = sessions.open;
    tab.keepBodies(ENDPOINTS.status);
    await tabs.navigate(tab, `/?session=${open.id}${filter ? `&filter=${filter}` : ''}`);
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
    const driver = startTaskTreeDriver(api, renamed ?? running);
    return withCleanup(async () => {
      await sleep(WARM_UP_MS);
      return body({ proxy, tab, hidden });
    }, async () => {
      await driver.stop();
      await api.cancelRuns(running);
    });
  });
}

// Scenario 1: bytes per minute at the proxy, each direction, over one task-tree window, with the received bytes split
// by endpoint and the visible tab's status polls.
async function measureWindow(proxy, tab, hidden) {
  const beforeWindow = proxy.counters();
  const windowStart = Date.now();
  await sleep(TASK_TREE_LOAD.windowMs);
  const afterWindow = proxy.counters();
  const windowEnd = Date.now();
  const minutes = (windowEnd - windowStart) / 60000;
  const received = ((afterWindow.received - beforeWindow.received) / 1000) / minutes;
  const sent = ((afterWindow.sent - beforeWindow.sent) / 1000) / minutes;
  const byEndpoint = receivedByEndpoint([tab, ...hidden], windowStart, windowEnd, minutes);
  byEndpoint.other = received - Object.values(byEndpoint).reduce((total, kb) => total + kb, 0);
  await tab.bodiesRead();
  return {
    receivedKbPerMin: received,
    receivedByEndpoint: byEndpoint,
    statusPoll: statusPollCounts(tab, windowStart, windowEnd, minutes),
    sentKbPerMin: sent,
  };
}

// Scenarios 1 to 3: one visible and two hidden tabs, a session running in the background.
export async function backgroundRep(ctx, repetition) {
  const { api, sessions } = ctx;
  const processing = await serverProcessingMs(ctx, sessions.probes);
  return withBackground(ctx, { label: 'background', filter: null, renamed: null }, async ({ proxy, tab, hidden }) => {
    const window = await measureWindow(proxy, tab, hidden);

    // Scenario 2: click to rendered, for a session that this page has not loaded.
    const target = sessions.switchTarget;
    await tab.watchChat('switch', target.marker);
    const clickedAt = await tab.clickSession(target.id);
    const shown = await tab.shownBy('switch', clickedAt, SHOW_TIMEOUT_MS);
    // The page's own bootstrap bytes when its request finished; otherwise the size the same body has on the wire.
    const pageBytes = tab.bytesOf(new RegExp(`/api/sessions/${target.id}/bootstrap$`), clickedAt, Infinity);
    const bootstrapBytes = pageBytes > 0 ? pageBytes : await api.bootstrapWireBytes(target.id);
    const switchState = shown.capped ? await tab.diagnose(clickedAt) : null;

    // Scenario 3: click to echo, in the session the page now shows. The echo is the server's push of the message,
    // which arrives as a socket frame carrying its text: the page shows the user's message locally before the server
    // answers, so the chat's text cannot time this step.
    const echo = `pn-echo ${repetition} ${randomTag()}`;
    const sentAt = await tab.sendText(echo);
    const echoAt = await pollFor(
      () => tab.frames.find((frame) => frame.at >= sentAt && frame.text.includes(echo))?.at ?? null,
      SHOW_TIMEOUT_MS,
    );
    const echoed = echoAt === null ? { at: sentAt + SHOW_TIMEOUT_MS, capped: true } : { at: echoAt, capped: false };
    const sendState = echoed.capped ? await tab.diagnose(sentAt) : null;

    return {
      ...window,
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
  });
}

// The list route of each other sidebar view, and the Slack origin that makes a created session a Threads-view root.
const VIEW_ROUTES = Object.freeze({ starred: '/api/sessions/starred', threads: '/api/sessions/chat-threads' });

function slackOrigin(index) {
  return { team_id: 'TBENCH', channel_id: 'CBENCH', thread_ts: `1700000000.${String(index).padStart(6, '0')}` };
}

// Fills a sidebar view to its live size (OTHER_VIEWS) and names the row the driver renames. The starred view holds the
// background session and the first mock rows of the Workspace, starred. The Threads view holds sessions the bench
// creates with a Slack origin, which makes each one a thread root. Returns the view's rows as its route lists them, and
// the median raw row.
async function populateView(ctx, filter) {
  const { api, sessions } = ctx;
  const rows = OTHER_VIEWS[filter].rows;
  let renamed;
  if (filter === 'starred') {
    const benchIds = new Set(Object.values(sessions).flat().map((session) => session.id));
    const workspace = await api.json('GET', '/api/sessions/');
    const others = workspace.map((row) => row.id).filter((id) => !benchIds.has(id)).slice(0, rows - 1);
    for (const id of [sessions.background.id, ...others]) {
      await api.json('POST', `/api/sessions/${id}/star`);
    }
    renamed = sessions.background.id;
  } else {
    const tag = randomTag();
    const created = [];
    for (let index = 1; index <= rows; index++) {
      const slots = { slack_origin: slackOrigin(index) };
      created.push(await api.createSession(`pn-thread-${tag}-${index}`, ctx.backendId, slots));
    }
    renamed = created[0];
  }
  const listed = await api.json('GET', VIEW_ROUTES[filter]);
  if (!Array.isArray(listed) || listed.length !== rows || !listed.some((row) => row.id === renamed)) {
    const count = Array.isArray(listed) ? listed.length : 'no';
    throw new Error(`${VIEW_ROUTES[filter]} lists ${count} rows, not the ${rows} the bench gave the view`);
  }
  const sizes = listed.map((row) => Buffer.byteLength(JSON.stringify(row)));
  return { renamed, rows: listed.length, rawRowMedianBytes: percentile(sizes, 0.5) };
}

// The background of scenario 1 with the visible tab on another sidebar view, which refetches its whole list on every
// task-tree frame. The view holds the rows populateView gave it, and the driver renames one of them, so the view's
// rows change at the rate the Workspace's do in scenario 1. Returns scenario 1's window numbers and the rows the tab
// painted.
export function viewBackgroundRep(filter) {
  return async (ctx) => {
    ctx.otherViews[filter] ??= await populateView(ctx, filter);
    const view = ctx.otherViews[filter];
    return withBackground(ctx, { label: filter, filter, renamed: view.renamed }, async ({ proxy, tab, hidden }) => {
      const rows = `document.querySelectorAll('#session-list a[id^="session-"]').length`;
      const painted = await tab.evaluate(`currentFilter === ${JSON.stringify(filter)} ? ${rows} : -1`);
      if (painted < 0) {
        throw new Error(`the visible tab does not show the ${filter} view`);
      }
      return { ...(await measureWindow(proxy, tab, hidden)), viewRowsPainted: painted };
    });
  };
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
