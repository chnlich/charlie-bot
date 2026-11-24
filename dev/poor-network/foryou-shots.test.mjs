// Tests the failed older-page request hook used to hold the retry screenshot in its failed state.

import assert from 'node:assert/strict';
import { test } from 'node:test';
import { runInNewContext } from 'node:vm';

import { failEveryOlderPage } from './foryou-shots.mjs';

test('the screenshot hook records the first failed request and keeps failing retries', async () => {
  const marker = 'target marker';
  const requestUrl = '/api/sessions/session-1/events?before=older';
  const originalCalls = [];
  const window = {
    fetch: async (input) => {
      originalCalls.push(input);
      return new Response('original');
    },
  };
  const document = {
    getElementById: (id) => id === 'messages' ? { textContent: marker } : null,
  };

  runInNewContext(failEveryOlderPage('session-1', marker), {
    Date: { now: () => 1234 },
    Response,
    document,
    window,
  });

  assert.equal(window.__pnFail.at, null);
  assert.equal(window.__pnFail.url, null);
  assert.equal((await window.fetch(requestUrl)).status, 500);
  assert.equal(window.__pnFail.at, 1234);
  assert.equal(window.__pnFail.url, requestUrl);
  assert.equal((await window.fetch(requestUrl)).status, 500);
  assert.equal(window.__pnFail.at, 1234);
  assert.equal(window.__pnFail.url, requestUrl);
  assert.equal((await window.fetch('/api/sessions/session-1/bootstrap')).status, 200);
  assert.deepEqual(originalCalls, ['/api/sessions/session-1/bootstrap']);
});
