// The restart scenario: a release that restarts the server while a tab is open, measured as plan 6 v4 section 4.2
// defines it. Each repetition publishes one release (a comment appended to the trial tree's served config.js, then the
// server restarted from the same launch as the trial's own), and times the measured tab's catch-up: the probe message
// the bench posts during the restart is on screen when the page is current. The gated wait of each cell runs from its
// return to the page's catch-up (its first catch-up after its last document started); the probe row's paint is reported
// beside it. The cells are:
//   restart-visible     the tab stays visible; the return is the replacement's server_ready.
//   restart-hidden-1    the tab is hidden; it is shown 1 s after its catch-up completes, while the stored shell refresh
//                       runs (expected: the refresh is cancelled, the page keeps its code, no navigation).
//   restart-hidden-2    the tab is hidden; it is shown 0.3 s after the page's reload navigation starts (expected: the
//                       stored shell answers it).
//   restart-hidden-3    the tab is hidden; it is shown once the reloaded document has loaded (expected: zero wait).
//   restart-frozen      the tab is hidden and frozen (Page.setWebLifecycleState) across the release, then thawed;
//                       shown once the reloaded document has loaded.
//   restart-1006        the tab is hidden; the link resets every connection when the server stops and holds for the
//                       link's blackout; shown once the reloaded document has loaded.
// A hidden tab that the phase never reaches is shown at the cap (HIDDEN_CAP_MS), so a side whose page never swaps
// while hidden still ends its repetition. The record names whether the cap ended it.

import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';

import { LINK } from './profile.mjs';
import { assetVersionOf } from './open.mjs';
import { sleep, waitFor } from './tabs.mjs';
import { withLink } from './scenarios.mjs';

const OBSERVE_INTERVAL_MS = 50;
// The wait a hidden tab gets for its phase, from the release (plan 6 v4: "保持上限 120 秒").
const HIDDEN_CAP_MS = 120000;
const SHOW_TIMEOUT_MS = 180000;
const SETTLE_TIMEOUT_MS = 300000;
const SERVER_STOP_TIMEOUT_MS = 60000;
const SERVER_READY_TIMEOUT_MS = 120000;
// Point 1: the tab is shown this long after its catch-up completes.
const CATCHUP_TO_SHOW_MS = 1000;
// Point 2: the tab is shown this long after the reload navigation starts.
const NAVIGATION_TO_SHOW_MS = 300;
const RELEASE_MARK = '// bench restart release';
const BOOT_SCRIPT =
  "window.__bootAt = Date.now(); document.addEventListener('charliebot:offline-shell-warm', (event) => { window.__warmReport = event.detail; });";
const NAVIGATION_RACE = /Execution context was destroyed|Cannot find context with specified id|navigated or closed/;

// The server the bench restarts: the trial's own at first, then the replacement this module started.
let serving = null;

function servingPid() {
  if (serving === null) {
    const pid = Number(process.env.CHARLIEBOT_SERVE_PID);
    if (!Number.isInteger(pid) || pid <= 0) {
      throw new Error('CHARLIEBOT_SERVE_PID must name the trial server the restart scenario replaces');
    }
    serving = { pid, child: null };
  }
  return serving.pid;
}

// The running server's launch, read from /proc before it stops: the same executable, arguments, environment, working
// directory and log file its replacement needs.
function launchOf(pid) {
  const argv = fs.readFileSync(`/proc/${pid}/cmdline`, 'utf8').split('\0').filter((arg) => arg !== '');
  const env = Object.fromEntries(
    fs.readFileSync(`/proc/${pid}/environ`, 'utf8').split('\0').filter((entry) => entry !== '').map((entry) => {
      const cut = entry.indexOf('=');
      return [entry.slice(0, cut), entry.slice(cut + 1)];
    }),
  );
  return {
    argv,
    env,
    cwd: fs.readlinkSync(`/proc/${pid}/cwd`),
    log: fs.readlinkSync(`/proc/${pid}/fd/1`),
  };
}

// Whether a process runs: a zombie has exited, and its parent has not reaped it yet.
function alive(pid) {
  try {
    const state = fs.readFileSync(`/proc/${pid}/stat`, 'utf8').replace(/^.*\)\s/, '')[0];
    return state !== 'Z' && state !== 'X';
  } catch (error) {
    if (error.code === 'ENOENT') {
      return false;
    }
    throw error;
  }
}

// Polls `check` until it returns a value, or throws at the time limit.
async function poll(check, timeoutMs, label) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await check();
    if (value) {
      return value;
    }
    if (Date.now() >= deadline) {
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${label}`);
    }
    await sleep(OBSERVE_INTERVAL_MS);
  }
}

// The server_ready time the replacement logged after `offset`, in epoch milliseconds, or null before the line lands.
function readyTimeSince(log, offset) {
  const text = fs.readFileSync(log).subarray(offset).toString('utf8');
  const line = text.split('\n').filter((candidate) => / server_ready/.test(candidate)).pop();
  if (line === undefined) {
    return null;
  }
  const stamp = /(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3})Z/.exec(line);
  if (stamp === null) {
    throw new Error(`the server_ready line carries no UTC timestamp: ${line}`);
  }
  return Date.parse(`${stamp[1].replace(' ', 'T')}Z`);
}

// Stops the serving server and starts the same launch again. Returns the replacement's server_ready time.
async function restartServer(ctx) {
  const pid = servingPid();
  const launch = launchOf(pid);
  process.kill(pid, 'SIGTERM');
  await poll(async () => !alive(pid), SERVER_STOP_TIMEOUT_MS, `server ${pid} to stop`);
  const offset = fs.statSync(launch.log).size;
  const out = fs.openSync(launch.log, 'a');
  const child = spawn(launch.argv[0], launch.argv.slice(1), {
    cwd: launch.cwd,
    env: launch.env,
    stdio: ['ignore', out, out],
  });
  fs.closeSync(out);
  serving = { pid: child.pid, child };
  const readyMs = await poll(async () => readyTimeSince(launch.log, offset), SERVER_READY_TIMEOUT_MS, 'server_ready');
  await poll(async () => {
    try {
      return (await ctx.api.request('GET', '/api/auth/status')).status === 200;
    } catch (error) {
      if (error.cause?.code === 'ECONNREFUSED') {
        return false;
      }
      throw error;
    }
  }, SERVER_READY_TIMEOUT_MS, 'the replacement to answer');
  return readyMs;
}

// A release: the served static tree gains a comment, and the server restarts from its own tree.
async function publishRelease(ctx, tag) {
  const root = process.env.CHARLIEBOT_REPO_ROOT;
  if (!root) {
    throw new Error('CHARLIEBOT_REPO_ROOT must name the trial tree that the server serves static files from');
  }
  fs.appendFileSync(path.join(root, 'web', 'static', 'js', 'config.js'), `\n${RELEASE_MARK} ${tag}\n`);
  return restartServer(ctx);
}

async function servedAssetVersion(ctx, sessionId) {
  const { status, body } = await ctx.api.request('GET', `/?session=${sessionId}`);
  if (status !== 200) {
    throw new Error(`GET the chat page answered ${status}`);
  }
  return assetVersionOf(body);
}

// The probe state of the measured page, read in one evaluation. A navigation or a frozen page answers null.
async function readPage(tab, tag) {
  const expression = `(() => ({
    row: (document.getElementById('messages')?.textContent ?? '').includes(${JSON.stringify(tag)}),
    boot: window.__bootAt ?? null,
    complete: document.readyState === 'complete',
  }))()`;
  try {
    return await tab.evaluate(expression);
  } catch (error) {
    if (NAVIGATION_RACE.test(error.message)) {
      return null;
    }
    throw error;
  }
}

// Watches the measured page until stopped: when the probe row first shows, each new document's start, and when the
// newest document has loaded.
function observe(tab, tag, bootAt0) {
  const state = { rowAt: null, documents: [], loadedAt: null, stop: false };
  let lastBoot = bootAt0;
  const done = (async () => {
    while (!state.stop) {
      const at = Date.now();
      const page = await readPage(tab, tag);
      if (page !== null) {
        if (page.boot !== lastBoot) {
          lastBoot = page.boot;
          state.documents.push(at);
          state.loadedAt = null;
          state.rowAt = null;
        }
        if (state.documents.length > 0 && page.complete && state.loadedAt === null) {
          state.loadedAt = at;
        }
        if (page.row && state.rowAt === null) {
          state.rowAt = at;
        }
      }
      await sleep(OBSERVE_INTERVAL_MS);
    }
  })();
  return { state, done };
}

// Resolves with the first truthy value of `check`, or with capped: true when the cap time passes first.
async function untilOrCap(check, capAt) {
  for (;;) {
    const value = check();
    if (value) {
      return { value, capped: false };
    }
    if (Date.now() >= capAt) {
      return { value: null, capped: true };
    }
    await sleep(OBSERVE_INTERVAL_MS);
  }
}

// The first catch-up that completes on any socket of the page at or after `at`.
function catchupAfter(tab, at) {
  return tab.frames.find((frame) => frame.type === 'catchup_complete' && frame.at >= at) ?? null;
}

// The catch-up bytes the page counted for its socket's replay after the release, from the socket telemetry it posts
// (client-events.js: catchup_bytes). Null until that post has gone out.
function catchupBytesAfter(tab, readyMs) {
  for (const entry of tab.requests.values()) {
    if (!entry.url.endsWith('/api/diag/client-events') || entry.postData === null) {
      continue;
    }
    const body = JSON.parse(entry.postData);
    if (body.kind === 'socket' && body.catchup_at && Date.parse(body.catchup_at) >= readyMs) {
      return { bytes: body.catchup_bytes };
    }
  }
  return null;
}

// The four parts of a return, from the replacement's server_ready to the probe row (plan 6 v4 section 4.2's split):
//   ready to attempt  from server_ready to the page's socket that carried the probe row (the carrier). The carrier can
//                     be created before server_ready, because it crosses the proxy while the replacement binds, so
//                     this part can be negative. firstAttemptAfterReadyMs names the first socket the page created at
//                     or after server_ready, whether or not it carried the row;
//   attempt to open   from the carrier's creation to its handshake response (101);
//   open to catch-up  from that socket's open to its catchup_complete, with the replay's frames by kind and bytes;
//   catch-up to row   from catchup_complete to the probe row's paint (negative when the row came in the replay).
// Each replay frame names its event index and message id, and whether the page already held it before the release: an
// event index below the page's cursor, or a message id the page had rendered. Every request the page started after
// ready rides the same link, so the HTTP requests of the window are listed beside the replay.
function returnSplit(tab, releaseAt, readyMs, tag, cursor, heldIds, rowAt) {
  const attempts = tab.sockets.filter((socket) => socket.url.includes('/ws/sessions/') && socket.at >= releaseAt);
  const probe = tab.frames.find((frame) => frame.at >= readyMs && frame.text.includes(tag));
  const carrier = probe === undefined ? undefined : attempts.find((socket) => socket.requestId === probe.requestId);
  const firstAfterReady = attempts.find((socket) => socket.at >= readyMs);
  const handshake = carrier === undefined
    ? undefined
    : tab.handshakes.find((entry) => entry.requestId === carrier.requestId && entry.status === 101);
  const openAt = handshake === undefined ? null : handshake.at;
  const catchup = handshake === undefined
    ? undefined
    : tab.frames.find((frame) => frame.requestId === carrier.requestId && frame.type === 'catchup_complete');
  const catchupAt = catchup === undefined ? null : catchup.at;
  const replay = handshake === undefined
    ? []
    : tab.frames.filter((frame) => frame.requestId === carrier.requestId && frame.at >= openAt &&
        (catchupAt === null || frame.at <= catchupAt));
  const frames = replay.map((frame) => {
    const payload = JSON.parse(frame.text);
    const eventIndex = payload.event_index ?? payload.message?.event_index ?? null;
    const messageId = payload.message?.id ?? null;
    const heldByIndex = eventIndex !== null && eventIndex < cursor;
    const heldByMessage = messageId !== null && heldIds.includes(String(messageId));
    return {
      type: frame.type,
      bytes: frame.bytes,
      eventIndex,
      messageId,
      heldByIndex,
      heldByMessage,
      atMs: frame.at - readyMs,
    };
  });
  const http = [...tab.requests.values()]
    .filter((entry) => entry.startAt >= readyMs && !entry.url.includes('/ws/') && entry.finishAt !== null)
    .map((entry) => ({
      path: new URL(entry.url).pathname,
      bytes: entry.bytes,
      startMs: entry.startAt - readyMs,
      finishMs: entry.finishAt - readyMs,
    }));
  return {
    attempts: attempts.map((socket) => ({ atMs: socket.at - readyMs, carried: socket === carrier })),
    firstAttemptAfterReadyMs: firstAfterReady === undefined ? null : firstAfterReady.at - readyMs,
    readyToAttemptMs: carrier === undefined ? null : carrier.at - readyMs,
    attemptToOpenMs: openAt === null ? null : openAt - carrier.at,
    catchupAtMs: catchupAt === null ? null : catchupAt - readyMs,
    openToCatchupMs: catchupAt === null ? null : catchupAt - openAt,
    catchupToRowMs: catchupAt === null ? null : rowAt - catchupAt,
    probeFrameMs: probe === undefined ? null : probe.at - readyMs,
    replay: frames,
    replayBytes: frames.reduce((sum, frame) => sum + frame.bytes, 0),
    heldBytes: frames
      .filter((frame) => frame.heldByIndex || frame.heldByMessage)
      .reduce((sum, frame) => sum + frame.bytes, 0),
    http,
  };
}

// The first document request of the page's reload navigation after the release (the shell's own request, which the
// worker answers from the stored shell).
function navigationAfter(tab, readyMs) {
  const request = [...tab.requests.values()].find(
    (entry) => entry.type === 'Document' && entry.startAt >= readyMs && /\?session=/.test(entry.url),
  );
  return request ?? null;
}

const CELLS = Object.freeze({
  'restart-visible': { hidden: false, reset: false, frozen: false, point: 'visible' },
  'restart-hidden-1': { hidden: true, reset: false, frozen: false, point: 'catchup' },
  'restart-hidden-2': { hidden: true, reset: false, frozen: false, point: 'navigation' },
  'restart-hidden-3': { hidden: true, reset: false, frozen: false, point: 'loaded' },
  'restart-frozen': { hidden: true, reset: false, frozen: true, point: 'loaded' },
  'restart-1006': { hidden: true, reset: true, frozen: false, point: 'loaded' },
});

// One repetition of a cell. Times are milliseconds from the release's start, except the waits, which run from the tab's
// return (the moment it is shown, or the replacement's server_ready for the visible cell).
async function restartRep(ctx, name) {
  const cell = CELLS[name];
  const { sessions } = ctx;
  return withLink(ctx, name, ['measured', 'other'], async ({ proxy, tabs, tab, hidden }) => {
    const [other] = hidden;
    await tabs.navigate(tab, `/?session=${sessions.open.id}`);
    await tab.ready();
    await tabs.show(tab);
    await tabs.waitIdle(SETTLE_TIMEOUT_MS);
    await waitFor(() => tab.evaluate('Boolean(window.__warmReport)'), SETTLE_TIMEOUT_MS, 'the offline shell warm');
    if (cell.hidden) {
      await tabs.show(other);
    }
    const tag = `restart probe ${Math.random().toString(16).slice(2, 10)}`;
    const bootAt0 = (await readPage(tab, tag)).boot;
    const version0 = await tab.evaluate('STATIC_ASSET_VERSION');
    // What the page holds before the release: its event cursor, and the ids of the messages it has rendered. The
    // replay's frames for events below the cursor, and for those messages, are the ones the page already held.
    const cursor0 = await tab.evaluate('eventCursor');
    const heldIds = await tab.evaluate(
      "[...document.querySelectorAll('#messages [data-message-id]')].map((node) => node.dataset.messageId)");
    if (cell.reset) {
      proxy.blackout({ seconds: LINK.blackoutSeconds, mode: 'reset' });
    }
    if (cell.frozen) {
      await tab.page.send('Page.setWebLifecycleState', { state: 'frozen' });
    }
    const releaseAt = Date.now();
    const capAt = releaseAt + HIDDEN_CAP_MS;
    const readyMs = await publishRelease(ctx, `${name} ${releaseAt}`);
    await ctx.api.sendMessage(sessions.open.id, tag);
    const version1 = await servedAssetVersion(ctx, sessions.open.id);
    if (version1 === version0) {
      throw new Error(`the release left the served asset version at ${version0}`);
    }
    if (cell.frozen) {
      await tab.page.send('Page.setWebLifecycleState', { state: 'active' });
    }
    const observer = observe(tab, tag, bootAt0);
    let restoreAt = readyMs;
    let capped = false;
    if (cell.point === 'catchup') {
      const { value: frame, capped: cut } = await untilOrCap(() => catchupAfter(tab, readyMs), capAt);
      capped = cut;
      await sleep(Math.max(0, (cut ? capAt : frame.at + CATCHUP_TO_SHOW_MS) - Date.now()));
    } else if (cell.point === 'navigation') {
      const { value: request, capped: cut } = await untilOrCap(() => navigationAfter(tab, readyMs), capAt);
      capped = cut;
      await sleep(Math.max(0, (cut ? capAt : request.startAt + NAVIGATION_TO_SHOW_MS) - Date.now()));
    } else if (cell.point === 'loaded') {
      const { capped: cut } = await untilOrCap(() => observer.state.loadedAt, capAt);
      capped = cut;
    }
    if (cell.hidden) {
      restoreAt = Date.now();
      await tabs.show(tab);
    }
    const rowAt = await waitFor(
      () => (cell.point === 'navigation' && !capped
        ? (observer.state.documents.length > 0 ? observer.state.rowAt : null)
        : observer.state.rowAt),
      SHOW_TIMEOUT_MS,
      'the probe row on screen',
    );
    observer.state.stop = true;
    await observer.done;
    // The gated catch-up is the page's first catch-up after its last document started, or after the replacement's
    // server_ready when the page never reloaded: a reloaded page is gated on its own catch-up, not the old document's.
    // The page posts its catch-up bytes after the replay, so the frames are read once that post has arrived.
    const documents = observer.state.documents;
    const since = Math.max(readyMs, documents.length > 0 ? documents[documents.length - 1] : readyMs);
    const caughtUp = await poll(
      async () => catchupAfter(tab, since),
      SHOW_TIMEOUT_MS,
      'the catch-up that brings the page current',
    );
    const telemetry = await poll(async () => catchupBytesAfter(tab, readyMs), SHOW_TIMEOUT_MS, 'the catch-up telemetry');
    return {
      cell: name,
      serverReadyMs: readyMs - releaseAt,
      restoreMs: restoreAt - releaseAt,
      rowMs: rowAt - releaseAt,
      // The gated quantity (plan 6 v4 section 4.2): the wait from the return to the caught-up instant. The return is
      // the show, or the replacement's server_ready for a tab visible throughout. A catch-up before the return leaves
      // no wait.
      waitMs: Math.max(0, caughtUp.at - restoreAt),
      // Report only: the wait from the return to the probe row's paint.
      probeWaitMs: Math.max(0, rowAt - restoreAt),
      documentsMs: observer.state.documents.map((at) => at - releaseAt),
      capped,
      // The page's own count of its replay (plan 6 v4 section 4.2). A hidden repetition that counts 0 is invalid: rerun.
      catchupBytes: telemetry.bytes,
      blackoutMs: cell.reset ? LINK.blackoutSeconds * 1000 : null,
      assetVersions: [version0, version1],
      cursor: cursor0,
      split: returnSplit(tab, releaseAt, readyMs, tag, cursor0, heldIds, rowAt),
    };
  }, BOOT_SCRIPT);
}

// The bench's scenario functions for the cells, keyed by cell name.
export const RESTART_SCENARIOS = Object.freeze(
  Object.fromEntries(Object.keys(CELLS).map((name) => [name, (ctx) => restartRep(ctx, name)])),
);

// Stops the replacement server the bench started, if any, so the trial finds no process of its own left running.
export async function stopRestartServer() {
  if (serving === null || serving.child === null) {
    return;
  }
  const pid = serving.child.pid;
  if (alive(pid)) {
    process.kill(pid, 'SIGTERM');
    await poll(async () => !alive(pid), SERVER_STOP_TIMEOUT_MS, `server ${pid} to stop`);
  }
  serving = null;
}
