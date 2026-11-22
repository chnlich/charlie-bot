// Tests of `--scenarios auto`: the default four are always measured, and a path a row can move adds that row, or the
// open condition for an open cell.

import assert from 'node:assert/strict';
import { test } from 'node:test';

import { autoScenarios, DEFAULT_SCENARIOS } from './scenario-map.mjs';

test('a change that matches no row measures the default four and nothing else', () => {
  const selected = autoScenarios(['docs/store-api.md']);
  assert.deepEqual(selected.scenarios.sort(), [...DEFAULT_SCENARIOS].sort());
  assert.deepEqual(selected.openConditions, []);
});

test('a page-load change selects the open scenario with the link condition', () => {
  const selected = autoScenarios(['web/static/js/app.js']);
  assert.ok(selected.scenarios.includes('open'));
  assert.deepEqual(selected.openConditions, ['link']);
  assert.ok(DEFAULT_SCENARIOS.every((name) => selected.scenarios.includes(name)));
});

test('a socket change selects the drop conditions, and an offline-shell change the offline ones', () => {
  assert.deepEqual(autoScenarios(['web/static/js/websocket.js']).openConditions.sort(), ['flaps', 'held', 'reset']);
  assert.deepEqual(autoScenarios(['web/static/js/offline-shell.js']).openConditions.sort(), ['offline', 'release']);
});

test('a task-tree change selects the starred and threads views, and a search change selects the search row', () => {
  const tasks = autoScenarios(['rust/crates/runtime/tasks/src/execution.rs']).scenarios;
  assert.ok(tasks.includes('starred') && tasks.includes('threads'));
  assert.ok(autoScenarios(['dev/poor-network/search.mjs']).scenarios.includes('search'));
});

test('the selection is the union of every changed file', () => {
  const selected = autoScenarios(['web/static/js/app.js', 'web/static/js/offline-shell.js']);
  assert.deepEqual(selected.openConditions.sort(), ['link', 'offline', 'release']);
});
