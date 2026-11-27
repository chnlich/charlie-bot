// Tests of dev/improve/trial-side.sh beside the bench lock: a lockless side never waits for the lock and never
// refuses to run because of its holder. A holder read that fails is recorded as unreadable, with the reader's reason,
// and the side's exit code stays its command's. The stop tests send SIGTERM to a side and check that its trial ends,
// that the lock is free and that the side record shows the stop. The tree tests check that the trial's own cleanup
// removes the trial tree after a stop, so the next side for the commit starts, and that the stop line names a tree that
// a SIGKILL left. The bench lock is a scratch file, so no case takes the host's lock.
// A test file's name ends in .test.mjs, so the bench-lock naming check in
// dev/poor-network/lock.integration.test.mjs reads it as a test.

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const IMPROVE_DIR = path.dirname(fileURLToPath(import.meta.url));
const TRIAL_SIDE = path.join(IMPROVE_DIR, 'trial-side.sh');
const LOCK = path.join(path.dirname(IMPROVE_DIR), 'poor-network', 'lock.sh');
const LOCK_FILE = 'pn-runs/.bench.lock';
const REAL_GREP = execFileSync('bash', ['-c', 'command -v grep'], { encoding: 'utf8' }).trim();

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

// The stop tests. A serve side runs a stub as its trial: the stub stands in for scripts/release/trial.sh, runs one
// child process, writes both pids under STUB_DIR, and on TERM ends its child, writes a teardown mark and exits 143, as
// trial.sh does. With STUB_IGNORE_TERM=1 it ignores TERM, so only a SIGKILL ends it.
const STOP_COMMIT = '0123456789abcdef0123456789abcdef01234567';
const TRIAL_STUB = [
  '#!/usr/bin/env bash',
  'if [[ ${STUB_IGNORE_TERM:-0} == 1 ]]; then',
  "  trap '' TERM",
  'else',
  '  teardown() {',
  '    kill -TERM "$child" 2>/dev/null',
  '    wait "$child"',
  '    : > "$STUB_DIR/teardown"',
  '    exit 143',
  '  }',
  '  trap teardown TERM',
  'fi',
  'sleep 60 &',
  'child=$!',
  'printf "%s\\n" "$$" > "$STUB_DIR/trial.pid"',
  'printf "%s\\n" "$child" > "$STUB_DIR/child.pid"',
  'wait "$child"',
  '',
].join('\n');

let stopCount = 0;

// A serve side that runs the stub as its trial, with its stderr collected. Each side names its own direction, so its
// marker directory in /tmp is its own.
function startStopSide(home, dir, extraEnv = {}) {
  const binary = path.join(dir, 'server');
  fs.writeFileSync(binary, 'server binary');
  const stub = path.join(dir, 'trial.sh');
  fs.writeFileSync(stub, TRIAL_STUB, { mode: 0o755 });
  stopCount += 1;
  const direction = `stop${process.pid}x${stopCount}`;
  const child = spawn('bash', [TRIAL_SIDE, stub, '--serve', STOP_COMMIT, binary, '--', 'driver'], {
    env: {
      ...scratchEnv(home),
      IMPROVE_DIRECTION: direction,
      CHARLIEBOT_RELEASE_RUN_DIR: path.join(dir, 'evidence'),
      STUB_DIR: dir,
      ...extraEnv,
    },
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr += chunk;
  });
  return { child, direction, stderr: () => stderr };
}

// Whether a process runs: a zombie has ended, and a process whose stat file is gone has ended.
function processAlive(pid) {
  try {
    const stat = fs.readFileSync(`/proc/${pid}/stat`, 'utf8');
    return !/^\d+ \(.*\) [ZX] /.test(stat);
  } catch (error) {
    if (error.code === 'ENOENT') {
      return false;
    }
    throw error;
  }
}

function pidFile(dir, name) {
  return Number(fs.readFileSync(path.join(dir, name), 'utf8'));
}

// Whether another process holds the bench lock: flock -n fails while it does.
function lockFree(lockFile) {
  return spawnSync('flock', ['-n', lockFile, 'true']).status === 0;
}

// The lines of the one side record in an evidence directory.
function sideRecordLines(evidence) {
  const records = fs.readdirSync(evidence).filter((name) => name.startsWith('side-record-'));
  assert.equal(records.length, 1, `expected one side record in ${evidence}, found ${records.length}`);
  return fs.readFileSync(path.join(evidence, records[0]), 'utf8').trim().split('\n');
}

test('a SIGTERM to a running side stops its trial, frees the lock and records the stop', {
  timeout: 60000,
}, async () => {
  const home = scratch('improve-trial-home-');
  const dir = scratch('improve-trial-run-');
  const lockFile = path.join(home, LOCK_FILE);
  const side = startStopSide(home, dir);
  try {
    await waitFor(() => fs.existsSync(path.join(dir, 'child.pid')));
    const trialPid = pidFile(dir, 'trial.pid');
    const childPid = pidFile(dir, 'child.pid');
    assert.equal(heldLockPids(lockFile).length, 1, 'the running trial does not hold the bench lock');

    side.child.kill('SIGTERM');
    const [code] = await once(side.child, 'close');
    assert.equal(code, 143, side.stderr());
    assert.match(side.stderr(), new RegExp(`improve trial side stopped by TERM: trial pid ${trialPid};`));
    assert.equal(fs.existsSync(path.join(dir, 'teardown')), true, 'the trial did not run its teardown');
    await waitFor(() => !processAlive(trialPid) && !processAlive(childPid));
    assert.equal(lockFree(lockFile), true, 'the side left the bench lock held');
    const record = sideRecordLines(path.join(dir, 'evidence'));
    assert.ok(record.includes('exit_status=143'), `the side record does not show the stop: ${record.join(' | ')}`);
  } finally {
    fs.rmSync(path.join('/tmp', `improve-${side.direction}`), { recursive: true, force: true });
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a trial that ignores SIGTERM is SIGKILLed at the stop bound, and its side still stops', {
  timeout: 60000,
}, async () => {
  const home = scratch('improve-trial-home-');
  const dir = scratch('improve-trial-run-');
  const lockFile = path.join(home, LOCK_FILE);
  const side = startStopSide(home, dir, { STUB_IGNORE_TERM: '1', IMPROVE_TRIAL_STOP_WAIT_SECONDS: '2' });
  try {
    await waitFor(() => fs.existsSync(path.join(dir, 'child.pid')));
    const trialPid = pidFile(dir, 'trial.pid');
    const childPid = pidFile(dir, 'child.pid');

    side.child.kill('SIGTERM');
    const [code] = await once(side.child, 'close');
    assert.equal(code, 143, side.stderr());
    assert.match(side.stderr(), new RegExp(
      `improve trial side stopped by TERM: trial pid ${trialPid};.*did not end within 2 s, so SIGKILL`,
    ));
    await waitFor(() => !processAlive(trialPid) && !processAlive(childPid));
    assert.equal(lockFree(lockFile), true, 'the side left the bench lock held');
    const record = sideRecordLines(path.join(dir, 'evidence'));
    assert.ok(record.includes('exit_status=143'), `the side record does not show the stop: ${record.join(' | ')}`);
  } finally {
    fs.rmSync(path.join('/tmp', `improve-${side.direction}`), { recursive: true, force: true });
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a SIGTERM while a side waits for the bench lock starts no trial and leaves the holder its lock', {
  timeout: 60000,
}, async () => {
  const home = scratch('improve-trial-home-');
  const dir = scratch('improve-trial-run-');
  const marker = path.join(dir, 'held');
  const lockFile = path.join(home, LOCK_FILE);
  const holder = startHolder(home, marker);
  let side = null;
  try {
    await waitFor(() => fs.existsSync(marker));
    side = startStopSide(home, dir, { IMPROVE_TRIAL_LOCK_WAIT_SECONDS: '60' });
    // The side is waiting for the lock whether or not its wait has begun, so the stop has the same outcome either way.
    await new Promise((resolve) => setTimeout(resolve, 1000));
    side.child.kill('SIGTERM');
    const [code] = await once(side.child, 'close');
    assert.equal(code, 143, side.stderr());
    assert.match(side.stderr(), /improve trial side stopped by TERM: trial pid none;/);
    assert.equal(fs.existsSync(path.join(dir, 'trial.pid')), false, 'a trial started during the lock wait');
    assert.equal(lockFree(lockFile), false, 'the stop freed the lock the holder keeps');
  } finally {
    if (side !== null) {
      fs.rmSync(path.join('/tmp', `improve-${side.direction}`), { recursive: true, force: true });
    }
    fs.writeFileSync(`${marker}.release`, '');
    await holder.exited;
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

// The tree tests. Their stub stands in for the tree that scripts/release/trial.sh keeps: it refuses, as the trial's
// preflight does, when the tree path exists; it creates the tree and the temp root, runs one child and, on TERM, exits
// 143 through its cleanup. The cleanup removes both paths through a child that runs for a second, as rm -rf of a seeded
// tree does, so a signal that reaches the removal leaves both paths. With STUB_IGNORE_TERM=1 the stub ignores TERM and
// has no cleanup; with STUB_QUICK=1 it ends through its cleanup at once.
const TREE_STUB = [
  '#!/usr/bin/env bash',
  'if [[ -e $STUB_TREE ]]; then',
  '  printf "preflight failed: the trial tree path already exists: %s\\n" "$STUB_TREE" >&2',
  '  exit 2',
  'fi',
  'mkdir -p "$STUB_TREE/home" "$STUB_TEMP_ROOT" || exit 1',
  'if [[ ${STUB_IGNORE_TERM:-0} == 1 ]]; then',
  "  trap '' TERM",
  'else',
  '  cleanup() {',
  '    local status=$?',
  '    trap - EXIT TERM',
  '    bash -c \'sleep 1 && rm -rf -- "$@"\' remove "$STUB_TREE" "$STUB_TEMP_ROOT"',
  '    exit "$status"',
  '  }',
  '  trap cleanup EXIT',
  "  trap 'exit 143' TERM",
  'fi',
  'if [[ ${STUB_QUICK:-0} == 1 ]]; then',
  '  exit 0',
  'fi',
  'sleep 60 &',
  'child=$!',
  'printf "%s\\n" "$$" > "$STUB_DIR/trial.pid"',
  'printf "%s\\n" "$child" > "$STUB_DIR/child.pid"',
  'wait "$child"',
  '',
].join('\n');

// The trial tree and temp root that trial.sh names for a commit under a TMPDIR, as its header gives them.
function trialPaths(tmpdir, commit) {
  const uid = process.getuid();
  const key = crypto.createHash('sha256').update(`${uid} ${commit}\n`).digest('hex');
  return {
    tree: path.join(tmpdir, `charliebot-release-trial-${uid}-${commit.slice(0, 12)}`),
    tempRoot: path.join(tmpdir, `t-${key.slice(0, 12)}`),
  };
}

// A serve side whose trial is the tree stub, with its TMPDIR, its evidence and the stub's pid files in `dir`. useLock is
// the side's IMPROVE_TRIAL_USE_LOCK: '1' holds the scratch bench lock, and '0' runs beside it, as an improve-ux side
// does.
function treeSideArgs(home, dir, useLock, evidence, extraEnv) {
  const binary = path.join(dir, 'server');
  fs.writeFileSync(binary, 'server binary');
  const stub = path.join(dir, 'tree-trial.sh');
  fs.writeFileSync(stub, TREE_STUB, { mode: 0o755 });
  const tmpdir = path.join(dir, 'tmp');
  fs.mkdirSync(tmpdir, { recursive: true });
  const { tree, tempRoot } = trialPaths(tmpdir, STOP_COMMIT);
  stopCount += 1;
  const direction = useLock === '0' ? 'ux' : `stop${process.pid}x${stopCount}`;
  const env = {
    ...scratchEnv(home),
    TMPDIR: tmpdir,
    IMPROVE_DIRECTION: direction,
    IMPROVE_TRIAL_USE_LOCK: useLock,
    CHARLIEBOT_RELEASE_RUN_DIR: path.join(dir, evidence),
    STUB_DIR: dir,
    STUB_TREE: tree,
    STUB_TEMP_ROOT: tempRoot,
    ...extraEnv,
  };
  return { args: [TRIAL_SIDE, stub, '--serve', STOP_COMMIT, binary, '--', 'driver'], env, direction, tree, tempRoot };
}

function startTreeSide(home, dir, useLock, evidence, extraEnv = {}) {
  const side = treeSideArgs(home, dir, useLock, evidence, extraEnv);
  const child = spawn('bash', side.args, { env: side.env, stdio: ['ignore', 'ignore', 'pipe'] });
  let stderr = '';
  child.stderr.on('data', (chunk) => {
    stderr += chunk;
  });
  return { ...side, child, stderr: () => stderr };
}

for (const useLock of ['1', '0']) {
  test(`a SIGTERM to a side (IMPROVE_TRIAL_USE_LOCK=${useLock}) lets the trial's cleanup remove its tree, and the next `
    + 'side for the commit starts', { timeout: 60000 }, async () => {
    const home = scratch('improve-trial-home-');
    const dir = scratch('improve-trial-run-');
    const lockFile = path.join(home, LOCK_FILE);
    const side = startTreeSide(home, dir, useLock, 'evidence-stopped');
    try {
      await waitFor(() => fs.existsSync(path.join(dir, 'child.pid')));
      const trialPid = pidFile(dir, 'trial.pid');
      const childPid = pidFile(dir, 'child.pid');
      assert.equal(fs.existsSync(side.tree), true, 'the stub did not create the trial tree');

      side.child.kill('SIGTERM');
      const [code] = await once(side.child, 'close');
      assert.equal(code, 143, side.stderr());
      const stopLine = side.stderr().split('\n').find((line) => line.startsWith('improve trial side stopped by TERM:'));
      assert.match(stopLine, new RegExp(`trial pid ${trialPid}; the trial ended after the signal`));
      assert.ok(!stopLine.includes(side.tree) && !stopLine.includes(side.tempRoot), stopLine);
      // The side exits after the trial's cleanup has ended, so both paths are gone when it does.
      assert.equal(fs.existsSync(side.tree), false, `the trial tree remains: ${side.stderr()}`);
      assert.equal(fs.existsSync(side.tempRoot), false, `the trial temp root remains: ${side.stderr()}`);
      assert.equal(processAlive(trialPid) || processAlive(childPid), false, 'a trial process outlived the side');
      if (useLock === '1') {
        assert.equal(lockFree(lockFile), true, 'the side left the bench lock held');
      }
      const record = sideRecordLines(path.join(dir, 'evidence-stopped'));
      assert.ok(record.includes('exit_status=143'), `the side record does not show the stop: ${record.join(' | ')}`);

      const next = treeSideArgs(home, dir, useLock, 'evidence-next', { STUB_QUICK: '1' });
      const run = spawnSync('bash', next.args, { env: next.env, encoding: 'utf8', timeout: 30000 });
      assert.equal(run.status, 0, run.stderr);
      assert.doesNotMatch(run.stderr, /preflight failed/);
    } finally {
      if (useLock === '1') {
        fs.rmSync(path.join('/tmp', `improve-${side.direction}`), { recursive: true, force: true });
      }
      fs.rmSync(home, { recursive: true, force: true });
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });
}

test('a trial that ignores SIGTERM is SIGKILLed at the bound, and the stop line names the trial paths that remain', {
  timeout: 60000,
}, async () => {
  const home = scratch('improve-trial-home-');
  const dir = scratch('improve-trial-run-');
  const side = startTreeSide(home, dir, '0', 'evidence', {
    STUB_IGNORE_TERM: '1',
    IMPROVE_TRIAL_STOP_WAIT_SECONDS: '2',
  });
  try {
    await waitFor(() => fs.existsSync(path.join(dir, 'child.pid')));
    const trialPid = pidFile(dir, 'trial.pid');
    const childPid = pidFile(dir, 'child.pid');

    side.child.kill('SIGTERM');
    const [code] = await once(side.child, 'close');
    assert.equal(code, 143, side.stderr());
    const stopLine = side.stderr().split('\n').find((line) => line.startsWith('improve trial side stopped by TERM:'));
    assert.match(stopLine, new RegExp(`trial pid ${trialPid};.*did not end within 2 s, so SIGKILL`));
    for (const left of [side.tree, side.tempRoot]) {
      assert.equal(fs.existsSync(left), true, `the SIGKILL left no ${left}, so the case shows nothing`);
      assert.ok(stopLine.includes(left), `the stop line does not name ${left}: ${stopLine}`);
    }
    assert.equal(processAlive(trialPid) || processAlive(childPid), false, 'a trial process outlived the side');
    const record = sideRecordLines(path.join(dir, 'evidence'));
    assert.ok(record.includes('exit_status=143'), `the side record does not show the stop: ${record.join(' | ')}`);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
