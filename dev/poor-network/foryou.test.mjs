// The For-you scenario's seed, against a scripted api: the shape it requires, the page's For-you rule it counts
// with, and the ways a target can fall out of that shape.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { seedForYouTargets, seedSparseTarget } from './foryou.mjs';

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