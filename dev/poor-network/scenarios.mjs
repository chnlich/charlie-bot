// The six scenarios of the README. Each repetition starts from a fresh browser profile behind a fresh proxy, so
// the page's own cache and state never carry from one repetition to the next. A repetition returns the raw numbers of
// its scenario; report.mjs turns them into the medians, the targets and the verdicts.
//
// Scenarios 1 to 3 share one repetition: the background load runs, the proxy counts its bytes for one window, and then
// one switch and one send happen in that same background. Scenarios 4, 5 and 6 each have their own repetition.

import crypto from 'node:crypto';
import fs from 'node:fs';

import { launchBrowser, chromeBinary } from './chrome.mjs';
import { startTaskTreeDriver, startTriggerChurn, startTurnChurn } from './api.mjs';
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
// The window after the first screen renders in which the switch's own requests still start: the
// idle-callback loads (usage, explain status) fire within ~1.5 s of it, and the status poll it
// triggers starts with it.
const SWITCH_SETTLE_MS = 5000;
// The wait for the next task_tree_changed frame and for the list refresh it starts; a frame comes every
// windowMs / changesPerWindow while the background runs.
const REFRESH_WAIT_TIMEOUT_MS = 60000;
const BOOTSTRAP_START_TIMEOUT_MS = 10000;

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
    const tabs = new Tabs({
      browser, assets: ctx.assets, accessKey: ctx.api.accessKey, origin: ctx.origin, cdn: 'local',
    });
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

// The sidebar list refreshes in flight in [from, to]: a refresh the window overlaps is requested at
// or before `to` and finishes at or after `from`. With fix A the Workspace refreshes changed rows by
// id and still refetch the whole list for the starred and Threads views, so both count as a list
// refresh. A switch's hold inside a refresh burst is what the calibration names, so the report tells
// those switches apart.
function overlappingListRefreshes(tab, from, to) {
  const overlapping = [];
  for (const request of tab.requests.values()) {
    if (!ENDPOINTS.fullList.test(request.url) && !ENDPOINTS.rows.test(request.url)) {
      continue;
    }
    const endsAt = request.finishAt === null ? Infinity : request.finishAt;
    if (request.startAt <= to && endsAt >= from) {
      overlapping.push({ startAt: request.startAt, finishAt: request.finishAt, bytes: request.bytes });
    }
  }
  return overlapping;
}

// The fields that differ between two row states, the field's name for each, and the ids of the rows that differ. A row
// only one state holds counts as `row added` or `row removed`.
function changedStatusFields(before, after) {
  const fields = new Set();
  const ids = new Set();
  for (const id of new Set([...before.keys(), ...after.keys()])) {
    if (!before.has(id)) {
      fields.add('row added');
      ids.add(id);
    } else if (!after.has(id)) {
      fields.add('row removed');
      ids.add(id);
    } else {
      const left = before.get(id);
      const right = after.get(id);
      for (const field of new Set([...Object.keys(left), ...Object.keys(right)])) {
        if (JSON.stringify(left[field]) !== JSON.stringify(right[field])) {
          fields.add(field);
          ids.add(id);
        }
      }
    }
  }
  return { fields, ids };
}

// The row state an answer leaves the page with. The change's server answers `{token, rows, gone}`: the rows that moved
// since the token the page sent, and the ids that left the filter's set. The baseline server answers the whole map. An
// answer with `rows` extends the state the page held; a whole map replaces it, its own rows included.
function statusStateAfter(state, text) {
  const body = JSON.parse(text);
  if (body && typeof body === 'object' && body.rows && typeof body.rows === 'object') {
    const next = new Map(state);
    for (const [id, row] of Object.entries(body.rows)) {
      next.set(id, row);
    }
    for (const id of body.gone ?? []) {
      next.delete(id);
    }
    return next;
  }
  return new Map(Object.entries(body));
}

// The tab's status polls that finished in [from, to]: how many, what the server answered by wire status, and what each
// 200 moved. A 304 carried no body: the page read the cached one. A 200 is applied to the state the tab held for its
// row set — the URL without its `since` token — and compared with it: `changedBodies` moved a row or the set,
// `unchangedBodies` repeated it, and `firstBodies` answered a row set the tab held no state for. `changedFields`
// counts, per field, the polls in which it changed for any row, and `changedRows` does the same by the role `roles`
// gives an id. `bytes200` and `bytes304` are the median bytes the tab received per answer of that status, headers
// included.
export function statusPollCounts(tab, from, to, minutes, roles = new Map()) {
  const counts = {
    perMinute: 0,
    answered200: 0,
    answered304: 0,
    otherStatuses: 0,
    noWireStatus: 0,
    changedBodies: 0,
    unchangedBodies: 0,
    firstBodies: 0,
    changedFields: {},
    changedRows: {},
    bytes200: [],
    bytes304: [],
  };
  const stateByRowSet = new Map();
  for (const request of tab.requests.values()) {
    if (!ENDPOINTS.status.test(request.url) || request.finishAt === null) {
      continue;
    }
    const inWindow = request.finishAt >= from && request.finishAt <= to;
    if (inWindow) {
      counts.perMinute += 1 / minutes;
      if (request.wireStatus === 200) {
        counts.answered200 += 1;
        counts.bytes200.push(request.bytes);
      } else if (request.wireStatus === 304) {
        counts.answered304 += 1;
        counts.bytes304.push(request.bytes);
      } else if (request.wireStatus === null) {
        counts.noWireStatus += 1;
      } else {
        counts.otherStatuses += 1;
      }
    }
    // The page's body of the poll: the server's for a 200, the cached one for a 304. A poll that finished before the
    // tab kept bodies carries none, and the comparison starts after it.
    if (request.status !== 200 || request.body === undefined) {
      continue;
    }
    if (request.body === null) {
      throw new Error(`no body read for status poll ${request.url}: ${request.bodyError ?? 'the read never finished'}`);
    }
    const rowSet = request.url.replace(/([?&])since=[^&]*&?/, '$1').replace(/[?&]$/, '');
    const previous = stateByRowSet.get(rowSet);
    const next = statusStateAfter(previous ?? new Map(), request.body);
    stateByRowSet.set(rowSet, next);
    if (!inWindow || request.wireStatus !== 200) {
      continue;
    }
    if (previous === undefined) {
      counts.firstBodies += 1;
      continue;
    }
    const { fields, ids } = changedStatusFields(previous, next);
    if (fields.size === 0) {
      counts.unchangedBodies += 1;
      continue;
    }
    counts.changedBodies += 1;
    for (const field of fields) {
      counts.changedFields[field] = (counts.changedFields[field] ?? 0) + 1;
    }
    for (const id of ids) {
      const role = roles.get(id) ?? 'other';
      counts.changedRows[role] = (counts.changedRows[role] ?? 0) + 1;
    }
  }
  counts.bytes200 = counts.bytes200.length > 0 ? percentile(counts.bytes200, 0.5) : null;
  counts.bytes304 = counts.bytes304.length > 0 ? percentile(counts.bytes304, 0.5) : null;
  return counts;
}

// The calibration's switch: clicked while the list refresh that a task_tree_changed frame has just triggered is in
// flight, the condition the plan's gate names. The bench reads the frame and the refresh from its own records, clicks
// at once, and then proves the condition: a refresh of that frame's batch was still in flight when the switch's
// bootstrap request started. A click that misses the refresh is an error, not a hold to score.
async function clickDuringListRefresh(tab, sessionId) {
  const framesSeen = tab.frames.filter((frame) => frame.type === 'task_tree_changed').length;
  const frame = await waitFor(
    () => tab.frames.filter((candidate) => candidate.type === 'task_tree_changed')[framesSeen] ?? null,
    REFRESH_WAIT_TIMEOUT_MS,
    'a task_tree_changed frame to trigger a list refresh',
  );
  const refresh = await waitFor(
    () =>
      [...tab.requests.values()].find(
        (request) =>
          (ENDPOINTS.fullList.test(request.url) || ENDPOINTS.rows.test(request.url)) && request.startAt > frame.at,
      ) ?? null,
    REFRESH_WAIT_TIMEOUT_MS,
    'the list refresh the frame started',
  );
  const clickedAt = await tab.clickSession(sessionId);
  const bootstrap = await waitFor(
    () =>
      [...tab.requests.values()].find(
        (request) => request.url.includes(`/api/sessions/${sessionId}/bootstrap`) && request.startAt >= clickedAt,
      ) ?? null,
    BOOTSTRAP_START_TIMEOUT_MS,
    "the switch's bootstrap request",
  );
  const stillInFlight = refresh.failed === null && (refresh.finishAt === null || refresh.finishAt >= bootstrap.startAt);
  if (!stillInFlight) {
    throw new Error(
      `the click at ${clickedAt} landed after the frame's list refresh had finished, so the hold is not a during-refresh hold`,
    );
  }
  return clickedAt;
}

// The background load: one visible and two hidden tabs on the open session, the background session running a held turn,
// the task-tree driver renaming `renamed` at the live rate, and `churn` carrying the status churn — the turn churn it
// broadcasts and the trigger churn it does not. The visible tab shows the sidebar view `filter` (null: the Workspace);
// the hidden tabs show the Workspace. `body` runs inside the load, which ends on every path.
async function withBackground(ctx, { label, filter, renamed, churn }, body) {
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
    const churnId = churn ?? sessions.churn.id;
    await api.sendMessage(running, `bench-hold background ${randomTag()}`);
    const driver = startTaskTreeDriver(api, renamed ?? running);
    const turnChurn = startTurnChurn(api, churnId);
    const triggerChurn = startTriggerChurn(api, churnId);
    return withCleanup(async () => {
      await sleep(WARM_UP_MS);
      return body({ proxy, tab, hidden });
    }, async () => {
      await triggerChurn.stop();
      await turnChurn.stop();
      await driver.stop();
      await api.cancelRuns(running);
    });
  });
}

// The roles the report names a changed row by: the churned row, the session the held turn runs in, and the open chat.
function churnRoles(sessions, churnId) {
  return new Map([
    [churnId, 'churn'],
    [sessions.churn.id, 'churn'],
    [sessions.background.id, 'background'],
    [sessions.open.id, 'open'],
  ]);
}

// Scenario 1: bytes per minute at the proxy, each direction, over one task-tree window, with the received bytes split
// by endpoint and the visible tab's status polls.
async function measureWindow(proxy, tab, hidden, roles) {
  // The sidebar spinner counters run over the same window as the byte counts: the visible tab's full sidebar
  // repaints and spinner restarts while the turn runs.
  await tab.installSidebarProbe();
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
  const probe = await tab.sidebarProbeCounts();
  return {
    receivedKbPerMin: received,
    receivedByEndpoint: byEndpoint,
    statusPoll: statusPollCounts(tab, windowStart, windowEnd, minutes, roles),
    sidebarSpin: probe && {
      repaints: probe.repaints,
      restarts: probe.restarts,
      repaintsPerMin: probe.repaints / minutes,
      restartsPerMin: probe.restarts / minutes,
    },
    sentKbPerMin: sent,
  };
}

// Scenarios 1 to 3: one visible and two hidden tabs, a session running in the background.
export async function backgroundRep(ctx, repetition) {
  const { api, sessions } = ctx;
  const processing = await serverProcessingMs(ctx, sessions.probes);
  return withBackground(ctx, { label: 'background', filter: null, renamed: null }, async ({ proxy, tab, hidden }) => {
    const window = await measureWindow(proxy, tab, hidden, churnRoles(sessions, sessions.churn.id));

    // Scenario 2: click to rendered, for a session that this page has not loaded.
    const target = sessions.switchTarget;
    await tab.watchSwitchPhase('switch', target.id);
    const clickedAt = await tab.clickSession(target.id);
    const shown = await tab.shownBy('switch', clickedAt, SHOW_TIMEOUT_MS);
    // The page's own bootstrap bytes when its request finished; otherwise the size the same body has on the wire.
    const pageBytes = tab.bytesOf(new RegExp(`/api/sessions/${target.id}/bootstrap$`), clickedAt, Infinity);
    const bootstrapBytes = pageBytes > 0 ? pageBytes : (await api.bootstrapWireBytes(target.id)).wireBytes;
    // The switch's own requests, from the click until 5 s after the first screen rendered: the
    // request paths naming the target session, with how often each went out. A path seen twice
    // within one switch is a repeated download.
    await sleep(SWITCH_SETTLE_MS);
    const switchRequests = tab.requestPathCounts(
      new RegExp(`/api/sessions/${target.id}/|session_id=${target.id}`), clickedAt, shown.at + SWITCH_SETTLE_MS);
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

    // The calibration's switch: a second switch in the same background, whose click the bench times to land while the
    // list refresh a task_tree_changed frame has just triggered is in flight. The scored switch above keeps its random
    // phase, so its numbers stay comparable with the baseline. The target is a held session, which the page has not
    // loaded and whose bootstrap has the live view's wire size.
    const duringTarget = sessions.held[0];
    await tab.watchChat('switch-during-refresh', duringTarget.marker);
    const duringClickedAt = await clickDuringListRefresh(tab, duringTarget.id);
    const duringShown = await tab.shownBy('switch-during-refresh', duringClickedAt, SHOW_TIMEOUT_MS);
    const duringPageBytes = tab.bytesOf(
      new RegExp(`/api/sessions/${duringTarget.id}/bootstrap$`),
      duringClickedAt,
      Infinity,
    );
    const duringBootstrapBytes =
      duringPageBytes > 0 ? duringPageBytes : (await api.bootstrapWireBytes(duringTarget.id)).wireBytes;
    const duringState = duringShown.capped ? await tab.diagnose(duringClickedAt) : null;

    return {
      ...window,
      switchMs: shown.at - clickedAt,
      switchCapped: shown.capped,
      switchFloorMs: floorMs(bootstrapBytes),
      switchBootstrapBytes: bootstrapBytes,
      switchListRefreshes: overlappingListRefreshes(tab, clickedAt, shown.at),
      switchRequests,
      switchRequestTotal: Object.values(switchRequests).reduce((total, count) => total + count, 0),
      switchState,
      switchDuringRefreshMs: duringShown.at - duringClickedAt,
      switchDuringRefreshCapped: duringShown.capped,
      switchDuringRefreshBootstrapBytes: duringBootstrapBytes,
      switchDuringRefreshState: duringState,
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

// Fills a sidebar view to its live size (OTHER_VIEWS) and names the row the driver renames and the row whose status
// churns. The starred view holds the background session, the churn session and the first mock rows of the Workspace,
// starred. The Threads view holds sessions the bench creates with a Slack origin, which makes each one a thread root.
// Returns the view's rows as its route lists them, and the median raw row.
async function populateView(ctx, filter) {
  const { api, sessions } = ctx;
  const rows = OTHER_VIEWS[filter].rows;
  let renamed;
  let churn;
  if (filter === 'starred') {
    const benchIds = new Set(Object.values(sessions).flat().map((session) => session.id));
    const workspace = await api.json('GET', '/api/sessions/');
    const others = workspace.map((row) => row.id).filter((id) => !benchIds.has(id)).slice(0, rows - 2);
    for (const id of [sessions.background.id, sessions.churn.id, ...others]) {
      await api.json('POST', `/api/sessions/${id}/star`);
    }
    renamed = sessions.background.id;
    churn = sessions.churn.id;
  } else {
    const tag = randomTag();
    const created = [];
    for (let index = 1; index <= rows; index++) {
      const slots = { slack_origin: slackOrigin(index) };
      created.push(await api.createSession(`pn-thread-${tag}-${index}`, ctx.backendId, slots));
    }
    renamed = created[0];
    churn = created[1];
  }
  const listed = await api.json('GET', VIEW_ROUTES[filter]);
  const named = [renamed, churn];
  if (!Array.isArray(listed) || listed.length !== rows || !named.every((id) => listed.some((row) => row.id === id))) {
    const count = Array.isArray(listed) ? listed.length : 'no';
    throw new Error(`${VIEW_ROUTES[filter]} lists ${count} rows, not the ${rows} the bench gave the view`);
  }
  const sizes = listed.map((row) => Buffer.byteLength(JSON.stringify(row)));
  return { renamed, churn, rows: listed.length, rawRowMedianBytes: percentile(sizes, 0.5) };
}

// The background of scenario 1 with the visible tab on another sidebar view, which refetches its whole list on every
// task-tree frame. The view holds the rows populateView gave it, the driver renames one of them and another one's
// status churns, so the view's rows change at the rate the Workspace's do in scenario 1. Returns scenario 1's window
// numbers and the rows the tab painted.
export function viewBackgroundRep(filter) {
  return async (ctx) => {
    ctx.otherViews[filter] ??= await populateView(ctx, filter);
    const view = ctx.otherViews[filter];
    return withBackground(ctx, { label: filter, filter, renamed: view.renamed, churn: view.churn },
      async ({ proxy, tab, hidden }) => {
        const rows = `document.querySelectorAll('#session-list a[id^="session-"]').length`;
        const painted = await tab.evaluate(`currentFilter === ${JSON.stringify(filter)} ? ${rows} : -1`);
        if (painted < 0) {
          throw new Error(`the visible tab does not show the ${filter} view`);
        }
        return {
          ...(await measureWindow(proxy, tab, hidden, churnRoles(ctx.sessions, view.churn))),
          viewRowsPainted: painted,
        };
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
      await tab.watchSwitchPhase(`held-${index}`, target.id);
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
    // An abort reaches the server only when the link returns, so each superseded bootstrap's
    // response was written in full during the blackout: its size on the wire, read from the
    // server. The browser never counts those bytes — an aborted request fails instead of
    // finishing — so the proxy's counters cannot attribute them either.
    let supersededServerBytes = 0;
    for (const target of clicks.slice(0, -1)) {
      supersededServerBytes += await api.bootstrapWireBytes(target.id);
    }
    return {
      restoreToRenderedMs: Math.max(shownAt, catchupAt) - restoreAt,
      // The two parts of the restore: the last switch's render, and the socket's catch-up.
      shownMs: shownAt - restoreAt,
      catchupMs: catchupAt - restoreAt,
      socketMs: socket.at - restoreAt,
      lastFloorMs: floorMs(lastBytes),
      lastBootstrapBytes: lastBytes,
      supersededServerBytes,
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
    const socket = tab.sockets.filter((entry) => entry.requestId === handshake.requestId).pop() ?? null;
    const catchupBytes = tab
      .framesBetween(handshake.at, catchupAt)
      .filter((frame) => frame.requestId === handshake.requestId)
      .reduce((sum, frame) => sum + frame.bytes, 0);
    return {
      reconnectMs: handshake.at - restoreAt,
      // The page's socket creation and the browser's first byte of its opening handshake, both after the restore. The
      // page owns the first; the browser's connection setup owns the second.
      socketMs: socket === null ? null : socket.at - restoreAt,
      // Every socket the page created, every handshake the browser started sending, every close it saw, and every
      // status poll the page sent, relative to the restore: which attempt won, and what was in flight beside it.
      socketAttemptsMs: tab.sockets.map((entry) => entry.at - restoreAt),
      handshakeSendsMs: tab.handshakeSends.map((entry) => entry.at - restoreAt),
      socketClosesMs: tab.socketCloses.map((entry) => ({ ms: entry.at - restoreAt, error: entry.error })),
      statusPollStartsMs: [...tab.requests.values()]
        .filter((request) => ENDPOINTS.status.test(request.url))
        .map((request) => request.startAt - restoreAt),
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
    await caughtUpSocket(tab, sessions.open.id);

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

// The page's socket for `sessionId`, open and past its catch-up. A reply sent before that arrives inside the replay
// instead of as stream frames, so the scenario would measure a catch-up rather than a stream.
async function caughtUpSocket(tab, sessionId) {
  const socket = await waitFor(
    () => tab.sockets.filter((entry) => entry.url.includes(`/ws/sessions/${sessionId}`)).pop() ?? null,
    SHOW_TIMEOUT_MS,
    `the page's socket for ${sessionId} to open`,
  );
  await waitFor(
    () => tab.frames.find((frame) => frame.type === 'catchup_complete' && frame.requestId === socket.requestId) ?? null,
    SHOW_TIMEOUT_MS,
    `the page's socket for ${sessionId} to finish its catch-up`,
  );
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
