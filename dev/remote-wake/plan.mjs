// The bench's workload: one seeded plan both builds run, so the two sides see the same waits, the same job
// lengths and the same restart times.
//
// A wait is one job in the remote container plus one trigger on a trial server. The plan fixes the wait's kind
// (a remote pid, a --cmd exit file, or a Slurm job), its length, the offset from the run's start at which its
// job starts and its trigger is registered, and the restart boundaries that stop and start both servers.
//
// Lengths come from the 104 completed remote wakes of production window W2 (2026-11-01 to 2026-11-08), stored
// as plain integers in w2-waits.txt. A length is clamped to LENGTH_FLOOR_SECONDS because a job must outlive its
// own verify-on-create probe; the source list's shortest wait lasted 0.2 s, which no registration can catch.
//
// The restart boundaries are chosen so that the share of waits a boundary crosses lies in RESTART_SHARE_RANGE. A
// wait is crossed when a restart falls between its registration and its fire, and the fire is the one today's probe
// schedule predicts (probe-schedule.mjs). A boundary is a restart: the last running server stops and a fresh one
// starts from the same home. Both builds use the same boundaries.
//
// Every random choice comes from one seeded generator, so the same arguments produce the same plan.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { predictedFireSeconds } from './probe-schedule.mjs';

export const KIND_SHARES = Object.freeze({ pid: 0.71, cmd: 0.04, slurm: 0.25 });
export const RESTART_SHARE_RANGE = Object.freeze([0.2, 0.25]);
export const LENGTH_FLOOR_SECONDS = 5;
export const LENGTH_CAP_SECONDS = 7200;
// The longest probe interval in force today, plus the 10 s of probe noise. The last segment outlives it so a
// wake whose job ended just before the segment's end still fires before the bench stops the server.
export const SETTLE_SECONDS = 700;
// The gap between the last registration and the fault window, so a verify-on-create probe still in flight is not caught.
export { FAULT_LEAD_SECONDS };

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const W2_WAITS_FILE = path.join(HERE, 'w2-waits.txt');

// A small seeded generator. Its whole state is one 32-bit integer, so a seed reproduces a plan exactly.
function generator(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = Math.imul(state ^ (state >>> 15), 1 | state);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function shuffle(items, random) {
  const out = [...items];
  for (let index = out.length - 1; index > 0; index--) {
    const swap = Math.floor(random() * (index + 1));
    [out[index], out[swap]] = [out[swap], out[index]];
  }
  return out;
}

export function loadW2Waits(file = W2_WAITS_FILE) {
  const values = fs
    .readFileSync(file, 'utf8')
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => Number(line));
  for (const value of values) {
    if (!Number.isInteger(value) || value < 0) {
      throw new Error(`${file} holds a value that is not a whole number of seconds: ${value}`);
    }
  }
  return values;
}

// The kind counts follow KIND_SHARES by largest remainder, so the counts always sum to the wait count. A plan with at
// least one wait per kind gives every kind a wait, because a kind absent from the list is a path the run never measures.
export function kindCounts(waits) {
  const shares = Object.entries(KIND_SHARES);
  const exact = shares.map(([kind, share]) => ({ kind, value: share * waits }));
  const counts = exact.map(({ kind, value }) => ({ kind, count: Math.floor(value), remainder: value - Math.floor(value) }));
  let left = waits - counts.reduce((sum, item) => sum + item.count, 0);
  const order = [...counts].sort((a, b) => b.remainder - a.remainder || a.kind.localeCompare(b.kind));
  for (const item of order) {
    if (left === 0) {
      break;
    }
    item.count += 1;
    left -= 1;
  }
  if (waits >= counts.length) {
    for (const item of counts) {
      if (item.count > 0) {
        continue;
      }
      const surplus = (item) => item.count - KIND_SHARES[item.kind] * waits;
      const donor = [...counts].sort((a, b) => surplus(b) - surplus(a))[0];
      donor.count -= 1;
      item.count = 1;
    }
  }
  return Object.fromEntries(counts.map(({ kind, count }) => [kind, count]));
}

export function pickLengths(pool, waits, random, cap = LENGTH_CAP_SECONDS) {
  if (pool.length >= waits) {
    return shuffle(pool, random)
      .slice(0, waits)
      .map((value) => clampLength(value, cap));
  }
  return Array.from({ length: waits }, () => clampLength(pool[Math.floor(random() * pool.length)], cap));
}

function clampLength(value, cap) {
  return Math.max(LENGTH_FLOOR_SECONDS, Math.min(cap, value));
}

function predictedFire(wait) {
  return predictedFireSeconds(wait.startOffset, wait.startOffset + wait.length);
}

// The waits a restart at `offset` crosses: it falls after their registration and before their predicted fire.
export function crossingAt(waits, offset) {
  return waits.filter((wait) => wait.startOffset < offset && offset < predictedFire(wait));
}

// Choose `restarts` boundaries whose crossing share lies in RESTART_SHARE_RANGE, closest first, each at least
// MIN_RESTART_SEPARATION_SECONDS after the one before it. A candidate needs at least MIN_RESTART_IN_FLIGHT crossed
// waits and lies at least MIN_RESTART_DELAY_SECONDS after the plan's first start, so a boundary never restarts two
// idle servers and never falls inside the first few registrations.
const MIN_RESTART_SEPARATION_SECONDS = 300;
const BOUNDARY_STEP_SECONDS = 15;
const MIN_RESTART_IN_FLIGHT = 2;
const MIN_RESTART_DELAY_SECONDS = 60;

export function chooseRestarts(
  waits,
  restarts,
  target,
  firstRestartOffset = MIN_RESTART_DELAY_SECONDS,
  excludeRanges = [],
) {
  if (restarts === 0) {
    return { boundaries: [], share: 0 };
  }
  const earliest = Math.min(...waits.map((wait) => wait.startOffset));
  const latest = Math.max(...waits.map((wait) => predictedFire(wait)));
  const inFlightMin = Math.min(MIN_RESTART_IN_FLIGHT, waits.length);
  const candidates = [];
  for (
    let offset = earliest + Math.max(MIN_RESTART_DELAY_SECONDS, firstRestartOffset);
    offset < latest;
    offset += BOUNDARY_STEP_SECONDS
  ) {
    if (excludeRanges.some(([from, to]) => offset >= from && offset <= to)) {
      continue;
    }
    const crossed = crossingAt(waits, offset).map((wait) => wait.index);
    if (crossed.length >= inFlightMin) {
      candidates.push({ offset, crossed, score: Math.abs(crossed.length / waits.length - target) });
    }
  }
  const chosen = [];
  const crossedSoFar = new Set();
  for (let round = 0; round < restarts; round++) {
    let best = null;
    for (const candidate of candidates) {
      if (chosen.some((boundary) => Math.abs(boundary - candidate.offset) < MIN_RESTART_SEPARATION_SECONDS)) {
        continue;
      }
      const union = new Set([...crossedSoFar, ...candidate.crossed]);
      const score = Math.abs(union.size / waits.length - target);
      if (best === null || score < best.score - 1e-9) {
        best = { offset: candidate.offset, crossed: candidate.crossed, unionSize: union.size, score };
      }
    }
    if (best === null || best.unionSize / waits.length > RESTART_SHARE_RANGE[1] + 1e-9) {
      break;
    }
    chosen.push(best.offset);
    for (const index of best.crossed) {
      crossedSoFar.add(index);
    }
  }
  chosen.sort((a, b) => a - b);
  return { boundaries: chosen, share: crossedSoFar.size / waits.length };
}

// Build one plan. `options`: waits, seed, staggerSeconds, restarts, faults (array of 'cut'/'hold'),
// cutSeconds, holdSeconds, lengthCap.
export function buildPlan(options) {
  const waitsCount = options.waits;
  const random = generator(options.seed);
  const pool = loadW2Waits();
  const lengths = pickLengths(pool, waitsCount, random, options.lengthCap ?? LENGTH_CAP_SECONDS);
  const counts = kindCounts(waitsCount);
  const kinds = shuffle(
    Object.entries(counts).flatMap(([kind, count]) => Array.from({ length: count }, () => kind)),
    random,
  );
  const spacing = options.staggerSeconds / waitsCount;
  const waits = Array.from({ length: waitsCount }, (_, index) => ({
    index,
    kind: kinds[index],
    length: lengths[index],
    startOffset: Math.floor(index * spacing),
    maxWait: lengths[index] + 3600,
    jobId: 100000 + index,
  }));

  // The faults start after the last restart and must land inside a running job, so no boundary falls within the fault
  // lead of the last job's end.
  const latestEnd = Math.max(...waits.map((wait) => wait.startOffset + wait.length));
  const restarts = chooseRestarts(waits, options.restarts, 0.225, Math.floor(options.staggerSeconds / 4), [
    [latestEnd - FAULT_LEAD_SECONDS, Infinity],
  ]);
  const segments = [];
  const bounds = [0, ...restarts.boundaries];
  for (let index = 0; index < bounds.length; index++) {
    const startOffset = bounds[index];
    const last = index === bounds.length - 1;
    const endOffset = last
      ? Math.max(...waits.map((wait) => wait.startOffset + wait.length)) + SETTLE_SECONDS
      : bounds[index + 1];
    segments.push({ index, startOffset, endOffset });
  }

  // The faults run after the last restart, so a rule never crosses a segment boundary, and inside a wait that is
  // still running then. A wait whose job ends before the restart completes its wake on the old server's schedule.
  const lastRestart = restarts.boundaries.length === 0 ? 0 : Math.max(...restarts.boundaries);
  const faultStart = Math.max(...waits.map((wait) => wait.startOffset), lastRestart) + FAULT_LEAD_SECONDS;
  const faults = chooseFaults(waits, { ...options, faultStart });
  return {
    seed: options.seed,
    waits,
    kinds,
    restarts: restarts.boundaries,
    restartShare: restarts.share,
    segments,
    faults,
  };
}

// Both faults start together, after every registration and inside a wait that is still running. A registration needs a
// fresh ssh connection, so a fault must not overlap one. The cut and the new-connection hold overlap on purpose: the
// hold stands in for the approval wait that a restart's fresh connection meets, and the cut stands in for the link
// dropping under the established connection. The restart boundaries are chosen outside the fault window, so a rule
// never outlives the segment that installed it.
const FAULT_LEAD_SECONDS = 20;

function chooseFaults(waits, options) {
  const wanted = options.faults ?? [];
  if (wanted.length === 0) {
    return [];
  }
  const startOffset = options.faultStart;
  const running = waits
    .filter((wait) => wait.startOffset < startOffset && startOffset < wait.startOffset + wait.length)
    .sort((a, b) => b.length - a.length || a.index - b.index);
  const fallback = [...waits].sort((a, b) => b.length - a.length || a.index - b.index);
  return wanted.map((kind) => {
    const wait = running[0] ?? fallback[0];
    return {
      kind,
      waitIndex: wait.index,
      startOffset,
      seconds: kind === 'cut' ? options.cutSeconds : options.holdSeconds,
      owner: 'before',
    };
  });
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = new Map();
  for (let index = 2; index < process.argv.length; index += 2) {
    args.set(process.argv[index], process.argv[index + 1]);
  }
  const plan = buildPlan({
    waits: Number(args.get('--waits') ?? 100),
    seed: Number(args.get('--seed') ?? 20261108),
    staggerSeconds: Number(args.get('--stagger-seconds') ?? 3600),
    restarts: Number(args.get('--restarts') ?? 3),
    faults: (args.get('--faults') ?? 'cut,hold').split(',').filter((item) => item.length > 0 && item !== 'none'),
    cutSeconds: Number(args.get('--cut-seconds') ?? 60),
    holdSeconds: Number(args.get('--hold-seconds') ?? 300),
    lengthCap: Number(args.get('--length-cap') ?? LENGTH_CAP_SECONDS),
  });
  process.stdout.write(`${JSON.stringify(plan, null, 2)}\n`);
}
