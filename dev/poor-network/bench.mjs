#!/usr/bin/env node
// The poor-network bench's measuring process. run.sh starts it as the command of a serve trial, so the trial's server is
// the one it measures and the trial stops it afterwards. It reads the trial's CHARLIEBOT_SERVE_* variables, checks the
// served home against the mock home, runs the chosen scenarios and writes one JSON file for the side.
//
//   node bench.mjs --side before|after --out <file.json> --mock-home <dir> --mock-home-bin <path> \
//     --reply-dir <dir> --assets <dir> --commit <text> [--repetitions N] \
//     [--scenarios background,held,reset,stream,starred,threads]
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
  TARGETS,
  TASK_TREE_LOAD,
  VIEW,
} from './profile.mjs';
import { REPLY_PIECES, REPLY_SOURCES } from './reply.mjs';
import { backgroundRep, heldBlackoutRep, longStreamRep, resetBlackoutRep, viewBackgroundRep } from './scenarios.mjs';
import { formatOpen, openScenario } from './open.mjs';
import { METRICS, calibrate, formatSide, percentile, summarize } from './report.mjs';
import { TrialWatch } from './trials.mjs';

// `starred` and `threads` measure scenario 1's background with the visible tab on that sidebar view. They run only when
// named, after the six scenarios' own.
const SCENARIOS = Object.freeze({
  background: backgroundRep,
  held: heldBlackoutRep,
  reset: resetBlackoutRep,
  stream: longStreamRep,
  starred: viewBackgroundRep('starred'),
  threads: viewBackgroundRep('threads'),
});
const DEFAULT_SCENARIOS = Object.freeze(['background', 'held', 'reset', 'stream']);

// The open scenario runs its own cells, five repetitions each, in cell order, so it is not one of the per-repetition
// scenarios above. It is selected by name only: the default run keeps the four scenarios of README.md.
const OPEN_SCENARIO = 'open';

// The sessions the bench needs are created on the trial's backend option, which scripts/release/trial.sh writes as
// charlie-code-release-trial; the charlie-code binary behind it is the stub that run.sh puts on PATH. Index 1 is the
// open session, 2 the background one, 3 to 5 the probes, and from FIRST_SIZED_INDEX on the switch target and the held
// targets, which take turns until their bootstrap reaches the live view's size.
const STUB_BACKEND_ID = 'charlie-code-release-trial';
const SEED_TIMEOUT_MS = 120000;
const FIRST_SIZED_INDEX = 6;
const MAX_SIZING_TURNS = 30;

function parseArgs(argv) {
  const args = { repetitions: REPETITIONS, scenarios: DEFAULT_SCENARIOS.join(',') };
  const flags = {
    '--side': 'side',
    '--out': 'out',
    '--mock-home': 'mockHome',
    '--mock-home-bin': 'mockHomeBin',
    '--reply-dir': 'replyDir',
    '--assets': 'assets',
    '--commit': 'commit',
    '--repetitions': 'repetitions',
    '--scenarios': 'scenarios',
  };
  for (let index = 0; index < argv.length; index += 2) {
    const key = flags[argv[index]];
    if (!key) {
      throw new Error(`unknown argument ${argv[index]}`);
    }
    args[key] = argv[index + 1];
  }
  for (const required of ['side', 'out', 'mockHome', 'mockHomeBin', 'replyDir', 'assets', 'commit']) {
    if (args[required] === undefined) {
      throw new Error(`--${required.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)} is required`);
    }
  }
  if (!['before', 'after'].includes(args.side)) {
    throw new Error(`--side must be before or after, got ${args.side}`);
  }
  args.repetitions = Number(args.repetitions);
  const named = new Set(args.scenarios.split(','));
  for (const name of named) {
    if (!(name in SCENARIOS) && name !== OPEN_SCENARIO) {
      throw new Error(`unknown scenario ${name}`);
    }
  }
  // The scenarios run in SCENARIOS' order, so a view scenario's starred rows never reach the six scenarios' own. The
  // open scenario runs after them: it is not one of the per-repetition scenarios.
  args.scenarios = [...Object.keys(SCENARIOS).filter((name) => named.has(name)), ...named.has(OPEN_SCENARIO) ? [OPEN_SCENARIO] : []];
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
  const id = await api.createSession(`pn-${tag}-${index}`, STUB_BACKEND_ID);
  for (let turn = 1; ; turn++) {
    const marker = `pn-seed ${tag} ${index} ${turn}`;
    await api.sendMessage(id, marker);
    await api.waitForIdle(id, SEED_TIMEOUT_MS);
    if (targetBytes === null) {
      return { id, marker, turns: 1, bootstrapWireBytes: null };
    }
    const { untrimmedBytes } = await api.bootstrapWireBytes(id);
    if (untrimmedBytes >= targetBytes) {
      return { id, marker, turns: turn, bootstrapWireBytes: untrimmedBytes };
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
    const targetBytes = index >= FIRST_SIZED_INDEX ? VIEW.bootstrapUntrimmedWireBytes : null;
    made.push(await seedSession(api, tag, index, targetBytes));
  }
  return {
    open: made[0],
    background: made[1],
    probes: made.slice(2, 5),
    switchTarget: made[FIRST_SIZED_INDEX - 1],
    held: made.slice(FIRST_SIZED_INDEX),
  };
}

// The default "all" view as the server lists it, bench sessions included. The view must hold the live view's rows, so a
// home of another size fails here, before any scenario runs.
async function measureView(api) {
  const rows = await api.json('GET', '/api/sessions/');
  if (!Array.isArray(rows)) {
    throw new Error('GET /api/sessions/ did not answer a list of rows');
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
  args, startedAt, finishedAt, status, error, results, openResult, assets, sessions, view, otherViews, replyBytes,
  chromeVersion,
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
    repetitions: args.repetitions,
    reply: { sources: REPLY_SOURCES, pieces: REPLY_PIECES, textBytes: replyBytes },
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
      sizedTargets: [sessions.switchTarget, ...sessions.held].map(({ id, turns, bootstrapWireBytes }) => ({
        id,
        turns,
        bootstrapWireBytes,
      })),
    },
    reps: results,
    open: openResult,
    metrics: {},
    calibration: null,
  };
  if (results.background.length > 0) {
    side.calibration = calibrate(results.background);
  }
  for (const metric of METRICS) {
    const repsOf = { 1: 'background', 2: 'background', 3: 'background', 4: 'held', 5: 'reset', 6: 'stream' }[metric.scenario];
    if (results[repsOf].length > 0 && status === 'measured') {
      side.metrics[metric.id] = summarize(results[repsOf], metric);
    }
  }
  return side;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const env = requireEnv(['CHARLIEBOT_SERVE_URL', 'CHARLIEBOT_SERVE_AUTH_HEADER', 'CHARLIEBOT_SERVE_PID']);
  chromeBinary();
  const startedAt = new Date().toISOString();
  const api = new ServerApi({ url: env.CHARLIEBOT_SERVE_URL, authHeader: env.CHARLIEBOT_SERVE_AUTH_HEADER });

  // Every failure after this point still writes the side's JSON, with status failed and the error.
  const ctx = {
    api,
    sessions: null,
    view: null,
    assets: new Map(),
    replyBytes: 0,
    frontPort: null,
    trustedSpki: null,
    origin: null,
    runDir: path.dirname(args.out),
    chromeVersion: null,
    backendId: STUB_BACKEND_ID,
    otherViews: {},
  };
  const results = Object.fromEntries(Object.keys(SCENARIOS).map((name) => [name, []]));
  // The open scenario fills this object cell by cell, so a failure part-way leaves the cells measured so far.
  const openResult = { cpu: null, reference: null, cells: [], releases: 0 };
  let status = 'measured';
  let error = null;
  let front = null;
  let cdn = null;
  const trials = new TrialWatch();
  ctx.trials = trials;
  try {
    trials.start();
    const check = spawnSync(args.mockHomeBin, ['check', args.mockHome, api.base], { stdio: 'inherit' });
    if (check.status !== 0) {
      throw new Error(`mock-home check exited ${check.status}: the served home is not the mock home`);
    }
    ctx.assets = await loadAssets(args.assets);
    ctx.replyBytes = replyTextBytes(args.replyDir);
    ctx.sessions = await createSessions(api);
    ctx.view = await measureView(api);
    const serve = new URL(api.base);
    const tls = makeCertificate(path.join(ctx.runDir, `tls-${args.side}`));
    const target = { host: serve.hostname, port: Number(serve.port) };
    front = await startFront({ key: tls.key, cert: tls.cert, target });
    ctx.frontPort = front.port;
    ctx.trustedSpki = tls.spki;
    ctx.origin = `https://localhost:${front.port}`;
    if (args.scenarios.includes(OPEN_SCENARIO)) {
      const cdnTls = makeCertificate(path.join(ctx.runDir, `tls-cdn-${args.side}`));
      cdn = await startCdn({ key: cdnTls.key, cert: cdnTls.cert, assets: ctx.assets });
      ctx.cdnPort = cdn.port;
      // The browser pins both servers' keys: the front's and the CDN stand-in's. Without the stand-in's pin every
      // CDN file fails the certificate check and no CDN byte crosses the link.
      ctx.cdnSpki = cdnTls.spki;
    }
    for (const name of args.scenarios) {
      if (name === OPEN_SCENARIO) {
        await openScenario(ctx, args.repetitions, openResult);
        continue;
      }
      for (let repetition = 1; repetition <= args.repetitions; repetition++) {
        process.stderr.write(`[${args.side}] ${name} repetition ${repetition}/${args.repetitions}\n`);
        const from = trials.mark();
        const rep = await SCENARIOS[name](ctx, repetition);
        const otherTrials = trials.since(from);
        const startedAt = new Date(from).toISOString();
        results[name].push({ ...rep, startedAt, finishedAt: new Date().toISOString(), otherTrials });
      }
    }
  } catch (caught) {
    status = 'failed';
    error = caught.stack ?? String(caught);
  } finally {
    trials.stop();
    if (cdn) {
      await cdn.close();
    }
    if (front) {
      await front.close();
    }
  }

  const side = buildSide({
    args,
    startedAt,
    finishedAt: new Date().toISOString(),
    status,
    error,
    results,
    openResult,
    assets: ctx.assets,
    sessions: ctx.sessions,
    view: ctx.view,
    otherViews: ctx.otherViews,
    replyBytes: ctx.replyBytes,
    chromeVersion: ctx.chromeVersion,
  });
  fs.writeFileSync(args.out, `${JSON.stringify(side, null, 2)}\n`);
  process.stderr.write(`wrote ${args.out}\n`);
  if (status !== 'measured') {
    process.stderr.write(`${error}\n`);
    process.exitCode = 1;
    return;
  }
  process.stdout.write(`${formatSide(side)}\n`);
  if (openResult.cpu !== null) {
    process.stdout.write(`${formatOpen(openResult)}\n`);
  }
}

main().catch((caught) => {
  process.stderr.write(`${caught.stack ?? caught}\n`);
  process.exitCode = 1;
});
