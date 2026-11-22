// The For-you scenario: a switch and a first open whose target session's first screen holds no For-you row, so the
// page must reach the session's newest For-you row from behind the first screen — from the bootstrap's
// `older_for_you` rows, or, before that field exists, by walking older-event pages. A third cell injects one failed
// older page into a switch. The cells and their caps are the acceptance table of the For-you plan (4.2).
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

import { deliverReplyOnline } from './open.mjs';
import { floorMs, withBackground, withLink } from './scenarios.mjs';

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
// The first-open cell's wait after the first page goes quiet, so the second navigation starts from a page that no
// longer loads.
const OPEN_SETTLE_MS = 10000;
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

// Adds the tail's trigger turns. Each fires after its one-second delay and runs the stub backend; the wait is the
// same idle poll the bench's seeding uses.
async function addTriggerTail(api, session) {
  for (let turn = 0; turn < TAIL_TURNS; turn++) {
    await api.scheduleTrigger(session.id, TAIL_TRIGGER_DELAY_S, `pn-foryou tail ${session.tag} ${turn}`);
    await api.waitForIdle(session.id, TAIL_TURN_TIMEOUT_MS);
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

// Seeds the two For-you targets once per side, after the bench's own sessions exist. The large target is sized
// first, both targets get the marker turn (the newest human turn, the newest For-you row), then the trigger tail.
// Returns the two targets with their marker text, and asserts each one's shape from its bootstrap.
export async function seedForYouTargets(api, sessions) {
  const tag = randomTag();
  const large = { ...sessions.foryouLarge, tag, role: 'large' };
  const typical = { ...sessions.foryouTypical, tag, role: 'typical' };
  for (const target of [large, typical]) {
    if (target === large) {
      target.sizing = await sizeTarget(api, target);
    }
    // The marker names its target: a cell times its marker's first paint, so a page that still shows another target's
    // marker would time nothing.
    const marker = `pn-foryou marker ${tag} ${target.role}`;
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
  return { large, typical };
}

// The init script of a first-open tab. It runs in every document of the tab and reports when the target's marker
// first shows in the chat, with its document's address, so the driver reads the report of the target's document and
// not of the page the browser opened first.
function forYouInitScript(marker) {
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

// The failed-page cell: the same switch, with the page's own fetch made to fail the target's first older-page
// request with a 500. The readouts are the click-to-first-row time, the failure's arrival, and whether the page
// retried the cursor (a plan-5 page retries the same cursor on its backoff; a page before it stops).
async function failedPageCell(ctx, tab, target, name) {
  await tab.showRow(target);
  await assertMarkerOffScreen(tab, target.marker, name);
  await tab.watchChat(name, target.marker);
  await tab.evaluate(`(() => {
    const original = window.fetch;
    window.__pnFail = { at: null, url: null };
    window.fetch = (input, init) => {
      const url = typeof input === 'string' ? input : (input && input.url) || '';
      if (window.__pnFail.at === null && url.includes('/api/sessions/${target.id}/events')) {
        window.__pnFail = { at: Date.now(), url };
        return Promise.resolve(new Response('{"error":"injected"}', { status: 500 }));
      }
      return original.call(window, input, init);
    };
  })()`);
  const clickedAt = await tab.clickSession(target.id);
  const shown = await tab.shownBy(name, clickedAt, SHOW_TIMEOUT_MS);
  const failedAt = await pollFor(() => tab.evaluate('window.__pnFail?.at ?? null'), FAIL_OBSERVE_TIMEOUT_MS);
  await sleep(FAILED_PAGE_SETTLE_MS);
  const pageRequestsAfterFailure = olderPageRequests(tab, target.id, failedAt ?? clickedAt, Date.now());
  return {
    firstRowMs: shown.at - clickedAt,
    capped: shown.capped,
    failureMs: failedAt === null ? null : failedAt - clickedAt,
    // The requests after the failure: the failed one, plus the retry a plan-5 page makes on its backoff.
    pageRequestsAfterFailure,
  };
}

// The first-open cell: a fresh browser opens another chat page, goes quiet, then opens the target — a page load the
// plan's Trade-off 3 sets no time target for. The readouts are the navigation-to-first-row time, the document's
// bytes, and the target's older-page requests before the first row.
async function openCell(ctx, target, name) {
  const documentPattern = new RegExp(`\\?session=${target.id}$`);
  return withLink(ctx, name, [], async ({ tabs }) => {
    const measured = await tabs.open(name, forYouInitScript(target.marker));
    await measured.page.send('Runtime.addBinding', { name: PAGE_REPORT_BINDING });
    await tabs.navigate(measured, `/?session=${ctx.sessions.open.id}`);
    await measured.ready();
    await sleep(OPEN_SETTLE_MS);
    const navigatedAt = Date.now();
    await tabs.navigate(measured, `/?session=${target.id}`);
    const report = await firstForYouReport(measured, documentPattern);
    return {
      firstRowMs: report === null ? SHOW_TIMEOUT_MS : report.at - navigatedAt,
      capped: report === null,
      documentBytes: measured.bytesOf(documentPattern, navigatedAt, Infinity),
      pages: report === null ? null : olderPageRequests(measured, target.id, navigatedAt, report.at),
    };
  });
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
    await deliverReplyOnline(tabs, target, repetition);

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

// One repetition: the switch cells and the failed-page cell in one background window, then the first-open cells on
// a fresh profile.
export async function forYouRep(ctx, repetition) {
  const { foryou } = ctx.sessions;
  const cells = await withBackground(
    ctx, { label: `foryou-${repetition}`, filter: null, renamed: null, churn: null },
    async ({ tab }) => {
      const large = await switchCell(ctx, tab, foryou.large, 'fy-switch-large');
      const typical = await switchCell(ctx, tab, foryou.typical, 'fy-switch-typical');
      const failedPage = await failedPageCell(ctx, tab, foryou.large, 'fy-failed-page');
      return { large, typical, failedPage };
    },
  );
  const openLarge = await openCell(ctx, foryou.large, `fy-open-large-${repetition}`);
  const openTypical = await openCell(ctx, foryou.typical, `fy-open-typical-${repetition}`);
  // The offline reopen waits for the copies-refresh report of the offline client, which only the after side emits (the
  // main side has no such client), so the before side records no offline cell (null).
  const offline = ctx.side === 'after'
    ? await offlineReopenCell(ctx, foryou.large, `fy-offline-${repetition}`, repetition)
    : null;
  return {
    switch: { large: cells.large, typical: cells.typical },
    failedPage: cells.failedPage,
    open: { large: openLarge, typical: openTypical },
    offline,
  };
}
