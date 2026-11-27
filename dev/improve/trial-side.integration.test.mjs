// Tests of dev/improve/trial-side.sh beside the bench lock: a lockless side never waits for the lock and never
// refuses to run because of its holder. A holder read that fails is recorded as unreadable, with the reader's reason,
// and the side's exit code stays its command's. The bench lock is a scratch file, so no case takes the host's lock.
// A test file's name ends in .test.mjs, so the bench-lock naming check in dev/poor-network/lock.test.mjs reads it as
// a test.

import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';

const IMPROVE_DIR = path.dirname(fileURLToPath(import.meta.url));
const TRIAL_SIDE = path.join(IMPROVE_DIR, 'trial-side.sh');
const LOCK = path.join(path.dirname(IMPROVE_DIR), 'poor-network', 'lock.sh');
const LOCK_FILE = 'pn-runs/.bench.lock';
const REAL_GREP = execFileSync('bash', ['-c', 'command -v grep'], { encoding: 'utf8' }).trim();
// The round direction these sides name. Its marker root is this run's own /tmp/improve-<direction>.
const DIRECTION = `trialsidetest_${process.pid}`;
const MARKER_ROOT = `/tmp/improve-${DIRECTION}`;
after(() => fs.rmSync(MARKER_ROOT, { recursive: true, force: true }));

function scratch(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// The environment of a process whose bench lock is the scratch file, so the test never reaches the host's lock.
function scratchEnv(home) {
  return { ...process.env, HOME: home, BENCH_LOCK_FILE: path.join(home, LOCK_FILE) };
}

// A directory holding a `grep` that the side's holder scan runs. It behaves like grep until BLIND_GREP_FLAG names an
// existing file; then it finds nothing, the way the scan finds nothing for a holder whose descriptor it cannot read.
function blindGrep(binDir) {
  fs.mkdirSync(binDir);
  const stub = path.join(binDir, 'grep');
  fs.writeFileSync(
    stub,
    [
      '#!/usr/bin/env bash',
      'if [[ -n ${BLIND_GREP_FLAG:-} && -e $BLIND_GREP_FLAG ]]; then',
      '  exit 1',
      'fi',
      `exec ${JSON.stringify(REAL_GREP)} "$@"`,
      '',
    ].join('\n'),
    { mode: 0o755 },
  );
  return `${binDir}:${process.env.PATH}`;
}

// One lockless side: dev/improve/trial-side.sh runs `bash -c <body>` under a scratch home and evidence directory.
function locklessSide(home, evidence, body, args, extraEnv = {}) {
  return spawnSync('bash', [TRIAL_SIDE, 'bash', '-c', body, 'bash', ...args], {
    env: {
      ...scratchEnv(home),
      IMPROVE_DIRECTION: 'ux',
      IMPROVE_TRIAL_USE_LOCK: '0',
      IMPROVE_TRIAL_LOCK_WAIT_SECONDS: '5',
      CHARLIEBOT_RELEASE_RUN_DIR: evidence,
      ...extraEnv,
    },
    encoding: 'utf8',
    timeout: 30000,
  });
}

// Starts a process that holds the bench lock through its own descriptor until `marker.release` exists. The lock
// belongs to the descriptor, so the shell outlives the flock helper that took it.
function startHolder(home, marker) {
  const keeper = path.join(path.dirname(marker), 'keeper.sh');
  fs.writeFileSync(
    keeper,
    [
      '#!/usr/bin/env bash',
      'set -euo pipefail',
      'source "$1"',
      'exec 3>"$(bench_lock_path)"',
      'bench_lock_fd_take 3 10',
      'touch "$2"',
      'while [[ ! -e "$2.release" ]]; do sleep 0.05; done',
      '',
    ].join('\n'),
    { mode: 0o755 },
  );
  const child = spawn('bash', [keeper, LOCK, marker], { env: scratchEnv(home), stdio: 'ignore' });
  return { child, exited: once(child, 'exit') };
}

// The pids /proc/locks lists as holding a file lock on `file`: fields are id, kind, advisory, mode, pid, device:inode.
function heldLockPids(file) {
  const inode = String(fs.statSync(file).ino);
  return fs
    .readFileSync('/proc/locks', 'utf8')
    .split('\n')
    .flatMap((line) => {
      const fields = line.trim().split(/\s+/);
      return fields[1] === 'FLOCK' && fields[5]?.endsWith(`:${inode}`) ? [Number(fields[4])] : [];
    });
}

// The start time of a process: field 22 of /proc/<pid>/stat, in clock ticks since boot.
function startTicks(pid) {
  const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
  // Field 2 is the command name in parentheses; field 3, the state, follows its last ")".
  const fields = stat.slice(stat.lastIndexOf(') ') + 2).split(' ');
  return Number(fields[19]);
}

// The lines of the side's one bench-lock record.
function benchLockLines(evidence) {
  const records = fs.readdirSync(evidence).filter((name) => name.startsWith('bench-lock-'));
  assert.equal(records.length, 1, `expected one bench-lock record in ${evidence}, found ${records.length}`);
  return fs.readFileSync(path.join(evidence, records[0]), 'utf8').trim().split('\n');
}

// The pid of the /proc/locks holder that an unreadable-holder line names, and the reason's claim.
function unreadableHolder(line, point) {
  const prefix = ` side ${point}: bench lock holder unreadable: bench lock: /proc/locks lists pid `;
  const start = line.indexOf(prefix);
  assert.ok(start >= 0, `${line} does not record an unreadable holder at the side ${point}`);
  const reason = line.slice(start + prefix.length);
  const match = /^(\d+) as the holder, but no process holds the descriptor of (.+)$/.exec(reason);
  assert.ok(match, line);
  return { pid: Number(match[1]), descriptor: match[2] };
}

async function waitFor(predicate, timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) {
      throw new Error('the condition did not hold in time');
    }
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
}

test('a lockless side records an unreadable holder at both points and runs its command', {
  timeout: 60000,
}, async () => {
  const home = scratch('improve-trial-home-');
  const dir = scratch('improve-trial-run-');
  const marker = path.join(dir, 'held');
  const evidence = path.join(dir, 'evidence');
  const ran = path.join(dir, 'ran');
  const lockFile = path.join(home, LOCK_FILE);
  const holder = startHolder(home, marker);
  try {
    await waitFor(() => fs.existsSync(marker));
    // The scan is blind from the first read, so the side never learns the holder of the lock it runs beside.
    const holderPids = heldLockPids(lockFile);
    assert.equal(holderPids.length, 1, `expected one held lock, found ${holderPids.length}`);
    const side = locklessSide(home, evidence, 'touch "$1"; exit "$2"', [ran, '3'], {
      PATH: blindGrep(path.join(dir, 'bin')),
      BLIND_GREP_FLAG: marker,
    });
    assert.equal(side.status, 3, side.stderr);
    assert.match(side.stderr, /the bench lock holder at the side start is unreadable/);
    assert.equal(fs.existsSync(ran), true, 'the trial command did not run beside an unreadable holder');

    const [start, end] = benchLockLines(evidence);
    for (const [line, point] of [[start, 'start'], [end, 'end']]) {
      const named = unreadableHolder(line, point);
      assert.equal(named.pid, holderPids[0], `the record names another holder: ${line}`);
      assert.equal(named.descriptor, lockFile, `the record names another lock file: ${line}`);
    }
  } finally {
    fs.writeFileSync(`${marker}.release`, '');
    await holder.exited;
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a holder that becomes unreadable at the side end keeps the command\'s exit code', {
  timeout: 60000,
}, async () => {
  const home = scratch('improve-trial-home-');
  const dir = scratch('improve-trial-run-');
  const marker = path.join(dir, 'held');
  const evidence = path.join(dir, 'evidence');
  const ran = path.join(dir, 'ran');
  const flag = path.join(dir, 'blind');
  const lockFile = path.join(home, LOCK_FILE);
  const holder = startHolder(home, marker);
  try {
    await waitFor(() => fs.existsSync(marker));
    // The command makes the scan blind before it exits, so the start read names the holder and the end read cannot.
    const side = locklessSide(home, evidence, 'touch "$1"; touch "$2"; exit "$3"', [ran, flag, '4'], {
      PATH: blindGrep(path.join(dir, 'bin')),
      BLIND_GREP_FLAG: flag,
    });
    assert.equal(side.status, 4, side.stderr);
    assert.equal(fs.existsSync(ran), true, 'the trial command did not run with the lock held');

    const [start, end] = benchLockLines(evidence);
    const named = / side start: bench lock holder (\d+) start=(\d+) /.exec(start);
    assert.ok(named, `the start read did not name the holder: ${start}`);
    assert.equal(Number(named[1]), holder.child.pid, `the start record names another process: ${start}`);
    assert.equal(Number(named[2]), startTicks(holder.child.pid), `the start record names another start time: ${start}`);

    const failed = unreadableHolder(end, 'end');
    assert.equal(failed.pid, heldLockPids(lockFile)[0], `the end record names another holder: ${end}`);
    assert.equal(failed.descriptor, lockFile, `the end record names another lock file: ${end}`);
  } finally {
    fs.writeFileSync(`${marker}.release`, '');
    await holder.exited;
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});