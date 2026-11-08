// The bench's orchestrator: it runs both builds' serve trials segment by segment, restarts them at the plan's
// boundaries, reads the ground truth out of the container, and writes the report.
//
// Both builds run concurrently and start each segment at the same wall-clock moment, so the two servers meet the
// same jobs under the same host load. A restart is the gap between one segment's trial ending and the next
// segment's server answering: the next trial seeds its home from the previous segment's copied sessions, so the
// pending triggers recover exactly as they do after a production restart.
//
// The ground truth is read after the last segment: each job's own end record inside the container, and the
// container sshd's log of authenticated connections. Neither depends on the server code under test.

import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { buildReport, formatReport, rowsToTsv } from './report.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const SEGMENT_SCRIPT = path.join(HERE, 'segment.mjs');
const SEGMENT_LEAD_MS = 5000;
const START_TIMEOUT_MS = 300000;
const SEGMENT_GRACE_MS = 900000;
const PORT_RANGES = Object.freeze({ before: [18800, 18849], after: [18850, 18899] });

function parseArgs(argv) {
  const args = {};
  for (let index = 0; index < argv.length; index += 2) {
    args[argv[index].replace(/^--/, '')] = argv[index + 1];
  }
  for (const required of ['run-dir', 'container', 'gate']) {
    if (args[required] === undefined) {
      throw new Error(`--${required} is required`);
    }
  }
  return args;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function readJson(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function docker(args) {
  return execFileSync('docker', args, { encoding: 'utf8', maxBuffer: 64 * 1024 * 1024 });
}

function logTail(file, lines = 30) {
  if (!fs.existsSync(file)) {
    return `(no log at ${file})`;
  }
  return fs.readFileSync(file, 'utf8').split('\n').slice(-lines).join('\n');
}

function startTrial({ args, meta, plan, segmentIndex, build }) {
  const runDir = args['run-dir'];
  const logDir = path.join(runDir, 'trial-logs');  fs.mkdirSync(logDir, { recursive: true });
  const logFile = path.join(logDir, `${build}-${segmentIndex}.log`);
  const log = fs.openSync(logFile, 'a');
  const [portMin, portMax] = PORT_RANGES[build];
  const treeRoot = `${meta.tree_prefix}-${build[0]}${segmentIndex}`;
  fs.rmSync(treeRoot, { recursive: true, force: true });
  fs.mkdirSync(treeRoot, { recursive: true });
  const seed = segmentIndex === 0
    ? path.join(runDir, 'homes', `${build}-mock`)
    : path.join(runDir, 'seed', build, String(segmentIndex));
  const env = {
    ...process.env,
    TRIAL_PORT_MIN: String(portMin),
    TRIAL_PORT_MAX: String(portMax),
    CHARLIEBOT_RELEASE_RUN_DIR: path.join(runDir, 'trial-evidence', `${build}-${segmentIndex}`),
    REMOTE_WAKE_SSH_CONFIG: path.join(runDir, 'ssh', `${build}.conf`),
    REMOTE_WAKE_SSH_CONTROL_DIR: path.join(meta.ctl_base, build),
  };
  const commit = meta.commits[build];
  const binary = meta.binaries[build];
  const trialScript = meta.trials[build];
  const child = spawn(
    trialScript,
    [
      '--serve', '--in-caller-scope',
      '--tree-root', treeRoot,
      '--seed-home', seed,
      commit, binary,
      '--',
      process.execPath, SEGMENT_SCRIPT,
      '--run-dir', runDir,
      '--build', build,
      '--segment', String(segmentIndex),
      '--container', args.container,
      '--user', meta.ssh_users[build],
      '--alias', 'rw-remote',
      '--backend', 'charlie-code-release-trial',
    ],
    { env, stdio: ['ignore', log, log] },
  );
  const done = new Promise((resolve) => {
    child.on('exit', (code, signal) => resolve({ code, signal, logFile }));
  });
  return { child, done };
}

async function waitForFile(file, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  while (!fs.existsSync(file)) {
    if (Date.now() > deadline) {
      throw new Error(`timed out waiting for ${file}`);
    }
    await sleep(200);
  }
  return Number(fs.readFileSync(file, 'utf8'));
}

async function waitWithTimeout(promises, timeoutMs, label) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label} did not end within ${timeoutMs} ms`)), timeoutMs);
  });
  try {
    return await Promise.race([Promise.all(promises), timeout]);
  } finally {
    clearTimeout(timer);
  }
}

function groundTruth(container, users) {
  const script = [
    'for u in /run/bench/*; do',
    '  user=$(basename "$u");',
    '  for d in "$u"/jobs/*; do',
    '    [ -d "$d" ] || continue;',
    '    id=$(basename "$d");',
    '    state=$(cat "$d/state" 2>/dev/null || true);',
    '    end=$(cat "$d/end" 2>/dev/null || true);',
    '    printf "%s|%s|%s|%s\\n" "$user" "$id" "$state" "$end";',
    '  done;',
    'done',
  ].join('\n');
  const jobs = { before: {}, after: {} };
  const byUser = new Map(Object.entries(users).map(([build, user]) => [user, build]));
  for (const line of docker(['exec', container, 'sh', '-c', script]).split('\n')) {
    const [user, id, state, end] = line.trim().split('|');
    const build = byUser.get(user);
    if (!build || !id) {
      continue;
    }
    jobs[build][id] = { state: state || null, end_ms: end ? Number(end) * 1000 : null };
  }

  const connections = [];
  let sshdLog = '';
  try {
    sshdLog = docker(['exec', container, 'cat', '/var/log/bench-sshd.log']);
  } catch {
    sshdLog = '';
  }
  for (const line of sshdLog.split('\n')) {
    const match = /^(\d+(?:\.\d+)?)\s+Accepted publickey for (\S+) from \S+ port (\d+)/.exec(line);
    if (!match) {
      continue;
    }
    connections.push({ at_ms: Number(match[1]) * 1000, user: match[2], source_port: Number(match[3]) });
  }
  return { jobs, authenticated_connections: connections };
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const runDir = args['run-dir'];
  const plan = readJson(path.join(runDir, 'plan.json'));
  const meta = readJson(path.join(runDir, 'meta.json'));
  const failures = [];

  let t0 = null;
  for (let segmentIndex = 0; segmentIndex < plan.segments.length; segmentIndex++) {
    const segment = plan.segments[segmentIndex];
    if (segmentIndex > 0) {
      const due = t0 + segment.startOffset * 1000;
      if (Date.now() < due) {
        await sleep(due - Date.now());
      }
    }
    const before = startTrial({ args, meta, plan, segmentIndex, build: 'before' });
    const after = startTrial({ args, meta, plan, segmentIndex, build: 'after' });

    if (segmentIndex === 0) {
      const startedBefore = await waitForFile(path.join(runDir, 'started', 'before-0'), START_TIMEOUT_MS);
      const startedAfter = await waitForFile(path.join(runDir, 'started', 'after-0'), START_TIMEOUT_MS);
      t0 = Math.max(startedBefore, startedAfter) + SEGMENT_LEAD_MS;
      fs.writeFileSync(path.join(runDir, 't0.tmp'), String(t0));
      fs.renameSync(path.join(runDir, 't0.tmp'), path.join(runDir, 't0'));
      console.log(`remote-wake: t0=${new Date(t0).toISOString()} (both servers ready)`);
    } else {
      const readyBefore = await waitForFile(path.join(runDir, 'started', `before-${segmentIndex}`), START_TIMEOUT_MS);
      const readyAfter = await waitForFile(path.join(runDir, 'started', `after-${segmentIndex}`), START_TIMEOUT_MS);
      console.log(
        `remote-wake: restart ${segmentIndex} ready at +${((Math.max(readyBefore, readyAfter) - t0) / 1000).toFixed(0)}s`,
      );
    }

    const windowMs = (segment.endOffset - segment.startOffset) * 1000 + SEGMENT_GRACE_MS;
    let exits;
    try {
      exits = await waitWithTimeout([before.done, after.done], windowMs, `segment ${segmentIndex}`);
    } catch (error) {
      for (const trial of [before, after]) {
        trial.child.kill('SIGTERM');
      }
      await sleep(5000);
      for (const trial of [before, after]) {
        trial.child.kill('SIGKILL');
      }
      throw error;
    }
    for (const [index, exit] of exits.entries()) {
      const build = index === 0 ? 'before' : 'after';
      if (exit.code !== 0) {
        failures.push(`segment ${segmentIndex} ${build}: trial exited ${exit.code ?? exit.signal}`);
        console.error(`remote-wake: ${build} segment ${segmentIndex} failed; log tail:\n${logTail(exit.logFile)}`);
      }
    }
    if (failures.length > 0) {
      break;
    }
  }

  const truth = groundTruth(args.container, meta.ssh_users);
  truth.rtt_ms = meta.rtt_ms;
  fs.writeFileSync(path.join(runDir, 'ground-truth.json'), `${JSON.stringify(truth, null, 2)}\n`);

  const { rows, summary } = buildReport({ runDir, gate: args.gate });
  for (const failure of failures) {
    summary.gate_failures.push(failure);
  }
  summary.passed = summary.gate_failures.length === 0;
  fs.writeFileSync(path.join(runDir, 'rows.tsv'), rowsToTsv(rows));
  fs.writeFileSync(path.join(runDir, 'summary.json'), `${JSON.stringify(summary, null, 2)}\n`);
  const text = formatReport(summary);
  fs.writeFileSync(path.join(runDir, 'report.txt'), text);
  process.stdout.write(text);
  fs.writeFileSync(path.join(runDir, 'done'), summary.passed ? 'pass\n' : 'fail\n');
  return failures.length > 0 ? 1 : summary.passed ? 0 : 3;
}

main()
  .then((code) => process.exit(code))
  .catch((error) => {
    console.error(`remote-wake orchestrate: ${error.stack ?? error}`);
    process.exit(1);
  });
