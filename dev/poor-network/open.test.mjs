// Tests of the open scenario's invocation-level decisions: the CPU rate the sides of one run share, the host
// interference a repetition names, and the bounded rerun of a repetition the host spoiled. A mistake here leaves
// the two sides of one run at different CPU speeds, or dresses the host's behaviour as the page's result. The open
// record folds a tab's reports, across a reload, into the times and the case one open reports.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import { chromeBinary, launchBrowser } from './chrome.mjs';
import { assetVersionOf, hostInterference, openCpuRecord, openMeasuredTab, openRecord, openSettled, runRepetitions }
  from './open.mjs';
import { LIVE_CPU_TAIL, OPEN } from './profile.mjs';
import { assertSameTargetAsBefore, expectedTargetsFrom, newestRowMessage } from './newest-row.mjs';
import { Tabs, waitFor } from './tabs.mjs';

const DOC_URL = 'https://localhost:8443/?session=open';

test('the asset version is read from the folded declaration the server serves, and from the spaced one', () => {
  assert.equal(assetVersionOf('<script>const STATIC_ASSET_VERSION="-c648d5b36ecf";</script>'), '-c648d5b36ecf');
  assert.equal(assetVersionOf('const STATIC_ASSET_VERSION = "v9";\n'), 'v9');
  assert.throws(() => assetVersionOf('<html></html>'), /carries no STATIC_ASSET_VERSION/);
});

// A repetition's record, with the fields the decisions read.
function repOf({ navigationError = null, networkChanged = [], openInHoldMs = null } = {}) {
  return { navigationError, networkChanged, openInHoldMs, newestMs: 20000 };
}

// A reference repetition whose document is case A's revalidated copy: a 304's headers on the wire for the cached body.
function referenceRep(rep, tailMs, { documentTransferBytes = 300, documentBodyBytes = 84566 } = {}) {
  return { rep, navigationError: null, tailMs, documentTransferBytes, documentBodyBytes };
}

test('one invocation, one CPU rate: a shared rate stands beside the one this side derives', () => {
  const reps = [referenceRep(1, 80), referenceRep(2, 84), referenceRep(3, 82)];
  const own = openCpuRecord(null, reps);
  assert.equal(own.rate, own.derivedRate);
  assert.equal(own.derivedRate, Math.max(1, Math.round((LIVE_CPU_TAIL.medianMs / 82) * 100) / 100));
  assert.deepEqual(own.referenceTails, [80, 84, 82]);

  const shared = openCpuRecord(3.56, reps);
  assert.equal(shared.rate, 3.56, 'the cells run at the rate the invocation passed');
  assert.equal(shared.derivedRate, own.derivedRate, 'the gap to this side\'s own rate stays readable');
  assert.notEqual(shared.rateSource, own.rateSource);
});

test('a reference document that crossed the link in full gives no CPU rate', () => {
  // Round 4's reference: the whole 85.6 KB page streamed over 7.4 s, and its 10 ms tail made the rate 24.55.
  const streamed = referenceRep(2, 10.3, { documentTransferBytes: 85936, documentBodyBytes: 85636 });
  assert.throws(() => openCpuRecord(null, [referenceRep(1, 80), streamed]), /reference repetition 2 gives no CPU tail/);
  const failed = { rep: 1, navigationError: 'net::ERR_TIMED_OUT', tailMs: null, documentTransferBytes: null,
    documentBodyBytes: null };
  assert.throws(() => openCpuRecord(3.56, [failed]), /net::ERR_TIMED_OUT/);
});

test('a request the host\'s network change dropped names the interference', () => {
  const cause = hostInterference(repOf({ networkChanged: [{ at: 5, url: DOC_URL }] }));
  assert.match(cause, /network change/);
  assert.match(cause, /\/\?session=/);
});

test('a navigation that failed with the network-change error is interference without a request record', () => {
  const cause = hostInterference(repOf({ navigationError: 'net::ERR_NETWORK_CHANGED' }));
  assert.match(cause, /net::ERR_NETWORK_CHANGED/);
});

test('a release open the bench host pushed late into the hold is interference, an open in place is the page\'s', () => {
  const late = hostInterference(repOf({ navigationError: 'net::ERR_TIMED_OUT', openInHoldMs: OPEN.openByMs + 700 }));
  assert.match(late, /bench host stalled/);
  assert.equal(hostInterference(repOf({ navigationError: 'net::ERR_TIMED_OUT', openInHoldMs: OPEN.openAfterMs + 400 })),
    null);
});

test('another load failure stays the repetition\'s own record', () => {
  assert.equal(hostInterference(repOf({ navigationError: 'net::ERR_CONNECTION_RESET' })), null);
  assert.equal(hostInterference(repOf()), null);
});

test('a spoiled repetition is rerun, and its rerun is recorded with its cause', async () => {
  let calls = 0;
  const reps = await runRepetitions(async () => {
    calls += 1;
    return calls === 1 ? repOf({ networkChanged: [{ at: 5, url: DOC_URL }] }) : repOf();
  }, 1);
  assert.equal(calls, 2, 'the repetition was measured again');
  assert.equal(reps.length, 1);
  assert.equal(reps[0].interferenceReruns.length, 1);
  assert.match(reps[0].interferenceReruns[0].cause, /network change/);
  assert.equal(reps[0].newestMs, 20000, 'the rerun\'s record, not the spoiled one, is kept');
});

test('the rerun bound fails the run and keeps every rerun in the repetition record', async () => {
  let calls = 0;
  const reps = [];
  await assert.rejects(
    runRepetitions(async () => {
      calls += 1;
      return repOf({ networkChanged: [{ at: calls, url: DOC_URL }] });
    }, 1, reps),
    /host interference/,
  );
  assert.equal(calls, 1 + OPEN.interferenceReruns, 'the first measurement plus the bounded reruns');
  assert.equal(reps.length, 1);
  assert.equal(reps[0].interferenceReruns.length, OPEN.interferenceReruns);
  assert.match(reps[0].hostInterference, /network change/);
  assert.equal(reps[0].networkChanged.length, 1, 'the final spoiled attempt is recorded, not counted as a page result');
});

test('a repetition that fails any other way is not rerun', async () => {
  let calls = 0;
  const reps = await runRepetitions(async () => {
    calls += 1;
    return repOf({ navigationError: 'net::ERR_CONNECTION_RESET' });
  }, 1);
  assert.equal(calls, 1);
  assert.deepEqual(reps[0].interferenceReruns, []);
});

test('a reload inside an open keeps the first navigation as the start', () => {
  // The first document loads, its socket answers 4409, and the reloaded document shows the newest message 10.6 s
  // after the first navigation.
  const record = openRecord([
    { kind: 'document', timeOrigin: 1000, recordAtStart: null },
    { kind: 'cause', timeOrigin: 1000, cause: 'stale_asset' },
    { kind: 'document', timeOrigin: 11000, recordAtStart: null },
    { kind: 'newest', timeOrigin: 11000, at: 11600 },
    { kind: 'load', timeOrigin: 11000, loadMs: 700, openCase: 'C' },
  ]);
  assert.equal(record.startWall, 1000, 'the open starts at the first navigation');
  assert.equal(record.newestWall - record.startWall, 10600, 'the reloaded newest message counts from the first navigation');
  assert.equal(record.loadWall, 11700, 'the open ends with the last document\'s load');
});

test('the newest-row target is the last message the For you view paints, and a side that differs from the before side stops', () => {
  const screen = [
    { id: 'u1', role: 'user', content: 'hi' },
    { id: 'w1', role: 'web_reply', content: 'answer' },
    { id: 's1', role: 'separator', kind: 'run_end' },
    { id: 'a1', role: 'assistant', content: 'worker text' },
    { id: 'x1', role: 'system', content: 'Error: boom' },
    { id: 'y1', role: 'system', content: 'note' },
    { id: 'z1', role: 'separator' },
  ];
  assert.equal(newestRowMessage(screen).id, 'x1');
  assert.equal(newestRowMessage(screen.slice(0, 2)).id, 'w1');
  assert.equal(newestRowMessage([{ id: 'a', role: 'assistant' }, { role: 'user', content: 'no id' }]), null);
  const before = expectedTargetsFrom({
    reference: { id: 'reference A link rate 1', reps: [{ rep: 1, newestTargetId: 'x1' }] },
    cells: [],
  });
  assertSameTargetAsBefore(before, 'reference A link rate 1', 1, 'x1');
  assert.throws(() => assertSameTargetAsBefore(before, 'reference A link rate 1', 1, 'w1'),
    /this side w1, the before side x1/);
  assertSameTargetAsBefore(null, 'A link', 1, 'w1');
});

test('the newest message counts at the first document that shows it', () => {
  const record = openRecord([
    { kind: 'document', timeOrigin: 1000, recordAtStart: null },
    { kind: 'newest', timeOrigin: 1000, at: 1900 },
    { kind: 'cause', timeOrigin: 1000, cause: 'stale_asset' },
    { kind: 'document', timeOrigin: 11000, recordAtStart: null },
    { kind: 'newest', timeOrigin: 11000, at: 11600 },
  ]);
  assert.equal(record.newestWall, 1900);
});

test('the measured byte window settles without waiting for a catch-up frame', () => {
  const record = { newestWall: 1000, loadWall: 1200 };
  assert.equal(openSettled(record, 1200 + OPEN.postLoadWindowMs - 1), false);
  assert.equal(openSettled(record, 1200 + OPEN.postLoadWindowMs), true);
  assert.equal(openSettled({ ...record, newestWall: null }, 1200 + OPEN.postLoadWindowMs), false);
});

test('a newest paint settles when its document has no load event', () => {
  const record = { newestWall: 1000, loadWall: null };
  assert.equal(openSettled(record, 1000 + OPEN.postLoadWindowMs - 1), false);
  assert.equal(openSettled(record, 1000 + OPEN.postLoadWindowMs), true);
});

test('the open case and the record it starts from come from the first document', () => {
  // The first document finds the browser's record without this session (case B) and writes the session into it; the
  // reloaded document reads the record with the session in it, which would name case A.
  const first = JSON.stringify({ first_open_ms: 1, sessions: { probe: 1 } });
  const reloaded = JSON.stringify({ first_open_ms: 1, sessions: { probe: 1, target: 9 } });
  const record = openRecord([
    { kind: 'document', timeOrigin: 1000, recordAtStart: first },
    { kind: 'load', timeOrigin: 1000, loadMs: 300, openCase: 'B' },
    { kind: 'cause', timeOrigin: 1000, cause: 'stale_asset' },
    { kind: 'document', timeOrigin: 11000, recordAtStart: reloaded },
    { kind: 'load', timeOrigin: 11000, loadMs: 300, openCase: 'A' },
  ]);
  assert.equal(record.openCase, 'B');
  assert.equal(record.recordAtStart, first);
});

test('each reload is recorded with its cause, and a reload whose document named none is recorded as null', () => {
  const once = openRecord([
    { kind: 'document', timeOrigin: 1000, recordAtStart: null },
    { kind: 'cause', timeOrigin: 1000, cause: 'stale_asset' },
    { kind: 'document', timeOrigin: 11000, recordAtStart: null },
  ]);
  assert.deepEqual(once.reloadCauses, ['stale_asset']);

  const twice = openRecord([
    { kind: 'document', timeOrigin: 1000, recordAtStart: null },
    { kind: 'cause', timeOrigin: 1000, cause: 'stale_asset' },
    { kind: 'document', timeOrigin: 11000, recordAtStart: null },
    { kind: 'document', timeOrigin: 21000, recordAtStart: null },
  ]);
  assert.deepEqual(twice.reloadCauses, ['stale_asset', null]);

  assert.deepEqual(openRecord([{ kind: 'document', timeOrigin: 1000, recordAtStart: null }]).reloadCauses, []);
});

// A page that reloads once, the way a stale page does: its first document reports the cause, then reloads. It holds
// an iframe, as a chat page holds its artifact frames, so a frame's own load must not count as a document.
const RELOADING_PAGE = `<!doctype html><html><body><iframe srcdoc="<p>frame</p>"></iframe><script>
if (sessionStorage.getItem('reloaded') === null) {
  sessionStorage.setItem('reloaded', '1');
  document.dispatchEvent(new CustomEvent('charliebot:page-load-cause', { detail: { cause: 'stale_asset' } }));
  location.reload();
}
</script></body></html>`;

test('a page reload reports its cause through the binding that outlives it, and a frame adds no document', async () => {
  const server = http.createServer((request, response) => {
    response.writeHead(200, { 'Content-Type': 'text/html' });
    response.end(RELOADING_PAGE);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  // The browser sends the page's plain-HTTP requests to the proxy the launch names, with their absolute URLs. This
  // forward proxy passes each one to the page server.
  const forward = http.createServer((request, response) => {
    const upstream = http.request(new URL(request.url), { method: request.method, headers: request.headers }, (answer) => {
      response.writeHead(answer.statusCode, answer.headers);
      answer.pipe(response);
    });
    upstream.on('error', (error) => response.destroy(error));
    request.pipe(upstream);
  });
  await new Promise((resolve) => forward.listen(0, '127.0.0.1', resolve));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'pn-reload-'));
  let browser = null;
  try {
    browser = await launchBrowser({
      binary: chromeBinary(),
      proxyPort: forward.address().port,
      userDataDir: path.join(dir, 'profile'),
      logPath: path.join(dir, 'chrome.log'),
      trustedSpki: '',
    });
    const tabs = new Tabs({
      browser, assets: new Map(), accessKey: 'test-key', origin: `http://127.0.0.1:${server.address().port}`, cdn: 'link',
    });
    // Three tabs, each with its own session storage, so each reloads once: the cause and the reload repeat.
    for (let attempt = 1; attempt <= 3; attempt++) {
      const tab = await openMeasuredTab(tabs, 'no-such-message');
      await tabs.navigate(tab, '/');
      const record = await waitFor(() => {
        const current = openRecord(tab.reports);
        return current.loadWall !== null && current.reloadCauses.length > 0 ? current : null;
      }, 20000, `the reloaded document of attempt ${attempt} to report its load`);
      assert.deepEqual(record.reloadCauses, ['stale_asset'], `attempt ${attempt}: the reload names its cause and no frame counts`);
      await tabs.close(tab);
    }
  } finally {
    if (browser !== null) {
      await browser.shutdown();
    }
    forward.close();
    server.close();
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
