// Tests of the open scenario's invocation-level decisions: the CPU rate the sides of one run share, the host
// interference a repetition names, and the bounded rerun of a repetition the host spoiled. A mistake here leaves
// the two sides of one run at different CPU speeds, or dresses the host's behaviour as the page's result.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { assetVersionOf, hostInterference, openCpuRecord, runRepetitions } from './open.mjs';
import { LIVE_CPU_TAIL, OPEN } from './profile.mjs';

const DOC_URL = 'https://localhost:8443/?session=open';

test('the asset version is read from the folded declaration the server serves, and from the spaced one', () => {
  assert.equal(assetVersionOf('<script>const STATIC_ASSET_VERSION="-c648d5b36ecf";</script>'), '-c648d5b36ecf');
  assert.equal(assetVersionOf('const STATIC_ASSET_VERSION = "v9";\n'), 'v9');
  assert.throws(() => assetVersionOf('<html></html>'), /carries no STATIC_ASSET_VERSION/);
});

// A repetition's record, with the fields the decisions read.
function repOf({ navigationError = null, networkChanged = [] } = {}) {
  return { navigationError, networkChanged, newestMs: 20000 };
}

test('one invocation, one CPU rate: a shared rate stands beside the one this side derives', () => {
  const tails = [10, 12, 11];
  const own = openCpuRecord(null, tails);
  assert.equal(own.rate, own.derivedRate);
  assert.equal(own.derivedRate, Math.max(1, Math.round((LIVE_CPU_TAIL.medianMs / 11) * 100) / 100));
  assert.match(own.rateSource, /this side/);

  const shared = openCpuRecord(23.57, tails);
  assert.equal(shared.rate, 23.57, 'the cells run at the rate the invocation passed');
  assert.equal(shared.derivedRate, own.derivedRate, 'the gap to this side\'s own rate stays readable');
  assert.match(shared.rateSource, /before side/);
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
