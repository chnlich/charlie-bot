// The A/A analysis (plan 7 v6, A/A). The same commit is measured in quiet windows and in loaded windows, and every
// repetition of its before and after sides is one sample. A metric's load effect is its loaded median minus its quiet
// median; a row (one scenario's metrics) is load-safe when, for each of its metrics, the 90 % bootstrap interval of that
// effect lies inside the quiet median's band (±AA_BAND of its absolute value). The stored before side reuses only the
// rows this file marks safe (before-store.mjs reads the file this writes).
//
//   node aa.mjs --out FILE --rev R --chrome V --seed S --quiet a.json,b.json --loaded c.json,d.json
//     [--rng-seed N]

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { flagsOf } from './before-store.mjs';
import { METRICS, percentile, repsOfMetric } from './report.mjs';

// Bootstrap resamples per metric (design value).
export const AA_RESAMPLES = 10000;
// The band around the quiet median that a load effect's interval must lie inside (design value).
export const AA_BAND = 0.05;

// A seeded generator (mulberry32), so one analysis gives the same verdict every time it runs.
export function seededRandom(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function resample(values, random) {
  return Array.from({ length: values.length }, () => values[Math.floor(random() * values.length)]);
}

// One metric's load effect: the medians of its quiet and loaded values, the 90 % bootstrap interval of the difference
// of the loaded and quiet medians, the band it must lie inside, and whether it does.
export function metricVerdict(quiet, loaded, { random, resamples = AA_RESAMPLES, band = AA_BAND }) {
  if (quiet.length === 0 || loaded.length === 0) {
    throw new Error(
      `an A/A metric needs repetitions in both windows, got ${quiet.length} quiet and ${loaded.length} loaded`,
    );
  }
  const effects = [];
  for (let draw = 0; draw < resamples; draw++) {
    effects.push(percentile(resample(loaded, random), 0.5) - percentile(resample(quiet, random), 0.5));
  }
  const interval = [percentile(effects, 0.05), percentile(effects, 0.95)];
  const limit = band * Math.abs(percentile(quiet, 0.5));
  return {
    quietMedian: percentile(quiet, 0.5),
    loadedMedian: percentile(loaded, 0.5),
    n: { quiet: quiet.length, loaded: loaded.length },
    interval,
    band: [-limit, limit],
    safe: interval[0] >= -limit && interval[1] <= limit,
  };
}

// The metric values of the repetitions of one metric's scenario, pooled over the measured sides of one window kind.
function valuesOf(sides, metric) {
  const values = [];
  for (const side of sides) {
    if (side.status !== 'measured') {
      throw new Error(`an A/A side is ${side.status}, not measured; its JSON cannot count as a sample`);
    }
    values.push(...(side.reps[repsOfMetric(metric)] ?? []).map(metric.value));
  }
  return values;
}

// The verdict of every row the metrics cover: each row's metrics, and whether all of them are safe.
export function analyseAA({ quiet, loaded, random }) {
  const rows = {};
  for (const metric of METRICS) {
    const row = repsOfMetric(metric);
    const verdict = metricVerdict(valuesOf(quiet, metric), valuesOf(loaded, metric), { random });
    rows[row] ??= { safe: true, metrics: {} };
    rows[row].metrics[metric.id] = verdict;
    rows[row].safe = rows[row].safe && verdict.safe;
  }
  return rows;
}

function readSides(list) {
  return list.split(',').map((file) => JSON.parse(fs.readFileSync(file, 'utf8')));
}

function main(argv) {
  const flags = flagsOf(argv);
  for (const name of ['out', 'rev', 'chrome', 'seed', 'quiet', 'loaded']) {
    if (!flags[name]) {
      throw new Error(`--${name} is required`);
    }
  }
  const random = seededRandom(Number(flags['rng-seed'] ?? 1));
  const quiet = readSides(flags.quiet);
  const loaded = readSides(flags.loaded);
  const record = {
    rev: flags.rev,
    chrome: flags.chrome,
    seedSha256: flags.seed,
    createdAt: new Date().toISOString(),
    windows: { quiet: quiet.length, loaded: loaded.length },
    rows: analyseAA({ quiet, loaded, random }),
  };
  fs.mkdirSync(path.dirname(flags.out), { recursive: true });
  fs.writeFileSync(flags.out, `${JSON.stringify(record, null, 2)}\n`);
  for (const [row, verdict] of Object.entries(record.rows)) {
    process.stdout.write(`${row} ${verdict.safe ? 'load-safe' : 'not load-safe'}\n`);
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  main(process.argv.slice(2));
}
