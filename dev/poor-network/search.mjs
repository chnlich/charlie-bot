// The sidebar search view: the sidebar's search box, typed at the bench link, for each query of SEARCH.queries, in two
// variants, SEARCH.repetitions times each (profile.mjs SEARCH). A repetition of either variant runs in its own browser
// profile behind its own proxy (withLink) and types the queries in order.
//
// Per typed query the view records:
// - e2e_ms: the answer's paint minus the input event, both on the page's clock;
// - body_bytes: the search response's encodedBodySize, the body as it crossed the link compressed;
// - wire_bytes: the response's transferSize, headers included;
// - overlap: the other requests in flight between the search request's start and its response's end, with their
//   transferSize, the status polls counted apart.
// The broadcast variant also runs a window from the query's first paint. For the window the bench renames its background
// session at SEARCH.broadcast's rate, and the server broadcasts each rename as task_tree_changed. Each broadcast makes the
// page refetch the open query, and an answer whose generation a newer refetch has overtaken is discarded unpainted. The
// window's accounting runs from the first paint until the page is idle. It counts the refetches the broadcasts caused,
// the answers that ended there, the answers that painted (a repaint of the list's last paint is not one), and the
// discards: the answers that ended and did not paint.

import { SEARCH } from './profile.mjs';
import { percentile } from './report.mjs';
import { withLink } from './scenarios.mjs';
import { sleep, waitFor } from './tabs.mjs';

const VARIANTS = Object.freeze(['idle', 'broadcast']);
const SEARCH_PATH = '/api/sessions/search';
const STATUS_PATH = '/api/sessions/status';
// The wait for a typed query to paint; the same bound browser_search.js gives its waits.
const PAINT_TIMEOUT_MS = 180000;
const SETTLE_TIMEOUT_MS = 300000;
// The broadcast window's answers drain after its end: 19 answers of about 117 KB share the link's 12.5 KB/s, so they take
// about three minutes to end, and the wait for them has its own bound.
const DRAIN_TIMEOUT_MS = 600000;

// Runs in each document before the page's own scripts. The resource entries go to an observer's log, not read from the
// resource timing buffer: the page's diagnostics clear that buffer every minute (client-events.js, TRAFFIC_FLUSH_MS), and
// a read of the buffer would miss the answers that ended before a clear. The input events of the search box are timed on
// the page's clock.
const PAGE_INIT_SCRIPT = `(() => {
  window.__resources = [];
  new PerformanceObserver((list) => {
    for (const entry of list.getEntries()) window.__resources.push(entry);
  }).observe({ type: 'resource' });
  window.__searchInputs = [];
  document.addEventListener('input', (event) => {
    if (event.target && event.target.id === 'sidebar-search') {
      window.__searchInputs.push({ t: performance.now(), wall: Date.now() });
    }
  }, true);
})();`;

// Runs once the page's scripts are in place. It wraps the list's render, so each render and its paint is recorded. A
// repaint draws the list's last paint again with the same `sessions` object, so it is marked and is not an answer's
// paint. It also wraps the search refetch, so each refetch a broadcast causes is recorded.
export const WRAP_RENDER_SCRIPT = `(() => {
  window.__searchRenders = [];
  window.__refetchCalls = [];
  let lastSessions = null;
  const render = window.renderSessionList;
  window.renderSessionList = function (sessions, filter) {
    const record = {
      filter,
      rows: sessions.length,
      repaint: sessions === lastSessions,
      t0: performance.now(),
      wall0: Date.now(),
    };
    lastSessions = sessions;
    const out = render.apply(this, arguments);
    record.t1 = performance.now();
    window.__searchRenders.push(record);
    requestAnimationFrame(() => setTimeout(() => {
      record.paint = performance.now();
      record.wallPaint = Date.now();
    }, 0));
    return out;
  };
  const refetch = window.handleSidebarSearch;
  window.handleSidebarSearch = function (query) {
    window.__refetchCalls.push(Date.now());
    return refetch.apply(this, arguments);
  };
})()`;

// The answer to the query typed after input `inputIndex`, once it has painted after render `renderIndex`; null until then.
// Its overlap is the other resource entries in flight between the answer's request start and its response end, so a
// request that ended while the answer was still waiting for its first byte counts. An entry still in flight at this read
// is not in the log yet, and is not counted.
export const READ_ANSWER_SCRIPT = `(function (query, inputIndex, renderIndex) {
  const input = window.__searchInputs[inputIndex];
  if (!input) return null;
  const render = window.__searchRenders.slice(renderIndex)
    .find((r) => r.filter === 'search' && !r.repaint && r.t0 > input.t && r.paint !== undefined);
  if (!render) return null;
  const name = '${SEARCH_PATH}?q=' + encodeURIComponent(query);
  const entries = window.__resources;
  const entry = entries.find((e) => e.name.includes(name) && e.startTime > input.t && e.responseEnd <= render.t0);
  if (!entry) return null;
  return {
    query,
    input_t: input.t,
    paint: render.paint,
    paint_wall: render.wallPaint,
    rows: render.rows,
    encodedBodySize: entry.encodedBodySize,
    transferSize: entry.transferSize,
    overlap: entries
      .filter((e) => e !== entry && e.startTime < entry.responseEnd && e.responseEnd > entry.startTime)
      .map((e) => ({ path: new URL(e.name).pathname, transferSize: e.transferSize })),
  };
})`;

// The page's view of the wall-clock span [start, end): the refetches the broadcasts caused, the search requests that began
// in it, the search answers that ended in it with their wire bytes, the search renders that began in it (answer paints
// and repaints apart), and the bytes of every request that began in it, by path with the ids replaced. A 304 answer's
// transferSize is its headers only; its body is the browser's cached copy, so the wire bytes are the transferSize.
export const READ_SPAN_SCRIPT = `(function (start, end) {
  const wall = (time) => performance.timeOrigin + time;
  const inSpan = (time) => time >= start && time < end;
  const entries = window.__resources;
  const answers = entries.filter((e) => e.name.includes('${SEARCH_PATH}?') && inSpan(wall(e.responseEnd)));
  const started = entries.filter((e) => inSpan(wall(e.startTime)));
  const bytesByPath = {};
  for (const entry of started) {
    const path = new URL(entry.name).pathname.replace(/[0-9a-f]{8}-[0-9a-f-]{27}/g, '{id}');
    bytesByPath[path] = (bytesByPath[path] || 0) + entry.transferSize;
  }
  const polls = started.filter((e) => new URL(e.name).pathname === '${STATUS_PATH}');
  const renders = window.__searchRenders.filter((r) => r.filter === 'search' && inSpan(r.wall0));
  return {
    refetch_calls: window.__refetchCalls.filter((time) => inSpan(time)).length,
    search_fetches: started.filter((e) => e.name.includes('${SEARCH_PATH}?')).length,
    answers: answers.length,
    answer_wire_bytes: answers.reduce((sum, e) => sum + e.transferSize, 0),
    answer_paints: renders.filter((r) => !r.repaint).length,
    repaints: renders.filter((r) => r.repaint).length,
    status_polls: polls.length,
    status_bytes: polls.reduce((sum, e) => sum + e.transferSize, 0),
    bytes_by_path: bytesByPath,
  };
})`;

function typeScript(query) {
  return `(() => {
    const input = document.getElementById('sidebar-search');
    input.value = ${JSON.stringify(query)};
    input.dispatchEvent(new Event('input', { bubbles: true }));
  })()`;
}

// The renames of a window: the change count at `load`'s rate, the first at the window's start. Every offset is inside the
// window, so a window holds exactly `changesPerWindow` renames.
export function broadcastOffsets({ changesPerWindow, windowMs }) {
  const interval = windowMs / changesPerWindow;
  return Array.from({ length: changesPerWindow }, (_, index) => index * interval);
}

// Sends the window's renames of the background session, each at its offset from `startWall`, without waiting for the
// previous one: a slow answer must not move the next rename. Resolves with the renames sent, or rejects with the first
// failed rename.
function startBroadcast(api, sessionId, startWall, load) {
  const pending = [];
  return (async () => {
    for (const [index, offset] of broadcastOffsets(load).entries()) {
      const wait = startWall + offset - Date.now();
      if (wait > 0) {
        await sleep(wait);
      }
      pending.push(api.json('PATCH', `/api/sessions/${sessionId}`, { name: `bench search rename ${index + 1}` })
        .then(() => null, (error) => error));
    }
    const failures = (await Promise.all(pending)).filter((outcome) => outcome !== null);
    if (failures.length > 0) {
      throw failures[0];
    }
    return pending.length;
  })();
}

// Clears the box, types `query`, and waits for its answer to paint. Resolves with the answer's page record.
async function typeQuery(tab, query) {
  await tab.evaluate('clearSidebarSearch()');
  await sleep(SEARCH.clearSettleMs);
  const before = await tab.evaluate('({ inputs: window.__searchInputs.length, renders: window.__searchRenders.length })');
  await tab.evaluate(typeScript(query));
  return waitFor(
    () => tab.evaluate(`(${READ_ANSWER_SCRIPT})(${JSON.stringify(query)}, ${before.inputs}, ${before.renders})`),
    PAINT_TIMEOUT_MS,
    `the search for ${query} to paint`,
  );
}

// The window from `firstPaintWall`: the renames, then the page's answers over the accounting span. The frames count only
// the window itself, the frames that arrive after its end belong to the settle.
async function broadcastWindow(ctx, tab, tabs, firstPaintWall) {
  const load = SEARCH.broadcast;
  const windowEnd = firstPaintWall + load.windowMs;
  const renames = startBroadcast(ctx.api, ctx.sessions.background.id, firstPaintWall, load);
  await sleep(Math.max(0, windowEnd - Date.now()));
  const frames = tab.frames
    .filter((frame) => frame.type === 'task_tree_changed' && frame.at >= firstPaintWall && frame.at < windowEnd).length;
  const sent = await renames;
  await tabs.waitIdle(DRAIN_TIMEOUT_MS);
  await sleep(SEARCH.windowSettleMs);
  const settledWall = Date.now();
  const span = await tab.evaluate(`(${READ_SPAN_SCRIPT})(${firstPaintWall}, ${settledWall})`);
  return {
    window_ms: load.windowMs,
    span_ms: settledWall - firstPaintWall,
    renames: sent,
    task_tree_changed_frames: frames,
    ...span,
    discarded: windowDiscards(span),
  };
}

// A window's discards: the answers that ended in its span and did not paint. Every paint in the span belongs to an answer
// that ended in it, so more paints than answers is an accounting fault, and the run stops on it.
export function windowDiscards(span) {
  if (span.answer_paints > span.answers) {
    throw new Error(`the window painted ${span.answer_paints} answers but only ${span.answers} ended in its span`);
  }
  return span.answers - span.answer_paints;
}

// One query's record from its answer: the e2e time, the body and wire bytes, and the overlap.
export function searchRecord(answer) {
  return {
    query: answer.query,
    e2e_ms: answer.paint - answer.input_t,
    body_bytes: answer.encodedBodySize,
    wire_bytes: answer.transferSize,
    rows: answer.rows,
    overlap: overlapSummary(answer.overlap),
  };
}

// The other requests that overlapped one answer: their count and wire bytes, and the status polls among them.
export function overlapSummary(overlapping) {
  const polls = overlapping.filter((entry) => entry.path === STATUS_PATH);
  return {
    count: overlapping.length,
    bytes: sum(overlapping.map((entry) => entry.transferSize)),
    status_count: polls.length,
    status_bytes: sum(polls.map((entry) => entry.transferSize)),
  };
}

// The median of a list, or null when the list is empty.
function median(values) {
  return values.length === 0 ? null : percentile(values, 0.5);
}

function sum(values) {
  return values.reduce((total, value) => total + value, 0);
}

// A query's median body against its live before body: the relative difference, and whether it lies within the tolerance.
// hydeogen is reported without a check, so its `within` is null.
export function bodyCheck(query, bodyBytes) {
  const live = SEARCH.liveBeforeBodyBytes[query];
  const delta = bodyBytes / live - 1;
  const within = SEARCH.checkedQueries.includes(query) ? Math.abs(delta) <= SEARCH.bodyToleranceShare : null;
  return { live, delta, within };
}

// One row per variant and query: the medians over the repetitions of e2e, body, wire, overlap and status-poll bytes, and
// the body check. The broadcast rows add the window's discards, summed over the repetitions, and the window's medians.
export function summarizeSearch(records) {
  const rows = [];
  for (const variant of VARIANTS) {
    for (const query of SEARCH.queries) {
      const reps = records.filter((record) => record.variant === variant && record.query === query);
      const bodyMedian = median(reps.map((record) => record.body_bytes));
      const row = {
        variant,
        query,
        reps: reps.length,
        e2e_ms_median: median(reps.map((record) => record.e2e_ms)),
        body_bytes_median: bodyMedian,
        wire_bytes_median: median(reps.map((record) => record.wire_bytes)),
        overlap_bytes_median: median(reps.map((record) => record.overlap.bytes)),
        status_bytes_median: median(reps.map((record) => record.overlap.status_bytes)),
        check: bodyCheck(query, bodyMedian),
      };
      if (variant === 'broadcast') {
        const windows = reps.map((record) => record.window);
        row.discarded_total = sum(windows.map((window) => window.discarded));
        row.discarded_median = median(windows.map((window) => window.discarded));
        row.renames_median = median(windows.map((window) => window.renames));
        row.frames_median = median(windows.map((window) => window.task_tree_changed_frames));
        row.fetches_median = median(windows.map((window) => window.search_fetches));
        row.window_status_bytes_median = median(windows.map((window) => window.status_bytes));
      }
      rows.push(row);
    }
  }
  return rows;
}

// One repetition of one variant: a fresh browser on the bench's open session, then each query typed in order.
async function searchRepetition(ctx, variant, repetition) {
  return withLink(ctx, `search-${variant}`, ['search'], async ({ tabs, tab }) => {
    await tab.page.send('Page.addScriptToEvaluateOnNewDocument', { source: PAGE_INIT_SCRIPT });
    await tabs.navigate(tab, `/?session=${ctx.sessions.open.id}`);
    await tab.ready();
    await tabs.show(tab);
    await tabs.waitIdle(SETTLE_TIMEOUT_MS);
    await sleep(SEARCH.loadSettleMs);
    await tab.evaluate(WRAP_RENDER_SCRIPT);
    const records = [];
    for (const query of SEARCH.queries) {
      const answer = await typeQuery(tab, query);
      const record = { variant, repetition, ...searchRecord(answer) };
      if (variant === 'broadcast') {
        record.window = await broadcastWindow(ctx, tab, tabs, answer.paint_wall);
      }
      records.push(record);
    }
    return records;
  });
}

// Runs the whole view. Resolves with the records, the summary and the constants the run measured against.
export async function searchScenario(ctx) {
  const startedAt = new Date().toISOString();
  const records = [];
  for (let repetition = 1; repetition <= SEARCH.repetitions; repetition++) {
    for (const variant of VARIANTS) {
      process.stderr.write(`search ${variant} repetition ${repetition}/${SEARCH.repetitions}\n`);
      records.push(...(await searchRepetition(ctx, variant, repetition)));
    }
  }
  return {
    startedAt,
    finishedAt: new Date().toISOString(),
    live: SEARCH.liveBeforeBodyBytes,
    bodyToleranceShare: SEARCH.bodyToleranceShare,
    broadcast: SEARCH.broadcast,
    records,
    summary: summarizeSearch(records),
  };
}

// The view's table: one line per variant and query, with the body against the live before body.
export function formatSearch(search) {
  const bytes = (value) => (value === null ? '-' : `${Math.round(value)}`);
  const seconds = (value) => (value === null ? '-' : (value / 1000).toFixed(2));
  const header = ['variant', 'query', 'reps', 'e2e s', 'body B', 'live B', 'delta', 'check', 'overlap B', 'status B',
    'discards', 'renames', 'frames', 'fetches'];
  const lines = [
    `search view: ${SEARCH.repetitions} repetitions per query; body check ±${SEARCH.bodyToleranceShare * 100}% of the ` +
      'live before body on sidebar, plan and cron (hydeogen reported only)',
    header.map((cell) => cell.padEnd(10)).join(' ').trimEnd(),
  ];
  for (const row of search.summary) {
    const cells = [
      row.variant,
      row.query,
      String(row.reps),
      seconds(row.e2e_ms_median),
      bytes(row.body_bytes_median),
      bytes(row.check.live),
      row.check.delta === null ? '-' : `${(row.check.delta * 100).toFixed(1)}%`,
      row.check.within === null ? 'n/a' : row.check.within ? 'within' : 'outside',
      bytes(row.overlap_bytes_median),
      bytes(row.status_bytes_median),
      row.variant === 'broadcast' ? `${row.discarded_total} total` : '-',
      row.variant === 'broadcast' ? bytes(row.renames_median) : '-',
      row.variant === 'broadcast' ? bytes(row.frames_median) : '-',
      row.variant === 'broadcast' ? bytes(row.fetches_median) : '-',
    ];
    lines.push(cells.map((cell) => cell.padEnd(10)).join(' ').trimEnd());
  }
  return lines.join('\n');
}
