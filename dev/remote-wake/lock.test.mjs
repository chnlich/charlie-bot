// Tests of the remote-wake bench lock: the bench holds the host's one bench lock, the file the poor-network bench's
// lock module names, so a poor-network side and a remote-wake container never run side by side.

import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const BENCH_DIR = path.dirname(fileURLToPath(import.meta.url));
const LOCK = path.join(BENCH_DIR, 'lock.sh');
const POOR_NETWORK_LOCK = path.join(BENCH_DIR, '..', 'poor-network', 'lock.sh');

function scratch(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// The shared file, the wait and the timeout exit as a shell computes them after sourcing one lock module.
function lockSettings(module, home) {
  const out = execFileSync(
    'bash',
    [
      '-c',
      'source "$1"; printf "%s\\n%s\\n%s\\n" "$BENCH_LOCK_FILE" "$BENCH_LOCK_WAIT_SECONDS" "$BENCH_LOCK_TIMEOUT_EXIT"',
      'bash',
      module,
    ],
    { env: { ...process.env, HOME: home }, encoding: 'utf8' },
  );
  const [file, wait, timeoutExit] = out.trim().split('\n');
  return { file, wait, timeoutExit };
}

// One side attempt under the poor-network lock, which execs its command in flock's process.
const POOR_NETWORK_HOLD = 'source "$1"; bench_lock bash "$2" "$4" "$3"';
const REMOTE_WAKE_HOLD = 'source "$1"; remote_wake_lock || exit $?; bash "$2" "$4" "$3"';

function attempt(home, script, module, body, log, label) {
  const child = spawn('bash', ['-c', script, 'bash', module, body, log, label], {
    env: { ...process.env, HOME: home },
    stdio: ['ignore', 'ignore', 'pipe'],
  });
  return { child, exited: once(child, 'exit') };
}

function poorNetworkAttempt(home, body, log, label) {
  return attempt(home, POOR_NETWORK_HOLD, POOR_NETWORK_LOCK, body, log, label);
}

// One side attempt under the remote-wake lock, which this shell holds on its own file descriptor until it exits.
function remoteWakeAttempt(home, body, log, label) {
  return attempt(home, REMOTE_WAKE_HOLD, LOCK, body, log, label);
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

// A body that records when its work starts and ends, so a held lock shows up as an ordering in the log.
function writeBody(dir) {
  const body = path.join(dir, 'side.sh');
  fs.writeFileSync(
    body,
    '#!/usr/bin/env bash\nset -euo pipefail\n' +
      'printf "%s-start %s\\n" "$1" "$(date +%s%N)" >>"$2"\n' +
      'sleep 2\n' +
      'printf "%s-end %s\\n" "$1" "$(date +%s%N)" >>"$2"\n',
  );
  return body;
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

test('the bench takes the poor-network lock file and names no lock file of its own', () => {
  const home = scratch('rw-lock-home-');
  try {
    const remoteWake = lockSettings(LOCK, home);
    const poorNetwork = lockSettings(POOR_NETWORK_LOCK, home);
    assert.equal(remoteWake.file, poorNetwork.file, 'the two benches name different lock files');
    assert.ok(path.isAbsolute(remoteWake.file) && remoteWake.file.length > 0);
    assert.equal(remoteWake.wait, '7200');
    assert.equal(remoteWake.timeoutExit, '75');

    // The shared file's name comes from the module that owns it, so this test spells no lock file of its own. A
    // mention in the bench's code would be a second owner of the shared file.
    const lockName = path.basename(remoteWake.file).replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const sources = fs
      .readdirSync(BENCH_DIR)
      .filter((name) => (name.endsWith('.sh') || name.endsWith('.mjs')) && !name.endsWith('.test.mjs'));
    const named = sources.reduce(
      (count, name) =>
        count + [...fs.readFileSync(path.join(BENCH_DIR, name), 'utf8').matchAll(new RegExp(lockName, 'g'))].length,
      0,
    );
    assert.equal(named, 0, `the remote-wake bench names a lock file ${named} times`);
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
  }
});

test('a poor-network hold makes the remote-wake lock step wait', { timeout: 60000 }, async () => {
  const home = scratch('rw-lock-home-');
  const dir = scratch('rw-lock-run-');
  const log = path.join(dir, 'attempts.log');
  const body = writeBody(dir);
  const poorNetwork = poorNetworkAttempt(home, body, log, 'pn');
  let remoteWake = null;
  try {
    // The poor-network side is inside its work before the remote-wake side starts, so it meets a held lock.
    await waitFor(() => fs.existsSync(log) && fs.readFileSync(log, 'utf8').includes('pn-start'));
    remoteWake = remoteWakeAttempt(home, body, log, 'rw');
    await new Promise((resolve) => setTimeout(resolve, 500));
    assert.equal(remoteWake.child.exitCode, null, 'the remote-wake lock step ended while poor-network held the lock');
    assert.equal(
      fs.readFileSync(log, 'utf8').split('rw-start').length - 1,
      0,
      'the remote-wake side started while poor-network held the lock',
    );

    const [poorNetworkCode] = await poorNetwork.exited;
    const [remoteWakeCode] = await remoteWake.exited;
    assert.equal(poorNetworkCode, 0);
    assert.equal(remoteWakeCode, 0);

    const events = attemptEvents(log);
    assert.deepEqual(events.map((event) => event.kind), ['pn-start', 'pn-end', 'rw-start', 'rw-end']);
    assert.ok(events[2].at >= events[1].at, 'the remote-wake side started before the poor-network hold ended');
  } finally {
    poorNetwork.child.kill('SIGKILL');
    remoteWake?.child.kill('SIGKILL');
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('a remote-wake hold makes the poor-network lock step wait', { timeout: 60000 }, async () => {
  const home = scratch('rw-lock-home-');
  const dir = scratch('rw-lock-run-');
  const log = path.join(dir, 'attempts.log');
  const body = writeBody(dir);
  const remoteWake = remoteWakeAttempt(home, body, log, 'rw');
  let poorNetwork = null;
  try {
    // The remote-wake shell holds the lock before the poor-network side starts.
    await waitFor(() => fs.existsSync(log) && fs.readFileSync(log, 'utf8').includes('rw-start'));
    poorNetwork = poorNetworkAttempt(home, body, log, 'pn');
    await new Promise((resolve) => setTimeout(resolve, 500));
    assert.equal(poorNetwork.child.exitCode, null, 'the poor-network side ended while remote-wake held the lock');
    assert.equal(
      fs.readFileSync(log, 'utf8').split('pn-start').length - 1,
      0,
      'the poor-network side started while remote-wake held the lock',
    );

    const [remoteWakeCode] = await remoteWake.exited;
    const [poorNetworkCode] = await poorNetwork.exited;
    assert.equal(remoteWakeCode, 0);
    assert.equal(poorNetworkCode, 0);

    const events = attemptEvents(log);
    assert.deepEqual(events.map((event) => event.kind), ['rw-start', 'rw-end', 'pn-start', 'pn-end']);
    assert.ok(events[2].at >= events[1].at, 'the poor-network side started before the remote-wake hold ended');
  } finally {
    remoteWake.child.kill('SIGKILL');
    poorNetwork?.child.kill('SIGKILL');
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
