#!/usr/bin/env node
// The poor-network bench's measuring process. run.sh starts it as the command of a serve trial, so the trial's server is
// the one it measures and the trial stops it afterwards. It reads the trial's CHARLIEBOT_SERVE_* variables, checks the
// served home against the mock home, runs the chosen scenarios and writes one JSON file for the side.
//
//   node bench.mjs --side before|after --out <file.json> --mock-home <dir> --mock-home-bin <path> \
//     --reply <reply.ndjson> --assets <dir> --commit <text> [--repetitions N] [--scenarios background,held,reset,stream]
//
// Exit: 0 when the scenarios were measured (the verdicts are in the JSON and the table); 1 when a measurement failed.

import { execFileSync, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

import { ServerApi } from './api.mjs';
import { loadAssets } from './assets.mjs';
import { chromeBinary } from './chrome.mjs';
import { startFront } from './front.mjs';
import { LINK, REPETITIONS, TARGETS, TASK_TREE_LOAD } from './profile.mjs';
import { REPLY_SOURCE } from './reply.mjs';
import { backgroundRep, heldBlackoutRep, longStreamRep, resetBlackoutRep } from './scenarios.mjs';
import { METRICS, calibrate, formatSide, summarize } from './report.mjs';

const SCENARIOS = Object.freeze({
  background: backgroundRep,
  held: heldBlackoutRep,
  reset: resetBlackoutRep,
  stream: longStreamRep,
});

// The sessions the bench needs: one open, one running in the background, three for the probes, one switch target and
// five for the held blackout. All of them are created on the trial's backend option, which scripts/release/trial.sh
// writes as charlie-code-release-trial; the charlie-code binary behind it is the stub that run.sh puts on PATH.
const NEEDED = 11;
const STUB_BACKEND_ID = 'charlie-code-release-trial';
const SEED_TIMEOUT_MS = 120000;

function parseArgs(argv) {
  const args = { repetitions: REPETITIONS, scenarios: Object.keys(SCENARIOS) };
  const flags = {
    '--side': 'side',
    '--out': 'out',
    '--mock-home': 'mockHome',
    '--mock-home-bin': 'mockHomeBin',
    '--reply': 'reply',
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
  for (const required of ['side', 'out', 'mockHome', 'mockHomeBin', 'reply', 'assets', 'commit']) {
    if (args[required] === undefined) {
      throw new Error(`--${required.replace(/[A-Z]/g, (letter) => `-${letter.toLowerCase()}`)} is required`);
    }
  }
  if (!['before', 'after'].includes(args.side)) {
    throw new Error(`--side must be before or after, got ${args.side}`);
  }
  args.repetitions = Number(args.repetitions);
  args.scenarios = args.scenarios.split(',');
  for (const name of args.scenarios) {
    if (!(name in SCENARIOS)) {
      throw new Error(`unknown scenario ${name}`);
    }
  }
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

// Creates the sessions the bench uses and gives each one stub turn. The mock home's own sessions hold no task boundary,
// so the server refuses a message to them; the bench's sessions are task-tree nodes. The seed text is each session's
// marker: unique to it, so the page shows a switch only when that session's own message is rendered.
async function createSessions(api) {
  const tag = crypto.randomBytes(4).toString('hex');
  const made = [];
  for (let index = 0; index < NEEDED; index++) {
    const id = await api.createSession(`pn-${tag}-${index + 1}`, STUB_BACKEND_ID);
    const marker = `pn-seed ${tag} ${index + 1}`;
    await api.sendMessage(id, marker);
    await api.waitForIdle(id, SEED_TIMEOUT_MS);
    made.push({ id, marker });
  }
  return {
    open: made[0],
    background: made[1],
    probes: made.slice(2, 5),
    switchTarget: made[5],
    held: made.slice(6, NEEDED),
  };
}

// The scripted reply's text bytes, summed from the NDJSON the stub prints.
function replyTextBytes(file) {
  let bytes = 0;
  for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
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

// A self-signed certificate for the TLS front. The browser accepts it under --ignore-certificate-errors, as it accepts
// the trial's certificate on Tailscale Serve's test stand-in.
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
  return { key: fs.readFileSync(key), cert: fs.readFileSync(cert) };
}

function buildSide({ args, startedAt, finishedAt, status, error, results, assets, sessions, replyBytes, chromeVersion }) {
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
    reply: { source: REPLY_SOURCE, textBytes: replyBytes },
    assets: Object.fromEntries([...assets].map(([url, asset]) => [url, asset.sha256])),
    sessions: sessions && {
      open: sessions.open.id,
      background: sessions.background.id,
      probes: sessions.probes.map((session) => session.id),
      switchTarget: sessions.switchTarget.id,
      held: sessions.held.map((session) => session.id),
    },
    reps: results,
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
    assets: new Map(),
    replyBytes: 0,
    frontPort: null,
    origin: null,
    runDir: path.dirname(args.out),
    chromeVersion: null,
  };
  const results = { background: [], held: [], reset: [], stream: [] };
  let status = 'measured';
  let error = null;
  let front = null;
  try {
    const check = spawnSync(args.mockHomeBin, ['check', args.mockHome, api.base], { stdio: 'inherit' });
    if (check.status !== 0) {
      throw new Error(`mock-home check exited ${check.status}: the served home is not the mock home`);
    }
    ctx.assets = await loadAssets(args.assets);
    ctx.replyBytes = replyTextBytes(args.reply);
    ctx.sessions = await createSessions(api);
    const serve = new URL(api.base);
    const tls = makeCertificate(path.join(ctx.runDir, `tls-${args.side}`));
    front = await startFront({ ...tls, target: { host: serve.hostname, port: Number(serve.port) } });
    ctx.frontPort = front.port;
    ctx.origin = `https://localhost:${front.port}`;
    for (const name of args.scenarios) {
      for (let repetition = 1; repetition <= args.repetitions; repetition++) {
        process.stderr.write(`[${args.side}] ${name} repetition ${repetition}/${args.repetitions}\n`);
        results[name].push(await SCENARIOS[name](ctx, repetition));
      }
    }
  } catch (caught) {
    status = 'failed';
    error = caught.stack ?? String(caught);
  } finally {
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
    assets: ctx.assets,
    sessions: ctx.sessions,
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
}

main().catch((caught) => {
  process.stderr.write(`${caught.stack ?? caught}\n`);
  process.exitCode = 1;
});
