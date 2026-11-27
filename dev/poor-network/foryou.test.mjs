// The For-you scenario's seed, against a scripted api: the shape it requires, the page's For-you rule it counts
// with, and the ways a target can fall out of that shape.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  cellsDue, measureOpenCell, openDocumentPath, seedForYouTargets, seedSparseTarget, triggerOlderPageUntilFailure,
} from './foryou.mjs';
import { PAGE_REPORT_BINDING, Tab } from './tabs.mjs';

// An api whose session log ends with the marker turn and `above` (the painted rows that turn leaves, one notice by
// default), followed by the trigger tail's rows (none painted). The bootstrap's first screen carries `firstScreen`
// (the tail's newest rows in a real run) and its `oldest_message_ordinal` is where the log ends. A bootstrap read
// after the first reports `tailLines` more event lines than the marker turn's read.
function fakeApi({
  firstScreen = [], above = [{role: 'web_reply_notice', content: 'notice'}], tailLines = 10, recordMarker = true,
} = {}) {
  const calls = [];
  const logs = new Map();
  const bootstrapReads = new Map();
  const logOf = (id) => {
    if (!logs.has(id)) logs.set(id, []);
    return logs.get(id);
  };
  const bootstrapOf = (id) => {
    const reads = (bootstrapReads.get(id) ?? 0) + 1;
    bootstrapReads.set(id, reads);
    return {
      event_count: reads === 1 ? 100 : 100 + tailLines,
      oldest_message_ordinal: logOf(id).length,
      messages: firstScreen,
    };
  };
  let created = 0;
  return {
    calls,
    async createSession(name, backendId) {
      calls.push(['create', name, backendId]);
      created++;
      return {id: `created-${created}`, group: 'g'};
    },
    async sendMessage(id, content) {
      calls.push(['send', id, content]);
      logOf(id).push({role: 'user', content: recordMarker ? content : 'another message'}, ...above);
    },
    async waitForIdle() {},
    async scheduleTrigger(id, _delay, message) {
      calls.push(['trigger', id, message]);
      logOf(id).push({role: 'scheduled_trigger', content: message}, {role: 'assistant', content: 'tail reply'});
      return `trigger:${message}`;
    },
    async cancelTrigger(id, triggerId) {
      calls.push(['cancel', id, triggerId]);
    },
    async bootstrapWireBytes() {
      return {wireBytes: 10, untrimmedBytes: 999999};
    },
    async json(_method, path) {
      calls.push(['get', path]);
      const events = path.match(/api\/sessions\/([^/]+)\/events\?before=(\d+)&limit=40/);
      if (events) {
        const log = logOf(events[1]);
        const before = Number(events[2]);
        const start = Math.max(0, before - 40);
        return {has_more: start > 0, next_before: start, messages: log.slice(start, before)};
      }
      const id = path.match(/api\/sessions\/([^/]+)\/bootstrap$/)[1];
      return bootstrapOf(id);
    },
  };
}

const SESSIONS = { foryouLarge: { id: 'large', group: 'g' }, foryouTypical: { id: 'typical', group: 'g' } };

test('the seed gives both targets the marker turn and the trigger tail, and returns their markers', async () => {
  const api = fakeApi();
  const targets = await seedForYouTargets(api, SESSIONS);
  // Each target's marker names it, so a cell's marker is on no other target's page.
  assert.notEqual(targets.large.marker, targets.typical.marker);
  assert.match(targets.large.marker, /^pn-foryou marker .* large$/);
  assert.match(targets.typical.marker, /^pn-foryou marker .* typical$/);
  // The large target sizes first: one sizing turn, then its marker turn; the typical target carries no sizing.
  const largeTurns = api.calls.filter((call) => call[0] === 'send' && call[1] === 'large');
  assert.match(largeTurns[0][2], /^pn-foryou size /);
  assert.match(largeTurns[1][2], /^pn-foryou marker /);
  assert.equal(api.calls.filter((call) => call[0] === 'send' && call[1] === 'typical').length, 1);
  // Each target's tail holds its keeper and fires its trigger turns, which the wait for idle follows; the keeper is
  // cancelled after the last turn, so no turn wakes on it.
  for (const id of ['large', 'typical']) {
    const triggers = api.calls.filter((call) => call[0] === 'trigger' && call[1] === id).map((call) => call[2]);
    assert.equal(triggers.length, 24);
    assert.match(triggers[0], /^pn-foryou keeper /);
    assert.equal(triggers.filter((message) => /^pn-foryou tail /.test(message)).length, 23);
    const cancels = api.calls.filter((call) => call[0] === 'cancel' && call[1] === id);
    assert.equal(cancels.length, 1);
    assert.match(cancels[0][2], /^trigger:pn-foryou keeper /);
  }
});

test('the sparse target is created on the backend and seeded with its marker and the tail', async () => {
  const api = fakeApi();
  const sparse = await seedSparseTarget(api, 'backend-id');
  assert.equal(sparse.id, 'created-1');
  assert.equal(sparse.role, 'sparse');
  assert.match(sparse.marker, /^pn-foryou marker .* sparse$/);
  assert.deepEqual(api.calls.find((call) => call[0] === 'create').slice(2), ['backend-id']);
  assert.equal(api.calls.filter((call) => call[0] === 'send' && call[1] === sparse.id).length, 1);
  assert.equal(api.calls.filter((call) => call[0] === 'trigger' && call[1] === sparse.id).length, 24);
});

test('a first screen that holds a For-you row by the page rule fails the seed', async () => {
  // A painted row of each kind the page's rule counts: a reply role and an error row (a system row with the prefix).
  for (const painted of [{role: 'user', content: 'unexpected'}, {role: 'system', content: 'Error: blocked'}]) {
    const api = fakeApi({firstScreen: [painted]});
    await assert.rejects(seedForYouTargets(api, SESSIONS), /first screen holds 1 For-you rows/);
  }
});

test('a marker behind the newest ten painted rows fails the seed', async () => {
  const notices = Array.from({length: 10}, (_, index) => ({role: 'web_reply_notice', content: `notice ${index}`}));
  const api = fakeApi({above: notices});
  await assert.rejects(seedForYouTargets(api, SESSIONS), /marker sits behind 10 painted rows/);
});

test('a marker the walk never finds fails the seed', async () => {
  // The log holds a different user message where the marker should be, so the walk ends at the log's start without
  // meeting the marker: the seed fails rather than timing another row.
  const api = fakeApi({recordMarker: false});
  await assert.rejects(seedForYouTargets(api, SESSIONS), /marker is not among the painted rows/);
});

test('a trigger tail that reaches past the For-you scan bound fails the seed', async () => {
  const api = fakeApi({tailLines: 1600});
  await assert.rejects(seedForYouTargets(api, SESSIONS), /trigger tail spans 1600 event lines/);
});

// Plan 5 v4 section 4.2: the large switch and both first opens take five repetitions, the other cells two. A repetition
// past a cell's count does not run it.
test('each For-you cell runs for its own repetitions', () => {
  const dueAt = (repetition) => Object.entries(cellsDue(repetition)).filter(([, due]) => due).map(([cell]) => cell);
  assert.equal(dueAt(1).length, 7);
  assert.deepEqual(dueAt(3), ['switchLarge', 'openLarge', 'openTypical']);
  assert.deepEqual(dueAt(6), []);
});

test('the failed-page cell keeps asking the page to scroll until its hook records the request', async () => {
  let scrolls = 0;
  let failedAt = null;
  const tab = {
    async evaluate(script) {
      if (script === 'window.__pnFail?.at ?? null') return failedAt;
      assert.match(script, /container\.dispatchEvent\(new Event\('scroll'\)\)/);
      scrolls++;
      if (scrolls === 2) failedAt = 123;
    },
  };

  assert.equal(await triggerOlderPageUntilFailure(tab, 2000), 123);
  assert.equal(scrolls, 2);
});

// A scripted `tabs` driver backed by a real `Tab` for `measureOpenCell`: `warmPolls` and `filesPolls` return one
// value per poll of `window.__warmReport` and `window.__filesStored` (repeating their last entry once exhausted).
function openCellHarness({
  warmPolls,
  filesPolls = [{ stored: 8, revalidated: 0 }],
  documentBytes = 0,
  workerRequests = [],
  reportTarget = true,
}) {
  const events = [];
  let tab = null;
  let warmIndex = 0;
  let filesIndex = 0;
  let initScriptSeen = null;
  const targetId = 'target-id';
  const openId = 'open-id';
  const ctx = { sessions: { open: { id: openId } } };
  const target = { id: targetId, marker: 'pn-foryou marker test large' };
  const tabs = {
    events,
    get initScript() {
      return initScriptSeen;
    },
    async open(name, initScript) {
      initScriptSeen = initScript;
      const page = {
        async send(method, params) {
          events.push(['send', method, params]);
          return {};
        },
        async evaluate(expression) {
          if (expression.includes("document.readyState === 'complete'")) return true;
          if (expression === 'window.__warmReport ?? null') {
            const value = warmPolls[Math.min(warmIndex, warmPolls.length - 1)];
            warmIndex++;
            events.push(['poll-warm', value]);
            return value;
          }
          if (expression === 'window.__filesStored ?? null') {
            const value = filesPolls[Math.min(filesIndex, filesPolls.length - 1)];
            filesIndex++;
            events.push(['poll-files', value]);
            return value;
          }
          throw new Error(`unexpected evaluate expression: ${expression}`);
        },
      };
      tab = new Tab(name, page);
      return tab;
    },
    async navigate(targetTab, urlPath) {
      events.push(['navigate', urlPath]);
      if (urlPath === `/?session=${targetId}`) {
        const docUrl = `https://front.test/?session=${targetId}`;
        targetTab.handle({
          method: 'Network.requestWillBeSent',
          params: { requestId: 'doc', request: { url: docUrl } },
        });
        targetTab.handle({
          method: 'Network.loadingFinished',
          params: { requestId: 'doc', encodedDataLength: documentBytes },
        });
        if (reportTarget) {
          targetTab.handle({
            method: 'Runtime.bindingCalled',
            params: {
              name: PAGE_REPORT_BINDING,
              payload: JSON.stringify({ kind: 'foryou', at: Date.now() + 5, href: docUrl }),
            },
          });
        }
      }
    },
    workerTrace() {
      return workerRequests;
    },
  };
  return { tabs, ctx, target };
}

test('the open cell navigates to the target only after the warm and on-demand-files reports arrive', async () => {
  const { tabs, ctx, target } = openCellHarness({
    warmPolls: [null, null, { stored: true }],
    filesPolls: [null, { stored: 8, revalidated: 0 }],
    documentBytes: 0,
    workerRequests: [],
  });

  const rep = await measureOpenCell(tabs, ctx, target, 'fy-open-large-1', 5000);
  assert.match(tabs.initScript, /charliebot:offline-shell-warm/);
  assert.match(tabs.initScript, /charliebot:offline-shell-files-stored/);

  const navigations = tabs.events.filter(([kind]) => kind === 'navigate').map(([, path]) => path);
  assert.deepEqual(navigations, ['/?session=open-id', '/?session=target-id']);

  const targetNavAt = tabs.events.findIndex(([kind, path]) => kind === 'navigate' && path === '/?session=target-id');
  const warmStoredAt = tabs.events.findIndex(([kind, val]) => kind === 'poll-warm' && val?.stored === true);
  const filesStoredAt = tabs.events.findIndex(([kind, val]) => kind === 'poll-files' && val?.stored === 8);
  assert.ok(warmStoredAt > 0 && filesStoredAt > warmStoredAt && targetNavAt > filesStoredAt);

  assert.equal(rep.failed, undefined);
  assert.equal(rep.capped, false);
  assert.equal(rep.documentBytes, 0);
  assert.equal(rep.pages, 0);
  assert.equal(rep.path, 'stored-shell');
});

test('the open cell records warm not reported and skips the target navigation when the deadline passes', async () => {
  const { tabs, ctx, target } = openCellHarness({ warmPolls: [null] });
  const rep = await measureOpenCell(tabs, ctx, target, 'fy-open-large-1', 50);

  const navigations = tabs.events.filter(([kind]) => kind === 'navigate').map(([, path]) => path);
  assert.deepEqual(navigations, ['/?session=open-id']);
  assert.deepEqual(rep, { failed: true, reason: 'warm not reported' });
});

test('the open cell records a reported warm error without waiting for on-demand files or navigating', async () => {
  const { tabs, ctx, target } = openCellHarness({
    warmPolls: [{ error: 'Error: the offline shell has no registration', update: null }],
    filesPolls: [null],
  });
  const rep = await measureOpenCell(tabs, ctx, target, 'fy-open-typical-1', 5000);

  const navigations = tabs.events.filter(([kind]) => kind === 'navigate').map(([, path]) => path);
  assert.deepEqual(navigations, ['/?session=open-id']);
  assert.equal(tabs.events.some(([kind]) => kind === 'poll-files'), false);
  assert.equal(rep.failed, true);
  assert.match(rep.reason, /the offline shell has no registration/);
});

test('each open rep records whether it came from the stored shell or the network', async () => {
  const stored = openCellHarness({
    warmPolls: [{ stored: true }],
    documentBytes: 0,
    workerRequests: [
      { url: 'https://front.test/?session=target-id', startMs: 0, status: 204 },
      { url: 'https://front.test/api/sessions/target-id/bootstrap', startMs: 20, status: 200 },
    ],
  });
  const storedRep = await measureOpenCell(stored.tabs, stored.ctx, stored.target, 'fy-open-large-1', 5000);
  assert.equal(storedRep.path, 'stored-shell');

  const workerNetwork = openCellHarness({
    warmPolls: [{ stored: true }],
    documentBytes: 0,
    workerRequests: [{ url: 'https://front.test/?session=target-id', startMs: 15, status: 200 }],
  });
  const workerNetworkRep = await measureOpenCell(
    workerNetwork.tabs,
    workerNetwork.ctx,
    workerNetwork.target,
    'fy-open-large-2',
    5000,
  );
  assert.equal(workerNetworkRep.path, 'network');

  const tabNetwork = openCellHarness({
    warmPolls: [{ stored: true }],
    documentBytes: 97000,
    workerRequests: [],
  });
  const tabNetworkRep = await measureOpenCell(
    tabNetwork.tabs,
    tabNetwork.ctx,
    tabNetwork.target,
    'fy-open-typical-1',
    5000,
  );
  assert.equal(tabNetworkRep.path, 'network');

  // A pre-navigation worker request (startMs < 0) does not mark the later navigation as a network fetch.
  assert.equal(
    openDocumentPath({
      documentBytes: 0,
      workerRequests: [{ url: 'https://front.test/?session=target-id', startMs: -500, status: 200 }],
      documentPattern: /\?session=target-id$/,
      report: { at: 100 },
    }),
    'stored-shell',
  );

  // The bench's own DevTools attach to the worker leaves an aborted navigation request for the target document
  // (`type: Other, status: null, failed: net::ERR_ABORTED, bytes: 0`). An entry without a numeric status is not a
  // response, so the aborted attach artifact keeps the stored-shell label.
  assert.equal(
    openDocumentPath({
      documentBytes: 0,
      workerRequests: [
        { url: 'https://front.test/?session=target-id', startMs: 0, status: 204 },
        {
          url: 'https://front.test/?session=target-id',
          type: 'Other',
          status: null,
          wireStatus: null,
          fromServiceWorker: null,
          fromDiskCache: null,
          failed: 'net::ERR_ABORTED',
          startMs: 15,
          finishMs: null,
          bytes: 0,
        },
        { url: 'https://front.test/?session=target-id', startMs: 20 },
      ],
      documentPattern: /\?session=target-id$/,
      report: { at: 100 },
    }),
    'stored-shell',
  );

  // A failed request is not a response even if the trace also has a numeric status.
  assert.equal(
    openDocumentPath({
      documentBytes: 0,
      workerRequests: [
        {
          url: 'https://front.test/?session=target-id',
          startMs: 15,
          status: 500,
          failed: 'net::ERR_ABORTED',
        },
      ],
      documentPattern: /\?session=target-id$/,
      report: { at: 100 },
    }),
    'stored-shell',
  );

  // A completed 200 for the target document is a network response, so the classification still reads network.
  assert.equal(
    openDocumentPath({
      documentBytes: 0,
      workerRequests: [{ url: 'https://front.test/?session=target-id', startMs: 15, status: 200 }],
      documentPattern: /\?session=target-id$/,
      report: { at: 100 },
    }),
    'network',
  );
});
