// Tests of the stored before side: the reuse rule (R, Chrome, seed, bench tree and load-safety), the stale-row rule,
// the window that picks R, and the assembly of a before side from measured and stored rows.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';

import {
  assembleBefore,
  decideRow,
  loadSafeRows,
  pickRev,
  planRows,
  rowsOf,
  saveRows,
  STORE_WINDOW_MS,
} from './before-store.mjs';

const KEY = { rev: 'R1', chrome: '156.0.8078.4', seedSha256: 'seed1', benchTree: 'tree1' };
const SAFE = new Set(['background']);

function stored(overrides = {}) {
  return { key: KEY, failsTargets: false, ...overrides };
}

function scratch() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'before-store-'));
}

test('a stored row is reused only when R, Chrome, seed and bench tree all match and the row is load-safe', () => {
  assert.equal(decideRow({ row: 'background', key: KEY, stored: stored(), loadSafe: SAFE }), 'reuse');
  for (const field of ['rev', 'chrome', 'seedSha256', 'benchTree']) {
    const changed = { ...KEY, [field]: 'another' };
    assert.equal(decideRow({ row: 'background', key: changed, stored: stored(), loadSafe: SAFE }), 'measure', field);
  }
  assert.equal(decideRow({ row: 'background', key: KEY, stored: null, loadSafe: SAFE }), 'measure', 'nothing stored');
  assert.equal(decideRow({ row: 'held', key: KEY, stored: stored(), loadSafe: SAFE }), 'measure', 'not load-safe');
});

test('a capped row is stored as capped and still reused under the same key', () => {
  assert.equal(decideRow({ row: 'background', key: KEY, stored: stored({ capped: 1 }), loadSafe: SAFE }), 'reuse');
});

test('a stored row that failed its target on R is stale, not reused', () => {
  const failing = stored({ failsTargets: true });
  assert.equal(decideRow({ row: 'background', key: KEY, stored: failing, loadSafe: SAFE }), 'stale');
});

test('the newest R stored within the window is the reference; an R older than the window is not', () => {
  const root = scratch();
  const now = Date.parse('2026-11-22T12:00:00Z');
  const entries = {
    old: '2026-11-20T12:00:00Z',
    mid: '2026-11-21T20:00:00Z',
    new: '2026-11-22T06:00:00Z',
  };
  for (const [rev, createdAt] of Object.entries(entries)) {
    fs.mkdirSync(path.join(root, rev));
    fs.writeFileSync(path.join(root, rev, 'rev.json'), JSON.stringify({ rev, createdAt }));
  }
  assert.equal(pickRev(root, now), 'new');
  const later = Date.parse(entries.new) + STORE_WINDOW_MS + 1000;
  assert.equal(pickRev(root, later), null, 'nothing is inside the window later');
  assert.equal(pickRev(path.join(root, 'absent'), now), null, 'no store yet');
});

test('a missing load-safe file means no row is load-safe, and the file names the safe rows only', () => {
  const root = scratch();
  assert.equal(loadSafeRows(path.join(root, 'load-safe.json')).size, 0);
  const file = path.join(root, 'load-safe.json');
  fs.writeFileSync(file, JSON.stringify({
    rows: {
      background: { safe: true, interval: [-1, 2] },
      held: { safe: false, interval: [-9, 9] },
    },
  }));
  assert.deepEqual([...loadSafeRows(file)], ['background']);
});

test('the store keeps scenarios and open conditions as rows, never search; the reference comes with an open row', () => {
  assert.deepEqual(rowsOf(['background', 'search'], ['link']), ['background', 'open:reference', 'open:link']);
  assert.deepEqual(rowsOf(['background'], []), ['background']);
});

test('saveRows stores only the load-safe rows, marks a row that failed its target, and keeps the probe rates', () => {
  const root = scratch();
  const side = {
    units: [
      { kind: 'rep', name: 'background 1', status: 'ok', probeRate: 3.9 },
      { kind: 'rep', name: 'held 1', status: 'ok', probeRate: 3.9 },
    ],
    reps: {
      background: [{ startedAt: '2026-11-22T10:00:00Z', finishedAt: '2026-11-22T10:01:00Z' }],
      held: [{ startedAt: '2026-11-22T10:01:00Z', finishedAt: '2026-11-22T10:02:00Z' }],
    },
    metrics: { 'background.received': { pass: false } },
    open: { cpu: null, reference: null, cells: [] },
  };
  const saved = saveRows({
    root, key: KEY, side, rows: ['background', 'held'], loadSafe: SAFE, now: '2026-11-22T10:05:00Z',
  });
  assert.deepEqual(saved, ['background']);
  const heldFile = path.join(root, KEY.rev, 'held.json');
  assert.equal(fs.existsSync(heldFile), false, 'a row that is not load-safe is not stored');
  const record = JSON.parse(fs.readFileSync(path.join(root, KEY.rev, 'background.json'), 'utf8'));
  assert.equal(record.failsTargets, true);
  assert.deepEqual(record.probeRates, [3.9]);
  assert.deepEqual(record.key, KEY);
  assert.deepEqual(JSON.parse(fs.readFileSync(path.join(root, KEY.rev, 'rev.json'), 'utf8')), {
    rev: KEY.rev,
    createdAt: '2026-11-22T10:05:00Z',
  });
});

test('assembleBefore merges a stored open row beside the measured side and names each row source', () => {
  const cell = {
    id: 'A link',
    case: 'A',
    condition: 'link',
    rate: 3.2,
    reps: [{ startedAt: '2026-11-22T09:00:00Z', finishedAt: '2026-11-22T09:01:30Z' }],
  };
  const record = {
    row: 'open:link',
    key: KEY,
    failsTargets: false,
    capped: 0,
    probeRates: [3.2],
    openRate: 3.2,
    reps: null,
    cells: [cell],
  };
  const measured = {
    status: 'measured',
    reps: { background: [] },
    units: [],
    metrics: {},
    open: { cpu: null, reference: null, cells: [], releases: 0 },
  };
  const side = assembleBefore({ measured, stored: [record] });
  assert.deepEqual(side.open.cells, [cell]);
  assert.equal(side.open.cpu.rate, 3.2);
  assert.equal(side.open.cpu.rateSource, 'stored');
  assert.deepEqual(side.beforeSource.rows, { 'open:link': 'stored' });
  assert.deepEqual(side.beforeSource.storedTotals.open.rows, 1);
  assert.equal(side.beforeSource.storedTotals.open.seconds, 90);
});

test('the open group is reused only when every member is stored, and one measured member measures the group', () => {
  const root = scratch();
  const dir = path.join(root, KEY.rev);
  fs.mkdirSync(dir, { recursive: true });
  const loadSafe = new Set(['background', 'open:reference', 'open:link', 'open:held']);
  const write = (file, row) => fs.writeFileSync(path.join(dir, file), JSON.stringify(stored({ row, openRate: 3.2 })));
  write('open_reference.json', 'open:reference');
  write('open_link.json', 'open:link');
  const rows = ['background', 'open:reference', 'open:link', 'open:held'];
  const missing = planRows({ root, key: KEY, rows, loadSafe });
  assert.deepEqual(missing.decisions, {
    background: 'measure',
    'open:reference': 'measure',
    'open:link': 'measure',
    'open:held': 'measure',
  });
  assert.deepEqual(missing.reuse, []);
  assert.equal(missing.openRate, null);
  write('open_held.json', 'open:held');
  const reused = planRows({ root, key: KEY, rows: rows.slice(1), loadSafe });
  assert.deepEqual(Object.values(reused.decisions), ['reuse', 'reuse', 'reuse']);
  assert.equal(reused.reuse.length, 3);
  assert.equal(reused.openRate, 3.2);
});
