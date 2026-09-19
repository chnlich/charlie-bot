// Collapsing an expanded worker card (workers.js toggleThreadDetail's else
// branch) must resolve: the branch may only touch state workers.js owns, and
// the final-fetch latch it once cleared lives inside sidebar/workers.js's
// IIFE closure, unreachable from here.
const vm = require('node:vm');
const assert = require('node:assert/strict');
const test = require('node:test');

const { loadToggleHarness } = require('./chat_rendering_context_stub');
const { FakeElement } = require('./fake_dom');

test('collapse clears the expand caches and resolves without rejection', async () => {
  const ctx = loadToggleHarness('workers.js', {});
  const detail = new FakeElement('div');
  const chevron = new FakeElement('span');
  // The rotate write reads .style; FakeElement carries none by default.
  chevron.style = {};
  ctx._elements.set('thread-detail-t1', detail);
  ctx._elements.set('chevron-t1', chevron);
  // Seed the expanded state the collapse branch clears: an expanded card holds
  // its thread in the loaded set and its raw event count in the count map.
  vm.runInContext('loadedThreads.add("t1"); loadedEventCounts.set("t1", 5);', ctx);

  await ctx.toggleThreadDetail('t1', 'sess-9');

  assert.equal(detail.classList.contains('hidden'), true);
  assert.equal(vm.runInContext('loadedThreads.has("t1")', ctx), false);
  assert.equal(vm.runInContext('loadedEventCounts.has("t1")', ctx), false);
});
