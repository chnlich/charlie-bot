// The For-you scenario: a switch and a first open whose target session's first screen holds no For-you row, so the
// page must reach the session's newest For-you row from behind the first screen — from the bootstrap's
// `older_for_you` rows, or, before that field exists, by walking older-event pages. A sparse cell switches to a target
// whose only For-you row is its marker. A failed-page cell injects one failed older page into a switch. The cells and
// their caps are the acceptance table of the For-you plan (4.2).
//
// The targets are seeded once per side (`seedForYouTargets`): the large target also carries a bootstrap sized to the
// live view's, and both targets end in a tail of scheduled-trigger turns. A trigger turn carries no For-you row (its
// message is a trigger, its reply the backend's assistant text, and its turn-end checks show agent messages), so the
// newest 40 messages hold none and the newest For-you row — the tail's last human turn — sits a few hundred event
// lines back, inside the bootstrap's 2,000-line For-you scan. The seed counts rows with the page's own For-you rule
// (`paintsInForYou`) and asserts the marker is among the newest rows the first paint carries, so a stub or a
// scan-bound change fails the seed instead of measuring a different defect.
//
// Every repetition runs in a fresh browser profile behind a fresh proxy. The switch cells run under the background
// load of scenarios 1 to 3, as the diagnostic run they replace did; the first-open cells run on another fresh
// profile, whose browser has opened a chat page before, but never the target (the open scenario's case B).

import { VIEW } from './profile.mjs';
import { paintsInForYou } from './newest-row.mjs';
import { PAGE_REPORT_BINDING, pollFor, sleep } from './tabs.mjs';

import { WARM_REPORT_INIT, deliverReplyOnline, reportOnWindow, warmFailure } from './open.mjs';
import { floorMs, withBackground, withLink } from './scenarios.mjs';
import { StepCapped } from './segments.mjs';

export const FOR_YOU_SCENARIO = 'foryou';

// The tail's trigger turns. Twenty-two turn the first screen's 40 messages over whatever a turn's own message count
// is; the seed asserts the shape from the bootstrap, so a stub change that undoes this fails loudly. The tail's
// event span must stay inside the bootstrap's 2,000-line For-you scan, which the seed also asserts.
const TAIL_TURNS = 23;
// The delay each seeded trigger waits before it fires, and the bound on one seeded turn's run.
const TAIL_TRIGGER_DELAY_S = 1;
const TAIL_TURN_TIMEOUT_MS = 120000;
// The most event lines the tail may add to a session: the scan reads 2,000 lines back, and the margin keeps the
// marker inside it however the first screen's own span falls.
const TAIL_MAX_EVENT_LINES = 1500;
// The most turns the large target's sizing takes, as the bench's own sized sessions allow.
const SIZING_MAX_TURNS = 30;

// The cell waits after the switch's first screen before its next cell starts, so the idle loads the screen starts
// (usage, explain status) run out of the switch they belong to. The failed-page cell waits twice as long, so the
// retry's own request is in its count.
const SWITCH_SETTLE_MS = 5000;
const FAILED_PAGE_SETTLE_MS = 10000;
// The first-open cell's deadline after `ready()` for the first page's offline-shell warm and on-demand file store to
// report. On the 100 kbit/s link, catch-up reaches `charliebot:page-current` ~14-16 s after navigation (~10-12 s after
// `ready()`), the worker script and quiet timer take ~2-3 s, the ~97 KB shell takes ~9 s, and the on-demand files
// follow (~5 s), about 25-30 s in all. 60,000 ms leaves ~2x margin for link spikes while staying well under
// `STEP_CAP_MS` (600,000 ms), so a missing report records cell failure instead of tripping the step cap.
const OPEN_WARM_TIMEOUT_MS = 60000;
// The wait for the injected failure to reach the page, and for the page's own first-row phase after the marker.
const FAIL_OBSERVE_TIMEOUT_MS = 30000;
const PHASE_OBSERVE_TIMEOUT_MS = 10000;

const SHOW_TIMEOUT_MS = 180000;
// The newest For-you rows a bootstrap carries before its first screen (store/for_you.rs FOR_YOU_MAX_ROWS). A marker
// within them is painted with the view's first rows.
const FOR_YOU_HEAD_ROWS = 10;
// The event lines one marker walk may read back: the seed keeps the tail under TAIL_MAX_EVENT_LINES, so this is the
// 2,000-line scan bound's page count with margin.
const MARKER_WALK_MAX_PAGES = 60;

function randomTag() {
  return Math.random().toString(16).slice(2, 10);
}

// The number of the target's older-page requests in [from, to]: the requests for the target's own event pages.
function olderPageRequests(tab, sessionId, from, to) {
  const counts = tab.requestPathCounts(new RegExp(`/api/sessions/${sessionId}/events`), from, to);
  return Object.values(counts).reduce((total, count) => total + count, 0);
}

async function bootstrapOf(api, sessionId) {
  return api.json('GET', `/api/sessions/${sessionId}/bootstrap`);
}

// Sizes `session` with human turns until its bootstrap content reaches the live view's bytes, as the bench's own
// sized sessions are. Returns the turns it took and the untrimmed bootstrap bytes it reached.
async function sizeTarget(api, session) {
  for (let turn = 1; ; turn++) {
    await api.sendMessage(session.id, `pn-foryou size ${session.tag} ${turn}`);
    await api.waitForIdle(session.id, TAIL_TURN_TIMEOUT_MS);
    const untrimmed = (await api.bootstrapWireBytes(session.id)).untrimmedBytes;
    if (untrimmed >= VIEW.bootstrapUntrimmedWireBytes) {
      return { turns: turn, untrimmedBytes: untrimmed };
    }
    if (turn === SIZING_MAX_TURNS) {
      throw new Error(
        `the For-you target ${session.id} reached ${untrimmed} untrimmed bootstrap bytes in ${turn} turns, ` +
          `short of ${VIEW.bootstrapUntrimmedWireBytes}`,
      );
    }
  }
}

// A tail turn leaves no wake of its own: no Waiting item, no child, and the next tail trigger is scheduled only after
// it. The turn-end `pending-wake` check would then block it, continue it once, and post the no-wake notice, which a
// trigger turn holds no user message to attach to: the runner posts it as an agent message, and an agent message is an
// input, so the notice starts a new turn that posts the same notice again and the session never idles. The keeper
// trigger is the tail's wake: it is pending while every tail turn runs, and the seed cancels it after the last one, so
// no turn wakes on it. The one-hour delay stays inside the four hours the check asks a wake trigger to fire within.
const TAIL_KEEPER_DELAY_S = 3600;

// Adds the tail's trigger turns. Each fires after its one-second delay and runs the stub backend; the wait is the
// same idle poll the bench's seeding uses.
async function addTriggerTail(api, session) {
  const keeper = await api.scheduleTrigger(session.id, TAIL_KEEPER_DELAY_S, `pn-foryou keeper ${session.tag}`);
  try {
    for (let turn = 0; turn < TAIL_TURNS; turn++) {
      await api.scheduleTrigger(session.id, TAIL_TRIGGER_DELAY_S, `pn-foryou tail ${session.tag} ${turn}`);
      await api.waitForIdle(session.id, TAIL_TURN_TIMEOUT_MS);
    }
  } finally {
    await api.cancelTrigger(session.id, keeper);
  }
}

// The painted rows the page's For-you rule shows before the first screen, counted from `oldest` back until `marker`
// is found: `newer` is how many the view paints above the marker. The walk reads the event pages the page's own
// older-page request reads, so a row the page cannot reach is not counted.
async function forYouRowsAbove(api, sessionId, oldest, marker) {
  let cursor = oldest;
  let newer = 0;
  for (let page = 0; page < MARKER_WALK_MAX_PAGES; page++) {
    const answer = await api.json('GET', `/api/sessions/${sessionId}/events?before=${cursor}&limit=40`);
    const messages = answer.messages ?? [];
    for (let index = messages.length - 1; index >= 0; index--) {
      const message = messages[index];
      if (message.role === 'user' && message.content === marker) {
        return {found: true, newer};
      }
      if (paintsInForYou(message)) newer++;
    }
    if (!answer.has_more) {
      return {found: false, newer};
    }
    cursor = answer.next_before;
  }
  return {found: false, newer};
}

// Seeds one For-you target: its marker turn (the newest human turn, the newest For-you row), then the trigger tail.
// Asserts the target's shape from its bootstrap, so a stub change or a scan-bound change fails the seed.
async function seedTarget(api, target) {
  // The marker names its target: a cell times its marker's first paint, so a page that still shows another target's
  // marker would time nothing.
  const marker = `pn-foryou marker ${target.tag} ${target.role}`;
  target.marker = marker;
  await api.sendMessage(target.id, marker);
  await api.waitForIdle(target.id, TAIL_TURN_TIMEOUT_MS);
  const markerBootstrap = await bootstrapOf(api, target.id);
  await addTriggerTail(api, target);
  const bootstrap = await bootstrapOf(api, target.id);
  const forYouRows = bootstrap.messages.filter(paintsInForYou);
  if (forYouRows.length > 0) {
    throw new Error(
      `the For-you target ${target.id}'s first screen holds ${forYouRows.length} For-you rows, ` +
        `so its switch has no older row to reach`,
    );
  }
  // The marker is the newest For-you row before the first screen, so the view paints it with its first rows: the
  // bootstrap's head carries the newest ten painted rows, and the marker must be one of them.
  const above = await forYouRowsAbove(api, target.id, bootstrap.oldest_message_ordinal, marker);
  if (!above.found) {
    throw new Error(
      `the For-you target ${target.id}'s marker is not among the painted rows behind its first screen`,
    );
  }
  if (above.newer >= FOR_YOU_HEAD_ROWS) {
    throw new Error(
      `the For-you target ${target.id}'s marker sits behind ${above.newer} painted rows, past the newest ` +
        `${FOR_YOU_HEAD_ROWS} a bootstrap carries`,
    );
  }
  // The marker turn's own events stand between the count its turn reported and the tail's first event, so the
  // tail's span bounds the marker's distance to the first screen from above.
  const tailLines = bootstrap.event_count - markerBootstrap.event_count;
  if (tailLines > TAIL_MAX_EVENT_LINES) {
    throw new Error(
      `the For-you target ${target.id}'s trigger tail spans ${tailLines} event lines, past the ` +
        `${TAIL_MAX_EVENT_LINES} a marker must stay inside to reach the bootstrap's 2,000-line scan`,
    );
  }
}

// Seeds the two For-you targets once per side, after the bench's own sessions exist. The large target is sized
// first, then both targets are seeded. Returns the two targets with their marker text.
export async function seedForYouTargets(api, sessions) {
  const tag = randomTag();
  const large = { ...sessions.foryouLarge, tag, role: 'large' };
  const typical = { ...sessions.foryouTypical, tag, role: 'typical' };
  large.sizing = await sizeTarget(api, large);
  await seedTarget(api, large);
  await seedTarget(api, typical);
  return { large, typical };
}

// Creates and seeds the sparse target: a session of its own whose only For-you row is its marker, the 8dcbbccc shape
// (a few For-you rows in a long log). The bench creates it after it counts its view, since the view counts the bench's
// sessions and this one is not among them.
export async function seedSparseTarget(api, backendId) {
  const tag = randomTag();
  const session = await api.createSession(`pn-${tag}-sparse`, backendId);
  const sparse = { ...session, tag, role: 'sparse' };
  await seedTarget(api, sparse);
  return sparse;
}

// The init script of a first-open tab. It runs in every document of the tab and reports when the target's marker
// first shows in the chat, with its document's address, so the driver reads the report of the target's document and
// not of the page the browser opened first.
export function forYouInitScript(marker) {
  return `(() => {
    if (window !== window.top) return;
    const report = (fields) => window.${PAGE_REPORT_BINDING}(
      JSON.stringify({ kind: 'foryou', timeOrigin: performance.timeOrigin, href: location.href, ...fields }));
    let seen = false;
    const scan = () => {
      if (!seen && (document.getElementById('messages')?.textContent || '').includes(${JSON.stringify(marker)})) {
        seen = true;
        report({ at: Date.now() });
      }
    };
    new MutationObserver(scan).observe(document, { childList: true, subtree: true, characterData: true });
    document.addEventListener('DOMContentLoaded', () => report({ domContentLoadedMs: performance.now() }));
  })();`;
}

// Watches the target's own first-row phase (`first_for_you_row`, a plan-5 page's switch record). A page before the
// plan fires no such phase, and the cell falls back to the marker's paint. The phase's detail carries the page's own
// elapsed time, which the cell records beside the watch's wall clock.
async function watchFirstRowPhase(tab, name, sessionId) {
  await tab.evaluate(`(() => {
    window.__pnWatch = window.__pnWatch || {};
    const entry = { at: null, elapsedMs: null };
    window.__pnWatch[${JSON.stringify(name)}] = entry;
    document.addEventListener('charliebot:session-switch-phase', (ev) => {
      if (entry.at === null && ev.detail && ev.detail.phase === 'first_for_you_row'
          && ev.detail.to_session === ${JSON.stringify(sessionId)}) {
        entry.at = Date.now();
        entry.elapsedMs = ev.detail.elapsed_ms;
      }
    });
  })()`);
}

async function firstRowPhase(tab, name, timeoutMs) {
  return pollFor(async () => {
    const entry = await tab.evaluate(`window.__pnWatch?.[${JSON.stringify(name)}] ?? null`);
    return entry && entry.at !== null ? entry : null;
  }, timeoutMs);
}

// Asks the page's own scroll handler for an older page until its failure hook records the request. Repeated scrolls
// cover a page that was already filling when the first For-you row appeared: that flight may finish before the hook
// becomes eligible, and the next scroll starts the measured failure.
export async function triggerOlderPageUntilFailure(tab, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const failedAt = await tab.evaluate('window.__pnFail?.at ?? null');
    if (failedAt !== null) return failedAt;
    await tab.evaluate(`(() => {
      const container = document.getElementById('messages');
      container.scrollTop = 0;
      container.dispatchEvent(new Event('scroll'));
    })()`);
    await sleep(200);
  }
  return null;
}

// Whether the page's chat shows `marker` now. A cell times the marker's first paint after its click, so a marker the
// page already shows before the click would time nothing; the cell fails instead of reporting a value of zero.
async function assertMarkerOffScreen(tab, marker, cell) {
  const shown = await tab.evaluate(
    `(document.getElementById('messages')?.textContent || '').includes(${JSON.stringify(marker)})`,
  );
  if (shown) {
    throw new Error(`${cell}: the page shows ${marker} before its click`);
  }
}

// The switch cell: the page shows another session, a person puts the target's row on screen and clicks it, and the
// cell times the click to the first For-you row (the seed's marker) and counts the target's older-page requests
// before that row. The count reads the page's own first-row phase when the page reports one, so a chain request the
// page starts at the same render never counts as a page before the row. A plan-5 page reaches the marker from the
// bootstrap's own `older_for_you` rows; a page before it walks the older-event pages.
async function switchCell(ctx, tab, target, name) {
  const { api } = ctx;
  await tab.showRow(target);
  await assertMarkerOffScreen(tab, target.marker, name);
  await tab.watchChat(name, target.marker);
  const phaseName = `${name}-phase`;
  await watchFirstRowPhase(tab, phaseName, target.id);
  const clickedAt = await tab.clickSession(target.id);
  const shown = await tab.shownBy(name, clickedAt, SHOW_TIMEOUT_MS);
  const phase = await firstRowPhase(tab, phaseName, PHASE_OBSERVE_TIMEOUT_MS);
  const firstRowAt = (phase && phase.at) || shown.at;
  const pageBytes = tab.bytesOf(new RegExp(`/api/sessions/${target.id}/bootstrap$`), clickedAt, Infinity);
  const bootstrapBytes = pageBytes > 0 ? pageBytes : (await api.bootstrapWireBytes(target.id)).wireBytes;
  await sleep(SWITCH_SETTLE_MS);
  return {
    firstRowMs: shown.at - clickedAt,
    capped: shown.capped,
    // The page's own elapsed time, when the page reports the phase (null on a page before the plan).
    phaseFirstRowMs: phase ? phase.elapsedMs : null,
    pages: olderPageRequests(tab, target.id, clickedAt, firstRowAt),
    bootstrapBytes,
    bootstrapFloorMs: floorMs(bootstrapBytes),
  };
}

// The failed-page cell: the same switch, with the page's first older-page request after the marker appears failing with
// a 500. The failure follows the click-to-first-row measurement, so it cannot mask that row on main. The readouts are
// the first-row time, the failure's arrival, and whether the page retried the cursor on its backoff.
async function failedPageCell(ctx, tab, target, name) {
  await tab.showRow(target);
  await assertMarkerOffScreen(tab, target.marker, name);
  await tab.watchChat(name, target.marker);
  await tab.evaluate(`(() => {
    const original = window.fetch;
    window.__pnFail = { at: null, url: null };
    window.fetch = (input, init) => {
      const url = typeof input === 'string' ? input : (input && input.url) || '';
      const markerShown = (document.getElementById('messages')?.textContent || '')
        .includes(${JSON.stringify(target.marker)});
      if (window.__pnFail.at === null && markerShown && url.includes('/api/sessions/${target.id}/events')) {
        window.__pnFail = { at: Date.now(), url };
        return Promise.resolve(new Response('{"error":"injected"}', { status: 500 }));
      }
      return original.call(window, input, init);
    };
  })()`);
  const clickedAt = await tab.clickSession(target.id);
  const shown = await tab.shownBy(name, clickedAt, SHOW_TIMEOUT_MS);
  const failedAt = await triggerOlderPageUntilFailure(tab, FAIL_OBSERVE_TIMEOUT_MS);
  if (failedAt === null) {
    throw new Error(`${name}: no older-page request failed after the first For-you row appeared`);
  }
  await sleep(FAILED_PAGE_SETTLE_MS);
  const pageRequestsAfterFailure = olderPageRequests(tab, target.id, failedAt, Date.now());
  return {
    firstRowMs: shown.at - clickedAt,
    capped: shown.capped,
    failureMs: failedAt === null ? null : failedAt - clickedAt,
    // The requests after the failure: the failed one, plus the retry a plan-5 page makes on its backoff.
    pageRequestsAfterFailure,
  };
}

// Whether the target document came from the stored shell ('stored-shell') or from the network ('network'), or null
// when the navigation never painted a row and no network fetch ran. A service-worker navigation leaves
// `documentBytes` zero on the tab even when the worker fell back to `fetch(request)` on the network, so the worker's
// own trace from `navigatedAt` (ignoring any navigation preload 204) is checked alongside `documentBytes`.
export function openDocumentPath({ documentBytes, workerRequests, documentPattern, report }) {
  if (documentBytes > 0) {
    return 'network';
  }
  if (workerRequests.some((req) => req.startMs >= 0 && documentPattern.test(req.url) && req.status !== 204)) {
    return 'network';
  }
  return report === null ? null : 'stored-shell';
}

// The first-open cell: a fresh browser opens another chat page, waits for the page's offline-shell warm and
// on-demand file store to report, then opens the target — a page load the plan's Trade-off 3 sets no time target for.
// When the warm never reports within `warmTimeoutMs` or reports a failure, the cell records failure without
// navigating. The readouts are the navigation-to-first-row time, the document's bytes, the target's older-page
// requests before the first row, and whether the document came from the stored shell or the network.
export async function measureOpenCell(tabs, ctx, target, name, warmTimeoutMs) {
  const documentPattern = new RegExp(`\\?session=${target.id}$`);
  const measured = await tabs.open(name, `${WARM_REPORT_INIT}\n${forYouInitScript(target.marker)}`);
  await measured.page.send('Runtime.addBinding', { name: PAGE_REPORT_BINDING });
  await tabs.navigate(measured, `/?session=${ctx.sessions.open.id}`);
  await measured.ready();
  const warmReport = await reportOnWindow(measured, '__warmReport', warmTimeoutMs);
  if (warmReport === null) {
    return { failed: true, reason: 'warm not reported' };
  }
  if (warmReport.stored !== true) {
    return {
      failed: true,
      reason: warmFailure({ side: 'after', caseName: name, report: warmReport, filesStored: null }),
    };
  }
  const filesStored = await reportOnWindow(measured, '__filesStored', warmTimeoutMs);
  const failure = warmFailure({ side: 'after', caseName: name, report: warmReport, filesStored });
  if (failure !== null) {
    return { failed: true, reason: failure };
  }
  const navigatedAt = Date.now();
  await tabs.navigate(measured, `/?session=${target.id}`);
  const report = await firstForYouReport(measured, documentPattern);
  const documentBytes = measured.bytesOf(documentPattern, navigatedAt, Infinity);
  const workerRequests = tabs.workerTrace(navigatedAt);
  return {
    firstRowMs: report === null ? SHOW_TIMEOUT_MS : report.at - navigatedAt,
    capped: report === null,
    documentBytes,
    pages: report === null ? null : olderPageRequests(measured, target.id, navigatedAt, report.at),
    path: openDocumentPath({ documentBytes, workerRequests, documentPattern, report }),
  };
}

async function openCell(ctx, target, name) {
  return withLink(ctx, name, [], ({ tabs }) => measureOpenCell(tabs, ctx, target, name, OPEN_WARM_TIMEOUT_MS));
}

// The newest report of a document whose address matches `documentPattern` and whose marker has shown, or null past
// SHOW_TIMEOUT_MS. The tab's reports are already parsed, one object per binding call.
function firstForYouReport(tab, documentPattern) {
  return pollFor(() => {
    const reports = tab.reports.filter(
      (entry) => entry.kind === 'foryou' && entry.at && documentPattern.test(entry.href),
    );
    return reports[reports.length - 1] ?? null;
  }, SHOW_TIMEOUT_MS);
}

// The offline reopen: a fresh profile delivers one reply to the large target online, as the open scenario does. The
// reply's stored-copy refresh stores the target's first screen and its sidebar copies. The same profile then reopens
// the target with the network off, so the stored shell and the stored copies are all the page has. The reopen's time is
// its navigation to the first For-you row; its document comes from the stored shell, so its bytes are zero.
async function offlineReopenCell(ctx, target, name, repetition) {
  const documentPattern = new RegExp(`\\?session=${target.id}$`);
  return withLink(ctx, name, [], async ({ tabs }) => {
    // The fresh profile's warm stores the target's first-screen copy; the reply's refresh re-stores only a copy that
    // already exists, so the reply waits for that warm.
    await deliverReplyOnline(tabs, target, repetition, {waitForStoredCopy: true});

    const measured = await tabs.open(name, forYouInitScript(target.marker));
    await measured.page.send('Runtime.addBinding', { name: PAGE_REPORT_BINDING });
    await tabs.show(measured);
    await measured.page.send('Network.emulateNetworkConditions', {
      offline: true, latency: 0, downloadThroughput: -1, uploadThroughput: -1,
    });
    const navigatedAt = Date.now();
    await tabs.navigate(measured, `/?session=${target.id}`);
    const report = await firstForYouReport(measured, documentPattern);
    return {
      firstRowMs: report === null ? SHOW_TIMEOUT_MS : report.at - navigatedAt,
      capped: report === null,
      documentBytes: measured.bytesOf(documentPattern, navigatedAt, Infinity),
    };
  });
}

// The repetitions each For-you cell takes (plan 5 v4 section 4.2, the after column): five for the two large cells and
// the typical first open, two for the rest. A repetition past a cell's count does not run it, and its record holds null
// for it, so the report counts each cell's repetitions as the plan does.
export const FOR_YOU_CELL_REPS = Object.freeze({
  switchLarge: 5,
  switchTypical: 2,
  switchSparse: 2,
  failedPage: 2,
  openLarge: 5,
  openTypical: 5,
  offline: 2,
});

export function cellsDue(repetition) {
  return Object.fromEntries(Object.entries(FOR_YOU_CELL_REPS).map(([cell, count]) => [cell, repetition <= count]));
}

// The For-you scenario's repetitions. Each cell is one step of the bench (ctx.unit), so the lock is taken and given up
// between cells. A switch cell and the failed-page cell run in their own background window (scenarios 1 to 3's load,
// as the diagnostic run they replace did); the first-open and offline cells run without it. A cell that passes its
// step cap is named in `cappedSteps`, which fails the side at its end, as a capped repetition does, and yields null.
export async function forYouScenario(ctx, repetitions, results, cappedSteps) {
  const { foryou } = ctx.sessions;
  for (let repetition = 1; repetition <= repetitions; repetition++) {
    const due = cellsDue(repetition);
    const from = ctx.trials.mark();
    const cell = (name, body) => runCell(ctx, `${name} ${repetition}`, body, cappedSteps);
    const rep = { switch: {}, failedPage: null, open: {}, offline: null };
    rep.switch.large = due.switchLarge
      ? await cell('foryou switch large', () => loadedSwitch(ctx, repetition, foryou.large, 'fy-switch-large'))
      : null;
    rep.switch.typical = due.switchTypical
      ? await cell('foryou switch typical', () => loadedSwitch(ctx, repetition, foryou.typical, 'fy-switch-typical'))
      : null;
    rep.switch.sparse = due.switchSparse
      ? await cell('foryou switch sparse', () => loadedSwitch(ctx, repetition, foryou.sparse, 'fy-switch-sparse'))
      : null;
    rep.failedPage = due.failedPage
      ? await cell('foryou failed page', () => loadedFailedPage(ctx, repetition, foryou.large))
      : null;
    rep.open.large = due.openLarge
      ? await cell('foryou open large', () => openCell(ctx, foryou.large, `fy-open-large-${repetition}`))
      : null;
    rep.open.typical = due.openTypical
      ? await cell('foryou open typical', () => openCell(ctx, foryou.typical, `fy-open-typical-${repetition}`))
      : null;
    // The offline reopen waits for the copies-refresh report of the offline client, which only the after side emits
    // (the main side has no such client), so the before side records no offline cell (null).
    rep.offline = ctx.side === 'after' && due.offline
      ? await cell('foryou offline reopen',
        () => offlineReopenCell(ctx, foryou.large, `fy-offline-${repetition}`, repetition))
      : null;
    results.push({
      ...rep,
      startedAt: new Date(from).toISOString(),
      finishedAt: new Date().toISOString(),
      otherTrials: ctx.trials.since(from),
    });
  }
}

// A switch cell in its own background window.
function loadedSwitch(ctx, repetition, target, name) {
  return withBackground(ctx, { label: `foryou-${repetition}`, filter: null, renamed: null, churn: null },
    ({ tab }) => switchCell(ctx, tab, target, name));
}

// The failed-page cell in its own background window.
function loadedFailedPage(ctx, repetition, target) {
  return withBackground(ctx, { label: `foryou-${repetition}`, filter: null, renamed: null, churn: null },
    ({ tab }) => failedPageCell(ctx, tab, target, 'fy-failed-page'));
}

// Runs one cell as a step of the bench.
async function runCell(ctx, name, body, cappedSteps) {
  try {
    return await ctx.unit('cell', name, body);
  } catch (caught) {
    if (!(caught instanceof StepCapped)) {
      throw caught;
    }
    cappedSteps.push(caught.message);
    return null;
  }
}
