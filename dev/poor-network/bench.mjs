#!/usr/bin/env node
// The poor-network bench's measuring process. run.sh starts it as the command of a serve trial, so the trial's server is
// the one it measures and the trial stops it afterwards. It reads the trial's CHARLIEBOT_SERVE_* variables, checks the
// served home against the mock home, runs the chosen scenarios and writes one JSON file for the side.
//
//   node bench.mjs --side before|after --out <file.json> --mock-home <dir> --mock-home-bin <path> \
//     --reply-dir <dir> --assets <dir> --commit <text> [--repetitions N] \
//     [--scenarios background,held,reset,stream,starred,threads] [--open-cpu-rate RATE]
//
// Exit: 0 when the scenarios were measured (the verdicts are in the JSON and the table); 1 when a measurement failed.

import { execFileSync, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

import { ServerApi } from './api.mjs';
import { loadAssets } from './assets.mjs';
import { chromeBinary } from './chrome.mjs';
import { startCdn } from './cdn.mjs';
import { startFront } from './front.mjs';
import {
  BENCH_SESSIONS,
  LINK,
  LIVE_READBACK,
  REPETITIONS,
  STATUS_CHURN,
  TARGETS,
  TASK_TREE_LOAD,
  VIEW,
} from './profile.mjs';
import { REPLY_PIECES, REPLY_SOURCES, TOOL_CALL_SPAN_SECONDS, TOOL_PAIRS, toolRowBytes } from './reply.mjs';
import {
  backgroundRep,
  heldBlackoutRep,
  longStreamRep,
  resetBlackoutRep,
  toolStreamRep,
  viewBackgroundRep,
} from './scenarios.mjs';
import { expectedTargetsFrom } from './newest-row.mjs';
import { OPEN_CONDITIONS, formatOpen, openPrepare, openProbe, openScenario } from './open.mjs';
import { RESTART_SCENARIOS } from './restart.mjs';
import { BenchGate, LOCK_TIMEOUT_EXIT, LockWaitTimeout } from './step-gate.mjs';
import { closeAllLive } from './live.mjs';
import { RateGateError, Segments, StepCapped } from './segments.mjs';
import { formatSearch, searchScenario } from './search.mjs';
import { FOR_YOU_SCENARIO, forYouScenario, seedForYouTargets, seedSparseTarget } from './foryou.mjs';
import { forYouShots } from './foryou-shots.mjs';
import { deriveSide, formatSide, percentile } from './report.mjs';
import { TrialWatch } from './trials.mjs';
import { sleep } from './tabs.mjs';

// `starred` and `threads` measure scenario 1's background with the visible tab on that sidebar view. They run only when
// named, after the six scenarios' own.
// The restart cells (restart.mjs) run only when named, like the two views above.
const SCENARIOS = Object.freeze({
  background: backgroundRep,
  held: heldBlackoutRep,
  reset: resetBlackoutRep,
  stream: longStreamRep,
  tools: toolStreamRep,
  starred: viewBackgroundRep('starred'),
  threads: viewBackgroundRep('threads'),
  ...RESTART_SCENARIOS,
});
const DEFAULT_SCENARIOS = Object.freeze(['background', 'held', 'reset', 'stream']);

// The open scenario runs its own cells, five repetitions each, in cell order, so it is not one of the per-repetition
// scenarios above. It is selected by name only: the default run keeps the four scenarios of README.md.
const OPEN_SCENARIO = 'open';
// The sidebar search view runs its own repetitions and variants (search.mjs). Like open, it is selected by name only.
const SEARCH_SCENARIO = 'search';

// `--scenario-repetitions` gives named scenarios their own repetition count, as `name=N,name=N`; the scenarios it does
// not name take `--repetitions`. A name the run does not select, or a count below one, fails the parse.
function parseScenarioRepetitions(text, scenarios) {
  const counts = new Map();
  if (text === '') return counts;
  const selected = scenarios.split(',');
  for (const pair of text.split(',')) {
    const [name, count] = pair.split('=');
    if (!selected.includes(name)) {
      throw new Error(`--scenario-repetitions names ${name}, which --scenarios does not select`);
    }
    if (!Number.isInteger(Number(count)) || Number(count) < 1) {
      throw new Error(`--scenario-repetitions ${name} needs a count of at least 1, got ${count}`);
    }
    counts.set(name, Number(count));
  }
  return counts;
}

// The sessions the bench needs are created on the trial's backend option, which scripts/release/trial.sh writes as
// charlie-code-release-trial; the charlie-code binary behind it is the stub that run.sh puts on PATH. Index 1 is the
// open session, 2 the background one, 3 to 5 the probes, and from FIRST_SIZED_INDEX on the switch target and the held
// targets, which take turns until their bootstrap reaches the live view's size. The last index is the row whose status
// churns, which the background load keeps idle in size.
const STUB_BACKEND_ID = 'charlie-code-release-trial';
const SEED_TIMEOUT_MS = 120000;
const FIRST_SIZED_INDEX = 6;
const MAX_SIZING_TURNS = 30;

function parseArgs(argv) {
  const args = {
    repetitions: REPETITIONS,
    scenarios: DEFAULT_SCENARIOS.join(','),
    openConditions: OPEN_CONDITIONS.join(','),
    openCpuRate: undefined,
    mode: 'measure',
  };
  const flags = {
    '--side': 'side',
    '--mode': 'mode',
    '--out': 'out',
    '--mock-home': 'mockHome',
    '--mock-home-bin': 'mockHomeBin',
    '--reply-dir': 'replyDir',
    '--assets': 'assets',
    '--commit': 'commit',
    '--repetitions': 'repetitions',
    '--scenario-repetitions': 'scenarioRepetitions',
    '--scenarios': 'scenarios',
    '--open-conditions': 'openConditions',
    '--open-cpu-rate': 'openCpuRate',
    '--expect-targets': 'expectTargets',
    '--lock-wait': 'lockWait',
    '--foryou-shots-dir': 'foryouShotsDir',
  };
  for (let index = 0; index < argv.length; index += 2) {
    const key = flags[argv[index]];
    if (!key) {
      throw new Error(`unknown argument ${argv[index]}`);
    }
    args[key] = argv[index + 1];
  }
  for (const required of ['side', 'out', 'mockHome', 'mockHomeBin', 'replyDir', 'assets', 'commit', 'lockWait']) {
    if (args[required] === undefined) {
      throw new Error(`--${required.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)} is required`);
    }
  }
  if (!['before', 'after'].includes(args.side)) {
    throw new Error(`--side must be before or after, got ${args.side}`);
  }
  // `seed` sizes the sessions, records their sizes in the side's JSON and stops: the seed-only check.
  if (!['measure', 'seed'].includes(args.mode)) {
    throw new Error(`--mode must be measure or seed, got ${args.mode}`);
  }
  args.repetitions = Number(args.repetitions);
  args.scenarioRepetitions = parseScenarioRepetitions(args.scenarioRepetitions ?? '', args.scenarios);
  if (!/^[1-9][0-9]*$/.test(args.lockWait)) {
    throw new Error(`--lock-wait must be a positive number of seconds, got ${args.lockWait}`);
  }
  args.lockWait = Number(args.lockWait);
  if (args.openCpuRate !== undefined) {
    args.openCpuRate = Number(args.openCpuRate);
    if (!(args.openCpuRate >= 1)) {
      throw new Error(`--open-cpu-rate must be a number of at least 1, got ${args.openCpuRate}`);
    }
  }
  const named = new Set(args.scenarios.split(','));
  for (const name of named) {
    if (!(name in SCENARIOS) && name !== FOR_YOU_SCENARIO && name !== OPEN_SCENARIO && name !== SEARCH_SCENARIO) {
      throw new Error(`unknown scenario ${name}`);
    }
  }
  args.openConditions = args.openConditions.split(',');
  for (const name of args.openConditions) {
    if (!OPEN_CONDITIONS.includes(name)) {
      throw new Error(`unknown open condition ${name}`);
    }
  }
  // The scenarios run in SCENARIOS' order, so a view scenario's starred rows never reach the six scenarios' own. The
  // open scenario runs after them: it is not one of the per-repetition scenarios.
  if (args.foryouShotsDir !== undefined && !named.has(FOR_YOU_SCENARIO)) {
    throw new Error('--foryou-shots-dir needs the foryou scenario in --scenarios');
  }
  args.scenarios = [
    ...Object.keys(SCENARIOS).filter((name) => named.has(name)),
    ...named.has(FOR_YOU_SCENARIO) ? [FOR_YOU_SCENARIO] : [],
    ...named.has(OPEN_SCENARIO) ? [OPEN_SCENARIO] : [],
    ...named.has(SEARCH_SCENARIO) ? [SEARCH_SCENARIO] : [],
  ];
  return args;
}

function requireEnv(names) {
  const values = {};
  for (const name of names) {
    const value = process.env[name];
    if (!value) {
      throw new Error(`${name} must be set (run.sh sets the trial's variables and POOR_NETWORK_CHROME)`);
    }
    values[name] = value;
  }
  return values;
}

// Creates one bench session and seeds it. A session with `targetBytes` takes turns until its bootstrap content
// reaches that many bytes on the wire untrimmed — the served body may be smaller, because the first screen's trim
// keeps it near its budget — and reports the turns it took; any other takes one turn. The mock home's own sessions
// hold no task boundary, so the server refuses a message to them; the bench's sessions are task-tree nodes. Each
// session's marker is its last turn's text, unique to it, so the page shows a switch only when that session's own
// message is rendered.
async function seedSession(api, tag, index, targetBytes) {
  const { id, group } = await api.createSession(`pn-${tag}-${index}`, STUB_BACKEND_ID);
  // A sized session's turns carry the sizing marker, which the stub answers with a sized seed reply.
  const prefix = targetBytes === null ? 'pn-seed' : 'pn-seed-sized';
  for (let turn = 1; ; turn++) {
    const marker = `${prefix} ${tag} ${index} ${turn}`;
    await api.sendMessage(id, marker);
    await api.waitForIdle(id, SEED_TIMEOUT_MS);
    if (targetBytes === null) {
      return { id, group, marker, turns: 1, bootstrapWireBytes: null };
    }
    const { untrimmedBytes } = await api.bootstrapWireBytes(id);
    if (untrimmedBytes >= targetBytes) {
      return { id, group, marker, turns: turn, bootstrapWireBytes: untrimmedBytes };
    }
    if (turn === MAX_SIZING_TURNS) {
      throw new Error(
        `session ${id} reached ${untrimmedBytes} untrimmed bootstrap bytes in ${turn} turns, short of ${targetBytes}; ` +
          await describeBootstrap(api, id),
      );
    }
  }
}

// The messages a session's bootstrap carries, by role, with their raw sizes: what a sizing that falls short reports.
async function describeBootstrap(api, sessionId) {
  const page = await api.json('GET', `/api/sessions/${sessionId}/bootstrap`);
  const roles = {};
  for (const message of page.messages) {
    roles[message.role] = (roles[message.role] ?? 0) + 1;
  }
  const sizes = page.messages.map((message) => Buffer.byteLength(JSON.stringify(message)));
  return `the bootstrap carries ${page.messages.length} messages ${JSON.stringify(roles)}, raw ${Buffer.byteLength(JSON.stringify(page))} bytes, largest message ${Math.max(...sizes)} bytes`;
}

// Creates and seeds the sessions the bench uses.
async function createSessions(api) {
  const tag = crypto.randomBytes(4).toString('hex');
  const made = [];
  for (let index = 1; index <= BENCH_SESSIONS; index++) {
    const sized = index >= FIRST_SIZED_INDEX && index < BENCH_SESSIONS - 2;
    made.push(await seedSession(api, tag, index, sized ? VIEW.bootstrapUntrimmedWireBytes : null));
  }
  return {
    open: made[0],
    background: made[1],
    probes: made.slice(2, 5),
    switchTarget: made[FIRST_SIZED_INDEX - 1],
    held: made.slice(FIRST_SIZED_INDEX, BENCH_SESSIONS - 3),
    churn: made[BENCH_SESSIONS - 3],
    // The For-you scenario's two targets: seeded plain here, sized and given their trigger tails by seedForYouTargets.
    foryouLarge: made[BENCH_SESSIONS - 2],
    foryouTypical: made[BENCH_SESSIONS - 1],
  };
}

// The scheduler binds the default cron tasks to root sessions at its first tick, 60 s after the server starts, and each
// bound session is a row of the default view (profile.mjs DEFAULT_CRON_ROWS). Until then the view holds fewer rows, so
// the count waits while it is short of VIEW.rows, up to this deadline; a count above VIEW.rows fails at once.
const VIEW_SETTLE_TIMEOUT_MS = 180000;
const VIEW_SETTLE_POLL_MS = 2000;

async function listView(api) {
  const rows = await api.json('GET', '/api/sessions/');
  if (!Array.isArray(rows)) {
    throw new Error('GET /api/sessions/ did not answer a list of rows');
  }
  return rows;
}

// The default "all" view as the server lists it, bench sessions and the default cron tasks' sessions included. The view
// must hold the live view's rows, so a home of another size fails here, before any scenario runs.
async function measureView(api) {
  const deadline = Date.now() + VIEW_SETTLE_TIMEOUT_MS;
  let rows = await listView(api);
  while (rows.length < VIEW.rows && Date.now() < deadline) {
    await sleep(VIEW_SETTLE_POLL_MS);
    rows = await listView(api);
  }
  if (rows.length !== VIEW.rows) {
    throw new Error(`the default view holds ${rows.length} rows, not the live view's ${VIEW.rows}`);
  }
  const sizes = rows.map((row) => Buffer.byteLength(JSON.stringify(row)));
  return { rows: rows.length, rawRowMedianBytes: percentile(sizes, 0.5) };
}

// The scripted reply's text bytes: the NDJSON of the first reply piece, summed. Every piece has the same count.
function replyTextBytes(dir) {
  let bytes = 0;
  for (const line of fs.readFileSync(path.join(dir, 'reply-0.ndjson'), 'utf8').split('\n')) {
    if (line.length === 0) {
      continue;
    }
    const event = JSON.parse(line);
    if (event.type === 'thought') {
      bytes += Buffer.byteLength(event.text);
    }
  }
  return bytes;
}

// A self-signed certificate for the TLS front, and the base64 SHA-256 of its public key, which Chrome trusts by name
// (launchBrowser in chrome.mjs).
function makeCertificate(dir) {
  fs.mkdirSync(dir, { recursive: true });
  const key = path.join(dir, 'key.pem');
  const cert = path.join(dir, 'cert.pem');
  execFileSync(
    'openssl',
    [
      'req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes',
      '-keyout', key, '-out', cert, '-days', '2', '-subj', '/CN=localhost',
      '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1',
    ],
    { stdio: 'ignore' },
  );
  const certPem = fs.readFileSync(cert);
  const spkiDer = new crypto.X509Certificate(certPem).publicKey.export({ type: 'spki', format: 'der' });
  return {
    key: fs.readFileSync(key),
    cert: certPem,
    spki: crypto.createHash('sha256').update(spkiDer).digest('base64'),
  };
}

function buildSide({
  args, startedAt, finishedAt, status, error, results, openResult, searchResult, assets, sessions, view, otherViews,
  replyBytes, chromeVersion, units, yields, cpuReference,
}) {
  const side = {
    side: args.side,
    commit: args.commit,
    startedAt,
    finishedAt,
    status,
    error,
    chrome: { binary: process.env.POOR_NETWORK_CHROME, version: chromeVersion },
    link: LINK,
    targets: TARGETS,
    taskTreeLoad: TASK_TREE_LOAD,
    statusChurn: STATUS_CHURN,
    repetitions: args.repetitions,
    scenarioRepetitions: Object.fromEntries(args.scenarioRepetitions),
    reply: {
      sources: REPLY_SOURCES,
      pieces: REPLY_PIECES,
      textBytes: replyBytes,
      toolPiece: { pairs: TOOL_PAIRS, callSpanSeconds: TOOL_CALL_SPAN_SECONDS, rowBytes: toolRowBytes() },
    },
    assets: Object.fromEntries([...assets].map(([url, asset]) => [url, asset.sha256])),
    view: view && { ...view, live: VIEW },
    otherViews,
    liveReadback: LIVE_READBACK,
    sessions: sessions && {
      open: sessions.open.id,
      background: sessions.background.id,
      probes: sessions.probes.map((session) => session.id),
      switchTarget: sessions.switchTarget.id,
      held: sessions.held.map((session) => session.id),
      churn: sessions.churn.id,
      // The sparse target is created in the measurement stage, so the seed-only check's record leaves it null.
      foryou: sessions.foryou && {
        large: sessions.foryou.large.id,
        typical: sessions.foryou.typical.id,
        sparse: sessions.foryou.sparse ? sessions.foryou.sparse.id : null,
        largeSizing: sessions.foryou.large.sizing,
      },
      sizedTargets: [sessions.switchTarget, ...sessions.held].map(({ id, turns, bootstrapWireBytes }) => ({
        id,
        turns,
        bootstrapWireBytes,
      })),
    },
    reps: results,
    open: openResult,
    search: searchResult,
    // Every step in run order, with its attempt, its status and, for a probe, its CPU rate; and every yield of the
    // lock.
    units,
    yields,
    cpuReference,
    metrics: {},
    calibration: null,
  };
  return deriveSide(side);
}

// The served home must be the mock home. The check runs before the bench waits for the lock: the scheduler binds the
// default cron rows at its first tick, 60 s after the server starts, and each row adds a session to the count the check
// compares, so a check after a long lock wait would fail a correct home.
function checkMockHome(args, api) {
  const check = spawnSync(args.mockHomeBin, ['check', args.mockHome, api.base], { stdio: 'inherit' });
  if (check.status !== 0) {
    throw new Error(`mock-home check exited ${check.status}: the served home is not the mock home`);
  }
}

// The session stage: the reply text and the sessions, and the For-you targets when the seed check or the For-you
// scenario needs them. Runs as one step, which is not retried, since a retry would create a second set of sessions.
async function prepareSessions(ctx, args) {
  ctx.assets = await loadAssets(args.assets);
  ctx.replyBytes = replyTextBytes(args.replyDir);
  ctx.sessions = await createSessions(ctx.api);
  if (args.mode === 'seed' || args.scenarios.includes(FOR_YOU_SCENARIO)) {
    ctx.sessions.foryou = await seedForYouTargets(ctx.api, ctx.sessions);
  }
}

// The measurement stage: the view, the For-you sparse target (created after the view is counted, so the count holds
// the bench's own sessions only), the TLS front, the CDN stand-in, and the open scenario's warm builds, which the CPU
// probes need. Runs as one step, which is not retried, since a retry would leave the first attempt's servers running.
async function prepareMeasure(ctx, args, openResult) {
  ctx.view = await measureView(ctx.api);
  if (args.scenarios.includes(FOR_YOU_SCENARIO)) {
    ctx.sessions.foryou.sparse = await seedSparseTarget(ctx.api, STUB_BACKEND_ID);
  }
  const serve = new URL(ctx.api.base);
  const tls = makeCertificate(path.join(ctx.runDir, `tls-${args.side}`));
  const target = { host: serve.hostname, port: Number(serve.port) };
  ctx.front = await startFront({ key: tls.key, cert: tls.cert, target });
  ctx.frontPort = ctx.front.port;
  ctx.trustedSpki = tls.spki;
  ctx.origin = `https://localhost:${ctx.front.port}`;
  // The browser pins both servers' keys: the front's and the CDN stand-in's. Without the stand-in's pin every CDN file
  // fails the certificate check and no CDN byte crosses the link. Every segment's probe is a page load, so the stand-in
  // always runs.
  const cdnTls = makeCertificate(path.join(ctx.runDir, `tls-cdn-${args.side}`));
  ctx.cdn = await startCdn({ key: cdnTls.key, cert: cdnTls.cert, assets: ctx.assets });
  ctx.cdnPort = ctx.cdn.port;
  ctx.cdnSpki = cdnTls.spki;
  ctx.prepared = await openPrepare(ctx, openResult);
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const env = requireEnv(['CHARLIEBOT_SERVE_URL', 'CHARLIEBOT_SERVE_AUTH_HEADER', 'CHARLIEBOT_SERVE_PID']);
  chromeBinary();
  const startedAt = new Date().toISOString();
  const api = new ServerApi({ url: env.CHARLIEBOT_SERVE_URL, authHeader: env.CHARLIEBOT_SERVE_AUTH_HEADER });

  // Every failure after this point still writes the side's JSON, with status failed and the error.
  const ctx = {
    side: args.side,
    api,
    sessions: null,
    view: null,
    assets: new Map(),
    replyBytes: 0,
    frontPort: null,
    trustedSpki: null,
    origin: null,
    front: null,
    cdn: null,
    prepared: null,
    runDir: path.dirname(args.out),
    chromeVersion: null,
    backendId: STUB_BACKEND_ID,
    otherViews: {},
    // The before side's newest-row targets, when this side runs after it in one invocation (run.sh): each repetition
    // checks its target against them before it measures.
    expectedTargets: args.expectTargets === undefined
      ? null
      : expectedTargetsFrom(JSON.parse(fs.readFileSync(args.expectTargets, 'utf8')).open),
  };
  const results = Object.fromEntries([...Object.keys(SCENARIOS), FOR_YOU_SCENARIO].map((name) => [name, []]));
  // The open scenario fills this object cell by cell, so a failure part-way leaves the cells measured so far.
  const openResult = { cpu: null, reference: null, cells: [], releases: 0 };
  // The search view fills this object once its repetitions end, so a side that fails before then leaves it null.
  let searchResult = null;
  let status = 'measured';
  let error = null;
  let exitCode = 1;
  // The steps that passed their cap twice: their scenario's repetition is missing from the results, and the side fails.
  const cappedSteps = [];
  const trials = new TrialWatch();
  ctx.trials = trials;
  const gate = new BenchGate({ lockWaitSeconds: args.lockWait });
  let segments = null;
  try {
    trials.start();
    checkMockHome(args, api);
    await gate.open();
    // The bench holds the lock one step at a time: it takes it here, gives it up after a step that a queued process
    // waits for, and takes it again. The probe that starts each segment runs inside the first step after each take.
    await gate.take();
    const sessions = await gate.step('prepare', 'the sessions and the For-you targets',
      () => prepareSessions(ctx, args), { attempts: 1 });
    if (sessions.status === 'capped') {
      throw new Error('the session stage passed its cap; the side did not start');
    }
    if (args.mode === 'seed') {
      status = 'seeded';
    } else {
      const measured = await gate.step('prepare', 'the measurement stages',
        () => prepareMeasure(ctx, args, openResult), { attempts: 1 });
      if (measured.status === 'capped') {
        throw new Error('the measurement stages passed their cap; the side did not start');
      }
    }
    segments = new Segments({
      gate,
      probe: () => openProbe(ctx, ctx.prepared.reference),
      side: args.side,
      reference: args.openCpuRate ?? null,
    });
    ctx.unit = async (kind, name, body) => {
      const outcome = await segments.unit(kind, name, body);
      if (outcome.status === 'capped') {
        throw new StepCapped(`${kind} ${name} passed its cap twice`);
      }
      return outcome.value;
    };
    for (const name of args.mode === 'seed' ? [] : args.scenarios) {
      if (name === FOR_YOU_SCENARIO) {
        await forYouScenario(ctx, args.scenarioRepetitions.get(name) ?? args.repetitions, results[name], cappedSteps);
        continue;
      }
      if (name === OPEN_SCENARIO) {
        await openScenario(ctx, args.repetitions, openResult, ctx.prepared, args.openConditions,
          args.openCpuRate ?? null);
        continue;
      }
      if (name === SEARCH_SCENARIO) {
        searchResult = await searchScenario(ctx);
        continue;
      }
      const repetitions = args.scenarioRepetitions.get(name) ?? args.repetitions;
      for (let repetition = 1; repetition <= repetitions; repetition++) {
        process.stderr.write(`[${args.side}] ${name} repetition ${repetition}/${repetitions}\n`);
        const from = trials.mark();
        let rep;
        try {
          rep = await ctx.unit('rep', `${name} ${repetition}`, () => SCENARIOS[name](ctx, repetition));
        } catch (caught) {
          if (!(caught instanceof StepCapped)) {
            throw caught;
          }
          // The other repetitions still run: a capped repetition is named and the side fails at the end.
          cappedSteps.push(caught.message);
          continue;
        }
        const otherTrials = trials.since(from);
        const startedAt = new Date(from).toISOString();
        results[name].push({ ...rep, startedAt, finishedAt: new Date().toISOString(), otherTrials });
      }
    }
    if (cappedSteps.length > 0) {
      throw new Error(`${cappedSteps.length} step(s) passed their cap twice: ${cappedSteps.join('; ')}`);
    }
    if (args.foryouShotsDir !== undefined) {
      await ctx.unit('shots', 'the For-you screenshots', () => forYouShots(ctx, args.foryouShotsDir));
      process.stderr.write(`wrote the For-you screenshots to ${args.foryouShotsDir}\n`);
    }
  } catch (caught) {
    status = 'failed';
    error = caught.stack ?? String(caught);
    exitCode = caught instanceof LockWaitTimeout ? LOCK_TIMEOUT_EXIT : (caught.exitCode ?? 1);
  } finally {
    trials.stop();
    try {
      await closeAllLive();
    } catch (caught) {
      status = 'failed';
      error = caught.stack ?? String(caught);
    }
    if (ctx.cdn) {
      await ctx.cdn.close();
    }
    if (ctx.front) {
      await ctx.front.close();
    }
    await gate.close();
  }

  const side = buildSide({
    args,
    startedAt,
    finishedAt: new Date().toISOString(),
    status,
    error,
    results,
    openResult,
    searchResult,
    assets: ctx.assets,
    sessions: ctx.sessions,
    view: ctx.view,
    otherViews: ctx.otherViews,
    replyBytes: ctx.replyBytes,
    chromeVersion: ctx.chromeVersion,
    units: gate.units,
    yields: gate.yields,
    cpuReference: segments?.reference ?? null,
  });
  fs.writeFileSync(args.out, `${JSON.stringify(side, null, 2)}\n`);
  process.stderr.write(`wrote ${args.out}\n`);
  if (status === 'failed') {
    process.stderr.write(`${error}\n`);
    process.exit(exitCode);
  }
  if (status === 'seeded') {
    process.exit(0);
  }
  // The side's JSON and its table are its whole product, and the trial waits for this process before it stops the
  // server. A socket or a Chrome handle left behind must not hold the trial open, so the process ends once the
  // table and the open scenario's line are out the door.
  await new Promise((resolve) => process.stdout.write(`${formatSide(side)}\n`, resolve));
  if (openResult.cpu !== null) {
    await new Promise((resolve) => process.stdout.write(`${formatOpen(openResult)}\n`, resolve));
  }
  if (searchResult !== null) {
    await new Promise((resolve) => process.stdout.write(`${formatSearch(searchResult)}\n`, resolve));
  }
  process.exit(0);
}

main().catch((caught) => {
  process.stderr.write(`${caught.stack ?? caught}\n`);
  process.exit(1);
});
