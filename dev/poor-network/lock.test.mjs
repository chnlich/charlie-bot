// Tests of the poor-network runner and bench lock. Both sides use the bench tree's trial script, and each side attempt
// takes the one file lock.sh names, so no caller option picks another file. The one option that skips the lock is an
// improve-ux trial side's IMPROVE_TRIAL_USE_LOCK=0, which records the lock's holder instead.

import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';

const BENCH_DIR = path.dirname(fileURLToPath(import.meta.url));
const DEV_DIR = path.dirname(BENCH_DIR);
const LOCK = path.join(BENCH_DIR, 'lock.sh');
const RUN = path.join(BENCH_DIR, 'run.sh');
const IMPROVE_SIDE = path.join(DEV_DIR, 'improve', 'trial-side.sh');
const LOCK_FILE = 'pn-runs/.bench.lock';
// The round direction the improve side tests name. Its marker root is this run's own /tmp/improve-<direction>.
const DIRECTION = `trialsidetest_${process.pid}`;
const MARKER_ROOT = `/tmp/improve-${DIRECTION}`;
after(() => fs.rmSync(MARKER_ROOT, { recursive: true, force: true }));

function scratch(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// The environment of a process under `home` that names no lock file: lock.sh resolves the bench lock itself.
function hostEnv(home) {
  const env = { ...process.env, HOME: home };
  delete env.BENCH_LOCK_FILE;
  return env;
}

// The environment of a process under a scratch `home` whose bench lock is the scratch file, so the test never reaches
// the host's lock.
function scratchEnv(home) {
  return { ...process.env, HOME: home, BENCH_LOCK_FILE: path.join(home, LOCK_FILE) };
}

// A variable of the lock module, read from a shell that sources the module under the given environment.
function lockVariable(env, name) {
  return execFileSync('bash', ['-c', 'source "$1"; printf "%s" "${!2}"', 'bash', LOCK, name], {
    env,
    encoding: 'utf8',
  });
}

// Every dev/ source that a reader or a script could carry the lock's name in, except the tests.
function devSources(dir) {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      return entry.name === 'node_modules' ? [] : devSources(full);
    }
    return /\.(sh|mjs|md)$/.test(entry.name) && !entry.name.endsWith('.test.mjs') ? [full] : [];
  });
}

// Runs an improve trial side under a scratch home with a short wait. Its command touches `ran`, writes the serve port
// range it saw to `ran.ports` and exits with `code`.
function improveSide(home, ran, waitSeconds, code, extraEnv = {}) {
  const command = [
    'touch "$1"',
    'printf "%s %s\\n" "${TRIAL_PORT_MIN:-unset}" "${TRIAL_PORT_MAX:-unset}" >"$1.ports"',
    'exit "$2"',
  ].join('; ');
  const env = {
    ...scratchEnv(home),
    IMPROVE_DIRECTION: DIRECTION,
    IMPROVE_TRIAL_LOCK_WAIT_SECONDS: String(waitSeconds),
  };
  delete env.IMPROVE_TRIAL_USE_LOCK;
  Object.assign(env, extraEnv);
  return spawnSync('bash', [IMPROVE_SIDE, 'bash', '-c', command, 'bash', ran, String(code)], {
    env,
    encoding: 'utf8',
    timeout: 30000,
  });
}

// Whether /proc/locks lists the process as a waiter for a file lock, a line whose lock is `-> FLOCK`.
function listedAsWaiter(pid) {
  return fs
    .readFileSync('/proc/locks', 'utf8')
    .split('\n')
    .some((line) => line.includes('-> FLOCK') && line.split(/\s+/).includes(String(pid)));
}

// The pids /proc/locks lists as holding a file lock on `file`. Fields: id, kind, advisory, mode, pid, device:inode, ...
// A blocked request's line has `->` in the kind's place, so only held locks match.
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
  // Field 2 is the command name in parentheses and may hold spaces; field 3, the state, follows its last ")".
  const fields = stat.slice(stat.lastIndexOf(') ') + 2).split(' ');
  return Number(fields[19]);
}

// The bench-lock record that a side with the lock off writes to its evidence directory: a line at start and at end.
function benchLockRecord(evidence) {
  const records = fs.readdirSync(evidence).filter((name) => name.startsWith('bench-lock-'));
  assert.equal(records.length, 1, `expected one bench-lock record in ${evidence}, found ${records.length}`);
  return fs.readFileSync(path.join(evidence, records[0]), 'utf8').trim().split('\n');
}

// Starts one side attempt: the side script runs under the bench lock and records its start and end times in `log`.
function sideAttempt(home, body, log) {
  const child = spawn('bash', ['-c', 'source "$1"; bench_lock bash "$2" "$3"', 'bash', LOCK, body, log], {
    env: scratchEnv(home),
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  return { child, exited: once(child, 'exit') };
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

function attemptEvents(log) {
  return fs
    .readFileSync(log, 'utf8')
    .trim()
    .split('\n')
    .map((line) => {
      const [kind, at] = line.split(' ');
      return { kind, at: Number(at) };
    });
}

test('the bench takes its lock under the user\'s passwd home, and only lock.sh names the file', () => {
  const home = scratch('pn-lock-home-');
  try {
    assert.equal(lockVariable(hostEnv(home), 'BENCH_LOCK_FILE'), path.join(os.userInfo().homedir, LOCK_FILE));

    const naming = devSources(DEV_DIR).filter((file) =>
      /pn-runs\/\.bench\.lock|BENCH_LOCK_FILE/.test(fs.readFileSync(file, 'utf8')),
    );
    assert.deepEqual(
      naming,
      [LOCK],
      `the lock file is named outside lock.sh in ${naming.map((file) => path.relative(DEV_DIR, file)).join(', ')}`,
    );
    const flockCalls = devSources(DEV_DIR).filter((file) => {
      const source = fs.readFileSync(file, 'utf8');
      return /pn-runs\/\.bench\.lock|BENCH_LOCK_FILE/.test(source) && /\bflock\s+-/.test(source);
    });
    assert.deepEqual(
      flockCalls,
      [LOCK],
      `the bench lock's flock call is outside lock.sh in ${
        flockCalls.map((file) => path.relative(DEV_DIR, file)).join(', ')
      }`,
    );
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('two side attempts serialize on the one lock, and the second waits for the first', { timeout: 60000 }, async () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const log = path.join(dir, 'attempts.log');
  const body = path.join(dir, 'side.sh');
  fs.writeFileSync(
    body,
    '#!/usr/bin/env bash\nset -euo pipefail\n' +
      'printf "start %s\\n" "$(date +%s%N)" >>"$1"\nsleep 2\nprintf "end %s\\n" "$(date +%s%N)" >>"$1"\n',
  );
  const first = sideAttempt(home, body, log);
  let second = null;
  try {
    // The first attempt is inside its work before the second starts, so the second meets a held lock.
    await waitFor(() => fs.existsSync(log) && fs.readFileSync(log, 'utf8').includes('start'));
    second = sideAttempt(home, body, log);
    await new Promise((resolve) => setTimeout(resolve, 500));
    assert.equal(second.child.exitCode, null, 'the second attempt ended while the first still held the lock');
    assert.equal(
      fs.readFileSync(log, 'utf8').split('start').length - 1,
      1,
      'the second attempt started while the first held the lock',
    );

    const [firstCode] = await first.exited;
    const [secondCode] = await second.exited;
    assert.equal(firstCode, 0);
    assert.equal(secondCode, 0);

    const events = attemptEvents(log);
    assert.deepEqual(events.map((event) => event.kind), ['start', 'end', 'start', 'end']);
    assert.ok(events[2].at >= events[1].at, 'the second attempt started before the first ended');
  } finally {
    first.child.kill('SIGKILL');
    second?.child.kill('SIGKILL');
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a process under a trial HOME takes the user\'s passwd-home lock, not one under HOME', () => {
  const trialHome = scratch('pn-lock-trial-');
  try {
    // The trial replaces HOME for the bench it runs; the lock stays the user's, from the passwd entry.
    assert.equal(
      lockVariable(hostEnv(trialHome), 'BENCH_LOCK_FILE'),
      path.join(os.userInfo().homedir, LOCK_FILE),
    );
  } finally {
    fs.rmSync(trialHome, { recursive: true, force: true });
  }
});

test('the bench refuses a caller-chosen lock file and takes the lock itself', { timeout: 30000 }, async () => {
  const home = scratch('pn-lock-home-');
  const chosen = path.join(home, 'chosen.lock');
  try {
    const child = spawn('bash', [RUN, '--lock', chosen], {
      env: scratchEnv(home),
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stderr = '';
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    const [code] = await once(child, 'close');
    assert.equal(code, 2, 'the bench accepted a caller-chosen lock file');
    assert.match(stderr, /--lock/);
    assert.equal(fs.existsSync(chosen), false);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('an improve side times out under the held lock without running its command', { timeout: 60000 }, async () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const held = path.join(dir, 'held');
  const ran = path.join(dir, 'ran');
  const body = path.join(dir, 'hold.sh');
  // The bench side holds the lock until the test writes the release file beside its marker.
  fs.writeFileSync(body, '#!/usr/bin/env bash\ntouch "$1"\nwhile [[ ! -e "$1.release" ]]; do sleep 0.1; done\n');
  const holder = sideAttempt(home, body, held);
  try {
    await waitFor(() => fs.existsSync(held));
    const side = improveSide(home, ran, 1, 0);
    assert.equal(side.status, Number(lockVariable(home, 'BENCH_LOCK_TIMEOUT_EXIT')), side.stderr);
    assert.match(side.stderr, /improve trial side stopped: the bench lock stayed held for 1 s/);
    assert.equal(fs.existsSync(ran), false, 'the trial command ran while the bench lock was held');
  } finally {
    fs.writeFileSync(`${held}.release`, '');
    await holder.exited;
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('an improve trial side runs its command on a free lock and passes the exit code on', { timeout: 60000 }, () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const ran = path.join(dir, 'ran');
  try {
    const side = improveSide(home, ran, 5, 3);
    assert.equal(side.status, 3, side.stderr);
    assert.equal(fs.existsSync(ran), true, 'the trial command did not run with the bench lock free');
    assert.equal(side.stderr, '');
    // The switch is unset, so the side holds the lock and serves on trial.sh's own port range.
    assert.equal(fs.readFileSync(`${ran}.ports`, 'utf8'), 'unset unset\n');

    const ran75 = path.join(dir, 'ran-75');
    const side75 = improveSide(home, ran75, 5, 75);
    assert.equal(side75.status, 75, side75.stderr);
    assert.equal(fs.existsSync(ran75), true, 'the trial command did not run with the bench lock free');
    assert.equal(side75.stderr, '', 'a command exit of 75 was reported as a lock timeout');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a lock-off improve side runs beside a held lock and records the holder', { timeout: 60000 }, async () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const held = path.join(dir, 'held');
  const ran = path.join(dir, 'ran');
  const evidence = path.join(dir, 'evidence');
  const body = path.join(dir, 'hold.sh');
  fs.writeFileSync(body, '#!/usr/bin/env bash\ntouch "$1"\nwhile [[ ! -e "$1.release" ]]; do sleep 0.1; done\n');
  const holder = sideAttempt(home, body, held);
  let waiter = null;
  try {
    await waitFor(() => fs.existsSync(held));
    // A second side queues behind the holder and /proc/locks lists it as a waiter; the record still names the holder.
    waiter = sideAttempt(home, body, path.join(dir, 'waiter'));
    await waitFor(() => listedAsWaiter(waiter.child.pid));
    // The wait is 1 s: a side that queued behind the held lock would stop with the lock timeout instead of running.
    const side = improveSide(home, ran, 1, 3, {
      IMPROVE_DIRECTION: 'ux',
      IMPROVE_TRIAL_USE_LOCK: '0',
      CHARLIEBOT_RELEASE_RUN_DIR: evidence,
    });
    assert.equal(side.status, 3, side.stderr);
    assert.equal(fs.existsSync(ran), true, 'the trial command did not run beside the held lock');
    assert.equal(fs.readFileSync(`${ran}.ports`, 'utf8'), '18670 18689\n', 'the side kept the bench port range');

    const [start, end] = benchLockRecord(evidence);
    const pid = holder.child.pid;
    assert.ok(start.includes(` side start: bench lock holder ${pid} `) && start.includes(body), start);
    assert.ok(end.includes(` side end: bench lock holder ${pid} `) && end.includes(body), end);
  } finally {
    waiter?.child.kill('SIGKILL');
    await waiter?.exited;
    fs.writeFileSync(`${held}.release`, '');
    await holder.exited;
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a lock-off side names the live holder, not the exited flock helper', { timeout: 60000 }, async () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const lock = path.join(home, LOCK_FILE);
  const marker = path.join(dir, 'taken');
  const evidence = path.join(dir, 'evidence');
  const ran = path.join(dir, 'ran');
  const keeper = path.join(dir, 'keeper.sh');
  // The step gate's holder shape: this shell opens the lock on fd 3 and takes it through bench_lock_fd_take, whose
  // flock helper exits at once. A child the shell starts inherits fd 3 too. The shell holds the lock until the test
  // releases it.
  fs.writeFileSync(
    keeper,
    [
      '#!/usr/bin/env bash',
      'set -euo pipefail',
      'source "$1"',
      'exec 3>"$(bench_lock_path)"',
      'bench_lock_fd_take 3 10',
      'sleep 300 &',
      'child=$!',
      'touch "$2"',
      'while [[ ! -e "$2.release" ]]; do sleep 0.1; done',
      'kill "$child"',
      '',
    ].join('\n'),
  );
  const holder = spawn('bash', [keeper, LOCK, marker], { env: scratchEnv(home), stdio: 'ignore' });
  const holderExited = once(holder, 'exit');
  let waiter = null;
  let waiterExited = null;
  try {
    await waitFor(() => fs.existsSync(marker));
    // /proc/locks names the flock helper, which has exited, while the shell still holds the lock.
    const helpers = heldLockPids(lock);
    assert.equal(helpers.length, 1, `expected one held lock, found ${helpers.length}`);
    assert.notEqual(helpers[0], holder.pid, '/proc/locks names the holder, not its flock helper');
    assert.equal(fs.existsSync(`/proc/${helpers[0]}`), false, 'the flock helper is still alive');

    // A second take queues behind the holder; a queued process is a waiter, never the record's holder.
    waiter = spawn('flock', ['-w', '60', lock, 'true'], { env: scratchEnv(home), stdio: 'ignore' });
    waiterExited = once(waiter, 'exit');
    await waitFor(() => listedAsWaiter(waiter.pid));
    const queued = execFileSync('bash', ['-c', 'source "$1"; bench_lock_waiter_pids', 'bash', LOCK], {
      env: scratchEnv(home),
      encoding: 'utf8',
    });
    assert.equal(queued.trim(), String(waiter.pid), 'the bench lock did not list the queued waiter');

    const side = improveSide(home, ran, 1, 3, {
      IMPROVE_DIRECTION: 'ux',
      IMPROVE_TRIAL_USE_LOCK: '0',
      CHARLIEBOT_RELEASE_RUN_DIR: evidence,
    });
    assert.equal(side.status, 3, side.stderr);
    const records = benchLockRecord(evidence);
    assert.equal(records.length, 2, `expected a start and an end record, found ${records.length}`);
    for (const record of records) {
      const named = / side (?:start|end): bench lock holder (\d+) start=(\d+) (.+)$/.exec(record);
      assert.ok(named, `the record does not name a holder with a start time: ${record}`);
      assert.equal(Number(named[1]), holder.pid, `the record names another process: ${record}`);
      assert.equal(Number(named[2]), startTicks(holder.pid), `the record's start time is not the holder's: ${record}`);
      assert.ok(named[3].includes(keeper), record);
    }
  } finally {
    waiter?.kill('SIGKILL');
    await waiterExited;
    fs.writeFileSync(`${marker}.release`, '');
    await holderExited;
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test(
  'bench_lock_holder returns none when the lock is released mid-scan, and fails when it stays held',
  { timeout: 60000 },
  async () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const binDir = path.join(dir, 'bin');
  fs.mkdirSync(binDir);
  const held = path.join(dir, 'held');
  const body = path.join(dir, 'hold.sh');
  fs.writeFileSync(body, '#!/usr/bin/env bash\ntouch "$1"\nwhile [[ ! -e "$1.release" ]]; do sleep 0.1; done\n');
  const holder = sideAttempt(home, body, held);
  try {
    await waitFor(() => fs.existsSync(held));

    // Stub grep so the scan always finds zero descriptors while the lock stays held: must fail loudly.
    const emptyGrep = path.join(binDir, 'grep');
    fs.writeFileSync(emptyGrep, '#!/usr/bin/env bash\nexit 1\n');
    fs.chmodSync(emptyGrep, 0o755);
    const heldEnv = { ...scratchEnv(home), PATH: `${binDir}:${process.env.PATH}` };
    const stillHeld = spawnSync('bash', ['-c', 'source "$1"; bench_lock_holder', 'bash', LOCK], {
      env: heldEnv,
      encoding: 'utf8',
    });
    assert.equal(stillHeld.status, 1, stillHeld.stdout);
    assert.match(stillHeld.stderr, /no process holds the descriptor/);

    // Stub grep so the holder releases the lock during the scan: must return empty with exit 0.
    const releaseGrep = path.join(dir, 'release-bin');
    fs.mkdirSync(releaseGrep);
    const releaseStub = path.join(releaseGrep, 'grep');
    const lockInode = fs.statSync(path.join(home, LOCK_FILE)).ino;
    fs.writeFileSync(
      releaseStub,
      [
        '#!/usr/bin/env bash',
        `touch "${held}.release"`,
        `while awk -v pat=":${lockInode}$" '$2 == "FLOCK" && $6 ~ pat { found = 1 } END { exit !found }' /proc/locks; do`,
        '  sleep 0.05',
        'done',
        'exit 1',
        '',
      ].join('\n'),
    );
    fs.chmodSync(releaseStub, 0o755);
    const releasedEnv = { ...scratchEnv(home), PATH: `${releaseGrep}:${process.env.PATH}` };
    const released = spawnSync('bash', ['-c', 'source "$1"; bench_lock_holder', 'bash', LOCK], {
      env: releasedEnv,
      encoding: 'utf8',
      timeout: 15000,
    });
    assert.equal(released.status, 0, released.stderr);
    assert.equal(released.stdout, '');
  } finally {
    fs.writeFileSync(`${held}.release`, '');
    await holder.exited;
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('an improve side with the bench lock off records none when the lock file is free', () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const evidence = path.join(dir, 'evidence');
  try {
    // A side with the lock on creates the lock file and releases it, so the lock file exists with no holder.
    const warm = improveSide(home, path.join(dir, 'warm'), 5, 0);
    assert.equal(warm.status, 0, warm.stderr);
    const ran = path.join(dir, 'ran');
    const side = improveSide(home, ran, 1, 0, {
      IMPROVE_DIRECTION: 'ux',
      IMPROVE_TRIAL_USE_LOCK: '0',
      CHARLIEBOT_RELEASE_RUN_DIR: evidence,
    });
    assert.equal(side.status, 0, side.stderr);
    assert.equal(fs.existsSync(ran), true, 'the trial command did not run with the lock off');
    const [start, end] = benchLockRecord(evidence);
    assert.match(start, / side start: bench lock holder none$/);
    assert.match(end, / side end: bench lock holder none$/);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a lock-off improve side refuses a direction other than ux', () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const ran = path.join(dir, 'ran');
  const evidence = path.join(dir, 'evidence');
  try {
    const side = improveSide(home, ran, 1, 0, {
      IMPROVE_TRIAL_USE_LOCK: '0',
      CHARLIEBOT_RELEASE_RUN_DIR: evidence,
    });
    assert.equal(side.status, 2, side.stderr);
    assert.match(side.stderr, /IMPROVE_TRIAL_USE_LOCK=0 requires IMPROVE_DIRECTION=ux/);
    assert.equal(fs.existsSync(ran), false, 'the trial command ran for a non-UX direction');
    assert.equal(fs.existsSync(evidence), false, 'the side created evidence for a non-UX direction');
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

// The side markers under this run's marker root: one `trial-side.<pid>.<random>` directory per side.
function sideMarkers() {
  if (!fs.existsSync(MARKER_ROOT)) {
    return [];
  }
  return fs.readdirSync(MARKER_ROOT).filter((name) => name.startsWith('trial-side.'));
}

test('an improve side keeps its marker through a cleanup of its TMPDIR during the lock wait', { timeout: 60000 }, async () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const tmp = path.join(dir, 'trial-tmp');
  fs.mkdirSync(tmp);
  const held = path.join(dir, 'held');
  const ran = path.join(dir, 'ran');
  const body = path.join(dir, 'hold.sh');
  fs.writeFileSync(body, '#!/usr/bin/env bash\ntouch "$1"\nwhile [[ ! -e "$1.release" ]]; do sleep 0.1; done\n');
  const holder = sideAttempt(home, body, held);
  let side = null;
  try {
    await waitFor(() => fs.existsSync(held));
    side = spawn('bash', [IMPROVE_SIDE, 'bash', '-c', 'touch "$1"; exit 0', 'bash', ran], {
      env: { ...scratchEnv(home), TMPDIR: tmp, IMPROVE_DIRECTION: DIRECTION, IMPROVE_TRIAL_LOCK_WAIT_SECONDS: '60' },
      stdio: ['ignore', 'ignore', 'pipe'],
    });
    let stderr = '';
    side.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    const exited = once(side, 'exit');
    await waitFor(() => sideMarkers().length === 1);
    // A trial's cleanup removes the TMPDIR that the side was started with, while the side waits for the lock.
    fs.rmSync(tmp, { recursive: true, force: true });
    fs.writeFileSync(`${held}.release`, '');
    const [code] = await exited;
    assert.equal(stderr, '', 'the side reported a problem after its TMPDIR was cleaned');
    assert.equal(code, 0);
    assert.equal(fs.existsSync(ran), true, 'the side did not run its command after the lock was released');
    assert.deepEqual(sideMarkers(), [], 'the side left its marker behind');
  } finally {
    fs.writeFileSync(`${held}.release`, '');
    await holder.exited;
    side?.kill('SIGKILL');
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a marker that a dead side left is named and removed, and it never counts as a start', { timeout: 60000 }, async () => {
  const home = scratch('pn-lock-home-');
  const dir = scratch('pn-lock-run-');
  const held = path.join(dir, 'held');
  const ran = path.join(dir, 'ran');
  const body = path.join(dir, 'hold.sh');
  fs.writeFileSync(body, '#!/usr/bin/env bash\ntouch "$1"\nwhile [[ ! -e "$1.release" ]]; do sleep 0.1; done\n');
  // The shell that prints its own pid has ended by the time the test reads it, so no process has this pid.
  const deadPid = execFileSync('bash', ['-c', 'echo $$'], { encoding: 'utf8' }).trim();
  const stale = path.join(MARKER_ROOT, `trial-side.${deadPid}.stale001`);
  fs.mkdirSync(stale, { recursive: true });
  // The stale marker says the command started. A side that trusted it would not report the lock timeout.
  fs.writeFileSync(path.join(stale, 'started'), '');
  const holder = sideAttempt(home, body, held);
  try {
    await waitFor(() => fs.existsSync(held));
    const side = improveSide(home, ran, 1, 0);
    assert.equal(side.status, Number(lockVariable(home, 'BENCH_LOCK_TIMEOUT_EXIT')), side.stderr);
    assert.match(side.stderr, /improve trial side stopped: the bench lock stayed held for 1 s/);
    assert.match(side.stderr, new RegExp(`removed the marker of dead side ${deadPid}`));
    assert.equal(fs.existsSync(stale), false, 'the dead side marker was left in place');
    assert.equal(fs.existsSync(ran), false, 'the trial command ran while the bench lock was held');
  } finally {
    fs.writeFileSync(`${held}.release`, '');
    await holder.exited;
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('both bench sides use the trial script from the bench tree', () => {
  const source = fs.readFileSync(RUN, 'utf8');
  assert.match(source, /bench_trial_script="\$tree\/scripts\/release\/trial\.sh"/);
  assert.match(source, /run_side before "\$bench_trial_script"/);
  assert.match(source, /run_side after "\$bench_trial_script"/);
});

// trial.sh's preflight refuses a socket path of 108 bytes or more. Its trial tree is <root>/trial-<12 hex>, its temp
// root is the tree's tmp, and Chrome's singleton socket sits 45 bytes below that temp root.
const SIDE_ROOT = path.join(BENCH_DIR, 'side-root.sh');
const SOCKET_BYTE_LIMIT = 108;

test('a side root keeps Chrome\'s socket path under trial.sh\'s limit for a 25-byte TMPDIR', () => {
  // A 25-byte TMPDIR under /tmp, named for this test process; os.tmpdir() can be longer than 24 bytes on a remote run.
  const tmpdir = path.join('/tmp', `pnroot${process.pid}`.padEnd(20, 'x'));
  fs.mkdirSync(tmpdir);
  try {
    assert.equal(Buffer.byteLength(tmpdir), 25);
    const env = { ...process.env, TMPDIR: tmpdir };
    const root = execFileSync('bash', [SIDE_ROOT], { encoding: 'utf8', env }).trim();
    assert.ok(root.startsWith(`${tmpdir}/pn-`), root);
    const socket = path.join(root, `trial-${'0'.repeat(12)}`, 'tmp', 'org.chromium.Chromium.XXXXXX', 'SingletonSocket');
    assert.ok(
      Buffer.byteLength(socket) < SOCKET_BYTE_LIMIT,
      `Chrome's socket path is ${Buffer.byteLength(socket)} bytes: ${socket}`,
    );
  } finally {
    fs.rmSync(tmpdir, { recursive: true, force: true });
  }
});

// run.sh names each side's commit in the bench tree before that side's trial starts, because trial.sh reads the commit
// and its origin/main merge base from that tree.
const SIDE_COMMIT = path.join(BENCH_DIR, 'side-commit.sh');

test('side-commit.sh fetches a commit the bench tree lacks from its source and names it origin/main', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'pn-side-commit-'));
  try {
    const env = {
      ...process.env,
      GIT_AUTHOR_NAME: 'bench',
      GIT_AUTHOR_EMAIL: 'bench@example.invalid',
      GIT_COMMITTER_NAME: 'bench',
      GIT_COMMITTER_EMAIL: 'bench@example.invalid',
    };
    const git = (cwd, ...args) =>
      execFileSync('git', ['-C', cwd, ...args], { encoding: 'utf8', env, stdio: ['ignore', 'pipe', 'pipe'] }).trim();
    const source = path.join(dir, 'source');
    const tree = path.join(dir, 'tree');
    fs.mkdirSync(source);
    fs.mkdirSync(tree);
    git(source, 'init', '-q');
    git(source, 'commit', '--allow-empty', '-q', '-m', 'baseline');
    const baseline = git(source, 'rev-parse', 'HEAD');
    git(tree, 'init', '-q');
    git(tree, 'commit', '--allow-empty', '-q', '-m', 'upload');
    assert.throws(() => git(tree, 'cat-file', '-e', `${baseline}^{commit}`), 'the tree starts without the baseline');

    execFileSync('bash', [SIDE_COMMIT, tree, baseline, source], { env });

    git(tree, 'cat-file', '-e', `${baseline}^{commit}`);
    assert.equal(git(tree, 'rev-parse', 'refs/remotes/origin/main'), baseline);
    assert.equal(git(tree, 'merge-base', 'origin/main', baseline), baseline);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

// trial.sh reads its probe endpoint from the live home's `backends.calls.probe` through that home's server, and the
// bench host runs no server. The bench home names one stand-in option, so run.sh hands the trial that option's answer.
test('run.sh answers the trial\'s probe with the bench home\'s stand-in endpoint', () => {
  const source = fs.readFileSync(RUN, 'utf8');
  const yaml = source.slice(source.indexOf('cat >"$live_home/config.yaml"'), source.indexOf('\nYAML\n'));
  const json = source.slice(source.indexOf('{"option"'), source.indexOf('\nJSON\n'));
  const field = (text, name) => new RegExp(`^\\s+${name}: (\\S+)$`, 'm').exec(text)[1];
  const option = { id: field(yaml, '- id'), model: field(yaml, 'model'), api_base: field(yaml, 'api_base') };
  assert.deepEqual(JSON.parse(json).option, option);
  const assigned = source.indexOf('probe_answer="$live_home/probe-answer.json"');
  const exported = source.indexOf('export CHARLIEBOT_TRIAL_PROBE_ANSWER=$probe_answer');
  assert.ok(assigned > 0 && assigned < exported, 'the answer file is named before the trial reads its path');
});

test('run.sh names each side\'s commit in the bench tree before that side\'s trial starts', () => {
  const source = fs.readFileSync(RUN, 'utf8');
  const beforeNamed = source.indexOf('name_trial_commit "$before_commit"');
  const beforeRun = source.indexOf('run_side before "$bench_trial_script"');
  const afterNamed = source.indexOf('name_trial_commit "$after_commit"');
  const afterRun = source.indexOf('run_side after "$bench_trial_script"');
  assert.ok(beforeNamed > 0 && beforeNamed < beforeRun, 'the before commit is named after the before trial starts');
  assert.ok(afterNamed > 0 && afterNamed < afterRun, 'the after commit is named after the after trial starts');
});

test('run.sh --side after with --before-run passes stored targets and enforces seed, Chrome, and bench-code identity', () => {
  const dir = scratch('pn-after-only-');
  const gitEnv = {
    ...process.env,
    GIT_AUTHOR_NAME: 'bench',
    GIT_AUTHOR_EMAIL: 'bench@example.invalid',
    GIT_COMMITTER_NAME: 'bench',
    GIT_COMMITTER_EMAIL: 'bench@example.invalid',
  };
  const git = (cwd, ...args) =>
    execFileSync('git', ['-C', cwd, ...args], { encoding: 'utf8', env: gitEnv, stdio: ['ignore', 'pipe', 'pipe'] }).trim();

  try {
    const repo = path.join(dir, 'repo');
    const pnDir = path.join(repo, 'dev', 'poor-network');
    fs.mkdirSync(pnDir, { recursive: true });
    fs.mkdirSync(path.join(repo, 'rust'), { recursive: true });
    fs.copyFileSync(RUN, path.join(pnDir, 'run.sh'));
    fs.chmodSync(path.join(pnDir, 'run.sh'), 0o755);
    for (const file of ['lock.sh', 'before-store.mjs', 'report.mjs', 'profile.mjs']) {
      fs.copyFileSync(path.join(BENCH_DIR, file), path.join(pnDir, file));
    }
    fs.writeFileSync(path.join(pnDir, 'bench.mjs'), '// bench\n');
    fs.writeFileSync(path.join(pnDir, 'README.md'), '# readme\n');
    fs.writeFileSync(path.join(pnDir, 'open.test.mjs'), '// test\n');
    git(repo, 'init', '-q');
    git(repo, 'add', '.');
    git(repo, 'commit', '-q', '-m', 'before bench');
    const beforeBenchRev = git(repo, 'rev-parse', 'HEAD');

    // Allowed edits (run.sh, README.md, *.test.mjs) advance HEAD past beforeBenchRev.
    fs.appendFileSync(path.join(pnDir, 'README.md'), 'note\n');
    fs.appendFileSync(path.join(pnDir, 'open.test.mjs'), '// extra\n');
    git(repo, 'add', '.');
    git(repo, 'commit', '-q', '-m', 'allowed bench edits');

    const seedDir = path.join(dir, 'seed');
    fs.mkdirSync(seedDir);
    fs.writeFileSync(path.join(seedDir, 'sessions.json'), '[]\n');
    const seedSha = execFileSync(
      'bash',
      ['-c', 'cd "$1" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum | cut -d" " -f1', 'bash', seedDir],
      { encoding: 'utf8' },
    ).trim();

    const beforeRun = path.join(dir, 'before-run');
    fs.mkdirSync(beforeRun);
    fs.writeFileSync(path.join(beforeRun, 'seed.sha256'), `${seedSha}  ${seedDir}\n`);
    fs.writeFileSync(path.join(beforeRun, 'before.json'), JSON.stringify({
      side: 'before',
      status: 'measured',
      chrome: { version: 'Chrome/149.0.7827.0' },
      open: {
        cpu: { rate: 4.56 },
        reference: { id: 'reference A link rate 1', reps: [{ rep: 1, newestTargetIdentity: 'web_reply_notice@139' }] },
        cells: [],
      },
    }));

    const binDir = path.join(dir, 'bin');
    fs.mkdirSync(binDir);
    const chromeGood = path.join(binDir, 'chrome-good');
    fs.writeFileSync(chromeGood, '#!/usr/bin/env bash\nprintf "Chromium 149.0.7827.0\\n"\n');
    fs.chmodSync(chromeGood, 0o755);
    const chromeBad = path.join(binDir, 'chrome-bad');
    fs.writeFileSync(chromeBad, '#!/usr/bin/env bash\nprintf "Google Chrome for Testing 156.0.8078.4\\n"\n');
    fs.chmodSync(chromeBad, 0o755);
    const cargoStop = path.join(binDir, 'cargo');
    fs.writeFileSync(cargoStop, '#!/usr/bin/env bash\nexit 99\n');
    fs.chmodSync(cargoStop, 0o755);

    const outDir = path.join(dir, 'out');
    const baseArgs = [
      path.join(pnDir, 'run.sh'),
      '--side', 'after',
      '--out-dir', outDir,
      '--before-run', beforeRun,
      '--before-bench-rev', beforeBenchRev,
      '--open-cpu-rate', '4.56',
      '--scenarios', 'open,foryou',
      '--seed-home', seedDir,
    ];
    const runWith = (chromeBin, extraArgs = []) => spawnSync('bash', [...baseArgs, ...extraArgs], {
      env: { ...process.env, PATH: `${binDir}:${process.env.PATH}`, POOR_NETWORK_CHROME: chromeBin },
      encoding: 'utf8',
    });

    // 1. Chrome mismatch refuses with exit 2 before creating a run or building.
    const badChrome = runWith(chromeBad);
    assert.equal(badChrome.status, 2, badChrome.stderr);
    assert.match(badChrome.stderr, /Chrome version mismatch/);

    // 2. Seed checksum mismatch refuses with exit 2.
    const wrongSeedDir = path.join(dir, 'wrong-seed');
    fs.mkdirSync(wrongSeedDir);
    fs.writeFileSync(path.join(wrongSeedDir, 'sessions.json'), '[{"id":"x"}]\n');
    const badSeed = spawnSync('bash', [
      path.join(pnDir, 'run.sh'),
      '--side', 'after',
      '--out-dir', outDir,
      '--before-run', beforeRun,
      '--before-bench-rev', beforeBenchRev,
      '--open-cpu-rate', '4.56',
      '--scenarios', 'open,foryou',
      '--seed-home', wrongSeedDir,
    ], {
      env: { ...process.env, PATH: `${binDir}:${process.env.PATH}`, POOR_NETWORK_CHROME: chromeGood },
      encoding: 'utf8',
    });
    assert.equal(badSeed.status, 2, badSeed.stderr);
    assert.match(badSeed.stderr, /seed checksum mismatch/);

    // 3. Disallowed change to bench.mjs refuses with exit 2.
    fs.appendFileSync(path.join(pnDir, 'bench.mjs'), '// changed\n');
    const badBench = runWith(chromeGood);
    assert.equal(badBench.status, 2, badBench.stderr);
    assert.match(badBench.stderr, /disallowed path.*bench\.mjs/);
    git(repo, 'checkout', '--', 'dev/poor-network/bench.mjs');

    // 4. Matching seed, Chrome, and allowed-only diff passes preflight and records bench-identity.json before cargo.
    const ok = runWith(chromeGood);
    assert.equal(ok.status, 99, `${ok.stdout}\n${ok.stderr}`);
    assert.match(ok.stdout, /stored benchTree/);
    const runs = fs.readdirSync(outDir).filter((name) => /^\d{8}T\d{6}Z$/.test(name));
    assert.equal(runs.length, 1);
    const identity = JSON.parse(fs.readFileSync(path.join(outDir, runs[0], 'bench-identity.json'), 'utf8'));
    assert.equal(identity.beforeBenchCommit, beforeBenchRev);
    assert.ok(identity.comparedFiles.includes('dev/poor-network/bench.mjs'));

    // 5. run.sh wires --expect-targets from before_run/before.json and compares against it in report.mjs.
    const runSource = fs.readFileSync(RUN, 'utf8');
    assert.match(runSource, /expect_args=\(--expect-targets "\$before_run\/before\.json"\)/);
    assert.match(runSource, /report\.mjs" "\$before_run\/before\.json" "\$run_dir\/after\.json"/);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
