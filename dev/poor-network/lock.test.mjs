// Tests of the bench lock: every side attempt takes the one file lock.sh names, so two side attempts on a host run one
// after the other, and no caller option picks another file or skips it. The improve trial side waits on the same lock.

import assert from 'node:assert/strict';
import { execFileSync, spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const BENCH_DIR = path.dirname(fileURLToPath(import.meta.url));
const DEV_DIR = path.dirname(BENCH_DIR);
const LOCK = path.join(BENCH_DIR, 'lock.sh');
const RUN = path.join(BENCH_DIR, 'run.sh');
const IMPROVE_SIDE = path.join(DEV_DIR, 'improve', 'trial-side.sh');
const LOCK_FILE = 'pn-runs/.bench.lock';

function scratch(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// A variable of the lock module, read from a shell that sources the module under a scratch home.
function lockVariable(home, name) {
  return execFileSync('bash', ['-c', 'source "$1"; printf "%s" "${!2}"', 'bash', LOCK, name], {
    env: { ...process.env, HOME: home },
    encoding: 'utf8',
  });
}

// The lock file as the bench code computes it.
function lockFile(home) {
  return lockVariable(home, 'BENCH_LOCK_FILE');
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

// Runs an improve trial side under a scratch home with a short wait. Its command touches `ran` and exits with `code`.
function improveSide(home, ran, waitSeconds, code) {
  return spawnSync('bash', [IMPROVE_SIDE, 'bash', '-c', 'touch "$1"; exit "$2"', 'bash', ran, String(code)], {
    env: { ...process.env, HOME: home, IMPROVE_TRIAL_LOCK_WAIT_SECONDS: String(waitSeconds) },
    encoding: 'utf8',
    timeout: 30000,
  });
}

// Starts one side attempt: the side script runs under the bench lock and records its start and end times in `log`.
function sideAttempt(home, body, log) {
  const child = spawn('bash', ['-c', 'source "$1"; bench_lock bash "$2" "$3"', 'bash', LOCK, body, log], {
    env: { ...process.env, HOME: home },
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

test('the bench takes its lock under $HOME/pn-runs, and only lock.sh names the file', () => {
  const home = scratch('pn-lock-home-');
  try {
    assert.equal(lockFile(home), path.join(home, LOCK_FILE));

    const naming = devSources(DEV_DIR).filter((file) =>
      /pn-runs\/\.bench\.lock|BENCH_LOCK_FILE/.test(fs.readFileSync(file, 'utf8')),
    );
    assert.deepEqual(
      naming,
      [LOCK],
      `the lock file is named outside lock.sh in ${naming.map((file) => path.relative(DEV_DIR, file)).join(', ')}`,
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

test('the bench refuses a caller-chosen lock file and takes the lock itself', { timeout: 30000 }, async () => {
  const home = scratch('pn-lock-home-');
  const chosen = path.join(home, 'chosen.lock');
  try {
    const child = spawn('bash', [RUN, '--lock', chosen], {
      env: { ...process.env, HOME: home },
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

test('an improve trial side waits out a held lock, then stops unrun and names the bench lock', { timeout: 60000 }, async () => {
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
  } finally {
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
