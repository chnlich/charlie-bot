// The status poll counting reads both servers' answers: the baseline's whole row map and the change's
// `{token, rows, gone}`. A mistake here dresses a wrong number as a measurement, so the counting is checked against
// synthetic poll records.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { statusPollCounts } from './scenarios.mjs';

// A tab whose status polls are the given records, oldest first: `{url, status (200 or 304 as the page sees it),
// wireStatus, body}`.
function tabOf(records) {
  const requests = new Map();
  records.forEach((record, index) => {
    requests.set(String(index), {
      url: record.url,
      startAt: index,
      finishAt: index,
      bytes: record.bytes ?? 500,
      status: record.status,
      wireStatus: record.wireStatus,
      failed: null,
      body: record.body,
    });
  });
  return { requests };
}

const STATUS_URL = '/api/sessions/status?filter=all&ids=session-a';

test('a whole-map answer that repeats the state it held counts as an unchanged 200', () => {
  const body = JSON.stringify({'session-a': {has_running_tasks: false}});
  const tab = tabOf([
    {url: STATUS_URL, status: 200, wireStatus: 200, body},
    {url: STATUS_URL, status: 200, wireStatus: 200, body},
    {url: STATUS_URL, status: 304, wireStatus: 304},
  ]);
  const counts = statusPollCounts(tab, 0, 10, 1);
  assert.equal(counts.answered200, 2);
  assert.equal(counts.answered304, 1);
  assert.equal(counts.firstBodies, 1, 'the first answer names a row set the tab held no state for');
  assert.equal(counts.unchangedBodies, 1, 'the second resent the state with no row changed');
  assert.equal(counts.changedBodies, 0);
});

test('a delta answer is applied to the state the tab held for its row set', () => {
  const tab = tabOf([
    {
      url: STATUS_URL,
      status: 200,
      wireStatus: 200,
      body: JSON.stringify({token: '1-7', rows: {'session-a': {has_running_tasks: false}}, gone: []}),
    },
    {
      url: `${STATUS_URL}&since=1-7`,
      status: 200,
      wireStatus: 200,
      body: JSON.stringify({token: '1-9', rows: {'session-a': {has_running_tasks: true}}, gone: []}),
    },
    {
      url: `${STATUS_URL}&since=1-9`,
      status: 200,
      wireStatus: 200,
      bytes: 320,
      body: JSON.stringify({token: '1-10', rows: {}, gone: []}),
    },
  ]);
  const counts = statusPollCounts(tab, 0, 10, 1);
  assert.equal(counts.firstBodies, 1);
  assert.equal(counts.changedBodies, 1, 'the second answer moved a row');
  assert.deepEqual(Object.keys(counts.changedFields), ['has_running_tasks']);
  assert.equal(counts.unchangedBodies, 1, 'an empty delta repeats the state, and the token alone is not a row change');
  assert.equal(counts.bytes200, 500, 'the median of the three answers, the token-carrying one included');
});

test('an id that leaves the set counts as a removed row', () => {
  const tab = tabOf([
    {
      url: STATUS_URL,
      status: 200,
      wireStatus: 200,
      body: JSON.stringify({
        token: '1-1',
        rows: {'session-a': {has_unread: false}, 'session-b': {has_unread: false}},
        gone: [],
      }),
    },
    {
      url: `${STATUS_URL}&since=1-1`,
      status: 200,
      wireStatus: 200,
      body: JSON.stringify({token: '1-3', rows: {}, gone: ['session-b']}),
    },
  ]);
  const counts = statusPollCounts(tab, 0, 10, 1);
  assert.equal(counts.changedBodies, 1);
  assert.deepEqual(Object.keys(counts.changedFields), ['row removed']);
});
