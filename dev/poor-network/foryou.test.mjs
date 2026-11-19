// The For-you scenario's seed, against a scripted api: the shape it requires, and the two ways a target can fall
// out of that shape.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { seedForYouTargets } from './foryou.mjs';

// An api whose session carries `roles` in its first screen and adds `tailLines` event lines with the tail. The large
// target's sizing reads a bootstrap already over the view's bytes, so one turn sizes it.
function fakeApi({ roles, tailLines }) {
  const calls = [];
  const markerCounts = new Map();
  let eventCount = 100;
  return {
    calls,
    async sendMessage(id, content) {
      calls.push(['send', id, content]);
    },
    async waitForIdle() {},
    async scheduleTrigger(id, _delay, message) {
      calls.push(['trigger', id, message]);
      return `trigger-${calls.length}`;
    },
    async bootstrapWireBytes(id) {
      return { wireBytes: 10, untrimmedBytes: 999999 };
    },
    async json(_method, path) {
      calls.push(['get', path]);
      // The marker's bootstrap is the first read of a target; the second carries the tail's lines on top of it.
      const id = path.match(/api\/sessions\/([^/]+)\/bootstrap/)[1];
      if (!markerCounts.has(id)) {
        markerCounts.set(id, eventCount);
        return { event_count: eventCount, messages: roles.map((role) => ({ role })) };
      }
      return { event_count: markerCounts.get(id) + tailLines, messages: roles.map((role) => ({ role })) };
    },
  };
}

const SESSIONS = { foryouLarge: { id: 'large', group: 'g' }, foryouTypical: { id: 'typical', group: 'g' } };

test('the seed gives both targets the marker turn and the trigger tail, and returns their markers', async () => {
  const api = fakeApi({ roles: ['assistant', 'scheduled_trigger'], tailLines: 10 });
  const targets = await seedForYouTargets(api, SESSIONS);
  assert.equal(targets.large.marker, targets.typical.marker);
  assert.match(targets.large.marker, /^pn-foryou marker /);
  // The large target sizes first: one sizing turn, then its marker turn; the typical target carries no sizing.
  const largeTurns = api.calls.filter((call) => call[0] === 'send' && call[1] === 'large');
  assert.match(largeTurns[0][2], /^pn-foryou size /);
  assert.match(largeTurns[1][2], /^pn-foryou marker /);
  assert.equal(api.calls.filter((call) => call[0] === 'send' && call[1] === 'typical').length, 1);
  // Each target's tail fires its trigger turns, which the wait for idle follows.
  assert.equal(api.calls.filter((call) => call[0] === 'trigger' && call[1] === 'large').length, 23);
  assert.equal(api.calls.filter((call) => call[0] === 'trigger' && call[1] === 'typical').length, 23);
});

test('a first screen that already holds a For-you row fails the seed', async () => {
  const api = fakeApi({ roles: ['assistant', 'user'], tailLines: 10 });
  await assert.rejects(seedForYouTargets(api, SESSIONS), /first screen holds .* For-you rows/);
});

test('a trigger tail that reaches past the For-you scan bound fails the seed', async () => {
  const api = fakeApi({ roles: ['assistant', 'scheduled_trigger'], tailLines: 1600 });
  await assert.rejects(seedForYouTargets(api, SESSIONS), /trigger tail spans 1600 event lines/);
});
