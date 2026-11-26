// The stored before side (plan 4.1, 4.2). A before side is the reference commit R's measurement of each row. A row is
// one scenario (background, held, ...) or one open condition (open:link, ...), stored as one file per row under
// $HOME/pn-runs/before-store/<R>/. A stored row is reused only when four things match the run that wants it: R, the
// Chrome version, the seed's sha256 and the bench's own tree (dev/poor-network's git tree), and the row is load-safe in
// the A/A result (aa/load-safe.json). The row file also names whether it failed its target on R.
//
//   node before-store.mjs paths                          print the default store and load-safe paths as JSON
//   node before-store.mjs pick <root>                    print the newest stored R within the window, or nothing
//   node before-store.mjs plan --rev R --chrome V --seed S --tree T --scenarios a,b [--open-conditions c,d]
//                              --stale report|measure [--root D] [--loadsafe F]
//                                                        print each row's decision, the stored files to reuse, and
//                                                        the open rate they ran at
//   node before-store.mjs save --rev R --chrome V --seed S --tree T --rows a,b --side FILE [--root D] [--loadsafe F]
//                                                        store the measured before side's load-safe rows
//   node before-store.mjs assemble --out FILE [--measured FILE] [--template FILE] [--stored f1,f2]
//                                                        write the before side of measured and stored rows; the
//                                                        template (the after side's JSON) stands in when none measured

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { METRICS, deriveSide, repsOfMetric } from './report.mjs';

// The window in which a stored R is the reference for a new run (design value: the main that a day's runs measure).
export const STORE_WINDOW_MS = 24 * 60 * 60 * 1000;

// Row names: `open:` rows are the open scenario's cells, and `open:reference` is its reference cell.
const OPEN_PREFIX = 'open:';
const REFERENCE_ROW = 'open:reference';
// Scenarios the store keeps as rows. `search` is measured on every run, so it is never stored.
const UNSTORED = new Set(['open', 'search']);

function fileNameOf(row) {
  return `${row.replace(/[^A-Za-z0-9._-]/g, '_')}.json`;
}

// The rows a selection measures: each stored scenario, the open reference when open runs, and each open condition.
export function rowsOf(scenarios, openConditions) {
  const rows = scenarios.filter((name) => !UNSTORED.has(name));
  if (openConditions.length > 0) {
    rows.push(REFERENCE_ROW);
    for (const condition of openConditions) {
      rows.push(`${OPEN_PREFIX}${condition}`);
    }
  }
  return rows;
}

export function sameKey(left, right) {
  return left.rev === right.rev && left.chrome === right.chrome && left.seedSha256 === right.seedSha256
    && left.benchTree === right.benchTree;
}

// The reuse rule. A row that is not load-safe is measured on both sides of every run. A stored row whose key differs
// from the run's is measured again and overwritten. A stored row that failed its target on R is stale: the run takes
// the newest origin/main as its R. Any other stored row is reused.
export function decideRow({ row, key, stored, loadSafe }) {
  if (!loadSafe.has(row)) {
    return 'measure';
  }
  if (stored === null || !sameKey(stored.key, key)) {
    return 'measure';
  }
  if (stored.failsTargets) {
    return 'stale';
  }
  return 'reuse';
}

// The decisions for a run's rows, and the stored files it reuses. The open group (the reference cell and the open
// conditions) is measured or reused as one: the bench runs the reference cell with every open cell, so a measured
// member measures the group. `stale` is 'report', which keeps a stale row's decision, or 'measure', which measures
// it: the run asks for 'measure' after it has moved to a newer R, where the stale row is measured again.
// `openRate` is the CPU rate the first reused open row ran at, or null.
export function planRows({ root, key, rows, loadSafe, stale }) {
  if (stale !== 'report' && stale !== 'measure') {
    throw new Error(`--stale must be report or measure, got ${stale}`);
  }
  const stored = new Map();
  const decisions = {};
  for (const row of rows) {
    stored.set(row, readRow(root, key.rev, row));
    const decision = decideRow({ row, key, stored: stored.get(row), loadSafe });
    decisions[row] = decision === 'stale' && stale === 'measure' ? 'measure' : decision;
  }
  const openRows = rows.filter((row) => row === REFERENCE_ROW || row.startsWith(OPEN_PREFIX));
  if (openRows.some((row) => decisions[row] === 'measure')) {
    for (const row of openRows.filter((name) => decisions[name] === 'reuse')) {
      decisions[row] = 'measure';
    }
  }
  const reused = rows.filter((row) => decisions[row] === 'reuse');
  const openRate = reused.map((row) => stored.get(row).openRate).find((rate) => rate !== null) ?? null;
  return {
    decisions,
    reuse: reused.map((row) => path.join(root, key.rev, fileNameOf(row))),
    openRate,
  };
}

// The newest R stored within the window, or null when none is. The window is measured from each R's creation.
export function pickRev(root, now = Date.now()) {
  if (!fs.existsSync(root)) {
    return null;
  }
  let best = null;
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    if (!entry.isDirectory()) {
      continue;
    }
    const meta = JSON.parse(fs.readFileSync(path.join(root, entry.name, 'rev.json'), 'utf8'));
    const created = Date.parse(meta.createdAt);
    if (now - created <= STORE_WINDOW_MS && (best === null || created > best.created)) {
      best = { rev: meta.rev, created };
    }
  }
  return best === null ? null : best.rev;
}

// The load-safe rows the A/A result names: a row is load-safe when its verdict says so.
export function loadSafeRows(file) {
  if (!fs.existsSync(file)) {
    return new Set();
  }
  const { rows } = JSON.parse(fs.readFileSync(file, 'utf8'));
  return new Set(Object.entries(rows).filter(([, verdict]) => verdict.safe === true).map(([row]) => row));
}

function readRow(root, rev, row) {
  const file = path.join(root, rev, fileNameOf(row));
  return fs.existsSync(file) ? JSON.parse(fs.readFileSync(file, 'utf8')) : null;
}

// The probe medians of the units that measured a row. A rep's unit is named `<row> <repetition>`; an open cell's
// unit is named `<cell id> <repetition>`, where the cell's condition names the row.
function probeMediansOf(side, row) {
  const matches = row === REFERENCE_ROW
    ? (unit) => unit.name.startsWith('reference ')
    : row.startsWith(OPEN_PREFIX)
      ? (unit) => unit.kind === 'open' && cellConditionOf(unit.name) === row.slice(OPEN_PREFIX.length)
      : (unit) => unit.kind === 'rep' && unit.name.startsWith(`${row} `);
  return side.units.filter((unit) => matches(unit) && unit.probeMedian !== undefined)
    .map((unit) => unit.probeMedian);
}

// The condition of an open cell from its unit name: `A link 3` is the link condition. The reference cell's name is
// `reference A link rate 1 <n>`, which the reference row takes instead.
function cellConditionOf(name) {
  return name.split(' ')[1];
}

function rowRecord(side, row, key, createdAt) {
  let reps = null;
  let cells = null;
  if (row === REFERENCE_ROW) {
    cells = [side.open.reference];
  } else if (row.startsWith(OPEN_PREFIX)) {
    const condition = row.slice(OPEN_PREFIX.length);
    cells = side.open.cells.filter((cell) => cell.condition === condition);
  } else {
    reps = side.reps[row] ?? [];
  }
  const failsTargets = METRICS.some((metric) => repsOfMetric(metric) === row
    && side.metrics[metric.id]?.pass === false);
  const capped = side.units.filter((unit) => unit.status === 'capped' && unit.name.startsWith(`${row} `)).length;
  return {
    row,
    key,
    createdAt,
    failsTargets,
    capped,
    probeMedians: probeMediansOf(side, row),
    openRate: row.startsWith(OPEN_PREFIX) || row === REFERENCE_ROW ? side.open.cpu.rate : null,
    reps,
    cells,
  };
}

// Writes the load-safe rows of a measured before side. The rows that are not load-safe are measured on every run, so
// they are not stored.
export function saveRows({ root, key, side, rows, loadSafe, now = new Date().toISOString() }) {
  const dir = path.join(root, key.rev);
  fs.mkdirSync(dir, { recursive: true });
  // The R's creation is its first save; later saves keep it, so the window counts from the first run that measured R.
  const meta = path.join(dir, 'rev.json');
  if (!fs.existsSync(meta)) {
    fs.writeFileSync(meta, `${JSON.stringify({ rev: key.rev, createdAt: now }, null, 2)}\n`);
  }
  const saved = [];
  for (const row of rows.filter((name) => loadSafe.has(name))) {
    const record = rowRecord(side, row, key, now);
    fs.writeFileSync(path.join(dir, fileNameOf(row)), `${JSON.stringify(record, null, 2)}\n`);
    saved.push(row);
  }
  return saved;
}

// The before side that a run reports: the rows it measured and the stored rows it reuses, each row's source named.
// Stored totals count each class's rows and the seconds their repetitions took; `stored` holds the records as read.
export function assembleBefore({ measured, template, stored }) {
  // The before side's own JSON is the base when it measured rows. A run that reuses every row has no before side to
  // read, so the after side's JSON is the template: it carries the same profile and shape, and the stored rows replace
  // its repetitions.
  const side = measured === null ? blankSide(template, stored) : structuredClone(measured);
  const sources = {};
  const totals = { rep: { rows: 0, seconds: 0 }, open: { rows: 0, seconds: 0 } };
  for (const [row, reps] of Object.entries(side.reps)) {
    if (reps.length > 0) {
      sources[row] = 'measured';
    }
  }
  for (const record of stored) {
    if (record.cells !== null) {
      side.open.cells.push(...record.cells);
      if (side.open.cpu === null) {
        side.open.cpu = { rate: record.openRate, rateSource: 'stored', derivedRate: record.openRate };
      }
      if (record.row === REFERENCE_ROW) {
        side.open.reference = record.cells[0];
      }
      totals.open.rows += 1;
      totals.open.seconds += seconds(record.cells.flatMap((cell) => cell.reps));
    } else {
      side.reps[record.row] = record.reps;
      totals.rep.rows += 1;
      totals.rep.seconds += seconds(record.reps);
    }
    sources[record.row] = 'stored';
  }
  side.beforeSource = { rows: sources, storedTotals: totals };
  return deriveSide(side);
}

// The after side's JSON, with the before side's fields reset to the stored rows' key (reference commit and Chrome).
function blankSide(template, stored) {
  if (stored.length === 0) {
    throw new Error('no before side to assemble: nothing was measured and nothing is stored');
  }
  const { key } = stored[0];
  const side = structuredClone(template);
  Object.assign(side, {
    side: 'before',
    commit: key.rev,
    status: 'measured',
    error: null,
    reps: {},
    units: [],
    yields: [],
    metrics: {},
    calibration: null,
    search: null,
    open: { cpu: null, reference: null, cells: [], releases: 0 },
  });
  side.chrome = { ...side.chrome, version: key.chrome };
  return side;
}

function seconds(reps) {
  return reps.reduce((sum, rep) => sum + (Date.parse(rep.finishedAt) - Date.parse(rep.startedAt)) / 1000, 0);
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

// The flags a command takes: `--name value` pairs, each named once.
export function flagsOf(argv) {
  const flags = {};
  for (let index = 0; index < argv.length; index += 2) {
    const name = argv[index];
    if (!name.startsWith('--') || argv[index + 1] === undefined) {
      throw new Error(`expected --name value pairs, got ${name}`);
    }
    flags[name.slice(2)] = argv[index + 1];
  }
  return flags;
}

// The key a run's rows are stored under, from the flags the run passes.
function keyOf(flags) {
  for (const name of ['rev', 'chrome', 'seed', 'tree']) {
    if (!flags[name]) {
      throw new Error(`--${name} is required`);
    }
  }
  return { rev: flags.rev, chrome: flags.chrome, seedSha256: flags.seed, benchTree: flags.tree };
}

function main(argv) {
  const [command, ...rest] = argv;
  const defaults = {
    root: path.join(os.homedir(), 'pn-runs', 'before-store'),
    loadSafe: path.join(os.homedir(), 'pn-runs', 'aa', 'load-safe.json'),
  };
  if (command === 'paths') {
    process.stdout.write(`${JSON.stringify(defaults)}\n`);
    return;
  }
  if (command === 'pick') {
    const rev = pickRev(rest[0] ?? defaults.root);
    if (rev !== null) {
      process.stdout.write(`${rev}\n`);
    }
    return;
  }
  const flags = flagsOf(rest);
  const root = flags.root ?? defaults.root;
  const loadSafe = loadSafeRows(flags.loadsafe ?? defaults.loadSafe);
  const list = (value) => (value ? value.split(',') : []);
  if (command === 'plan') {
    const rows = rowsOf(list(flags.scenarios), list(flags['open-conditions']));
    const plan = planRows({ root, key: keyOf(flags), rows, loadSafe, stale: flags.stale });
    process.stdout.write(`${JSON.stringify(plan)}\n`);
    return;
  }
  if (command === 'save') {
    const saved = saveRows({ root, key: keyOf(flags), side: readJson(flags.side), rows: list(flags.rows), loadSafe });
    process.stdout.write(`${JSON.stringify({ saved })}\n`);
    return;
  }
  if (command === 'assemble') {
    const stored = flags.stored ? flags.stored.split(',').map(readJson) : [];
    const measured = flags.measured ? readJson(flags.measured) : null;
    const template = flags.template ? readJson(flags.template) : null;
    const side = assembleBefore({ measured, template, stored });
    fs.writeFileSync(flags.out, `${JSON.stringify(side, null, 2)}\n`);
    return;
  }
  throw new Error(`unknown command ${command}; use paths, pick, plan, save or assemble`);
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main(process.argv.slice(2));
}
