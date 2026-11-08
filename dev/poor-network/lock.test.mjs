// Tests of the bench lock: every side attempt takes the one file lock.sh names, so two side attempts on a host run one
// after the other, and no caller option picks another file or skips it.

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
const RUN = path.join(BENCH_DIR, 'run.sh');
const LOCK_FILE = 'pn-runs/.bench.lock';

function scratch(prefix) {
  return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

// The lock file as the bench code computes it, read from a shell that sources the module under a scratch home.
function lockFile(home) {
  return execFileSync('bash', ['-c', 'source "$1"; printf "%s\\n" "$BENCH_LOCK_FILE"', 'bash', LOCK], {
    env: { ...process.env, HOME: home },
    encoding: 'utf8',
  }).trim();
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

test('the bench takes its lock under $HOME/pn-runs, and lock.sh names it once', () => {
  const home = scratch('pn-lock-home-');
  try {
    assert.equal(lockFile(home), path.join(home, LOCK_FILE));

    const sources = fs
      .readdirSync(BENCH_DIR)
      .filter((name) => (name.endsWith('.sh') || name.endsWith('.mjs')) && !name.endsWith('.test.mjs'));
    const named = sources.reduce(
      (count, name) =>
        count + [...fs.readFileSync(path.join(BENCH_DIR, name), 'utf8').matchAll(/pn-runs\/\.bench\.lock/g)].length,
      0,
    );
    assert.equal(named, 1, `the bench code names the lock path ${named} times, not once`);
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
