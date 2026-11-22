// Tests of the bench's step gate: the yield rule (a queued process gets the lock at a step's end), the cap (a step past
// its cap is stopped and runs once more), and the descriptor (no child of the bench holds the lock). The tests run the
// real lock functions under a scratch HOME, so the lock file is the one lock.sh names there.

import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';

import { closeAllLive, trackLive } from './live.mjs';
import { BenchGate } from './step-gate.mjs';

const savedHome = process.env.HOME;
const home = fs.mkdtempSync(path.join(os.tmpdir(), 'step-gate-'));
process.env.HOME = home;
// Every gate a test opens, closed at the end, so a failed assertion cannot leave the lock held for the next test.
const gates = [];
after(async () => {
  for (const gate of gates) {
    await gate.close();
  }
  process.env.HOME = savedHome;
  fs.rmSync(home, { recursive: true, force: true });
});
const lockFile = path.join(home, 'pn-runs', '.bench.lock');

async function waitUntil(check, timeoutMs = 10000) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (await check()) {
      return;
    }
    if (Date.now() >= deadline) {
      throw new Error('condition not met in time');
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
}

async function openGate(capMs = 60000) {
  const gate = new BenchGate({ lockWaitSeconds: 30, capMs });
  gates.push(gate);
  await gate.open();
  return gate;
}

// A process outside the bench that queues for the lock, touches `done` once it holds it, and holds it for a moment.
// `closed` resolves with its exit code; it is listened for from the spawn, since the waiter may exit during a yield.
function queueOutsideWaiter(done) {
  const child = spawn('flock', ['-w', '60', lockFile, 'sh', '-c', 'touch "$1"; sleep 0.2', 'sh', done], { stdio: 'ignore' });
  const closed = once(child, 'close').then(([code]) => code);
  return { closed };
}

test('a yield hands the lock to a process queued during the step, and the gate queues again', async () => {
  const gate = await openGate();
  await gate.take();
  const done = path.join(home, 'waiter-done');
  const { closed } = queueOutsideWaiter(done);
  await waitUntil(async () => (await gate.waiters()).length > 0);
  const record = await gate.yieldIfWaited();
  assert.equal(await closed, 0);
  assert.ok(fs.existsSync(done), 'the queued process ran once the gate released the lock');
  assert.ok(record !== null, 'a queued process means a yield');
  assert.equal(record.waiterPids.length, 1);
  assert.ok(record.seenToReleaseMs >= 0);
  assert.equal(gate.held, true, 'the gate queues again after the yield');
  assert.equal(gate.yields.length, 1);
  await gate.close();
});

test('nothing queued means no yield, and the gate keeps the lock', async () => {
  const gate = await openGate();
  await gate.take();
  assert.equal(await gate.yieldIfWaited(), null);
  assert.equal(gate.held, true);
  await gate.close();
});

test('a step past its cap is stopped and runs once more; a second pass past the cap ends it as capped', async () => {
  const gate = await openGate(200);
  await gate.take();
  let runs = 0;
  const stopped = [];
  const result = await gate.step('measure', 'hangs', () => {
    runs += 1;
    const attempt = runs;
    trackLive(`browser-${attempt}`, async () => {
      stopped.push(attempt);
    });
    return new Promise(() => {});
  });
  assert.equal(result.status, 'capped');
  assert.equal(runs, 2);
  assert.deepEqual(stopped, [1, 2]);
  assert.deepEqual(gate.units.map((unit) => unit.status), ['capped', 'capped']);
  assert.deepEqual(gate.units[0].stopped, ['browser-1']);
  // A step after the capped ones runs as usual, and the gate still holds the lock.
  const after2 = await gate.step('measure', 'next', () => 'value');
  assert.equal(after2.status, 'ok');
  assert.equal(after2.value, 'value');
  assert.equal(gate.held, true);
  await gate.close();
});

test('a step that passes its cap once and then finishes is kept as ok on its second pass', async () => {
  const gate = await openGate(200);
  await gate.take();
  let runs = 0;
  const result = await gate.step('measure', 'stall once', () => {
    runs += 1;
    return runs === 1 ? new Promise(() => {}) : 'second';
  });
  assert.equal(result.status, 'ok');
  assert.equal(result.value, 'second');
  assert.equal(result.unit.attempt, 2);
  assert.deepEqual(gate.units.map((unit) => unit.status), ['capped', 'ok']);
  await gate.close();
});

test('a step that throws fails the caller, and the cap does not hide the error', async () => {
  const gate = await openGate(60000);
  await gate.take();
  await assert.rejects(
    gate.step('prepare', 'broken', () => {
      throw new Error('the stage broke');
    }),
    /the stage broke/,
  );
  await gate.close();
});

test('a capped step leaves no child holding the lock: its child is stopped, and a queued process gets the lock', async () => {
  const gate = await openGate(200);
  await gate.take();
  let child = null;
  const result = await gate.step('measure', 'spawns', () => {
    child = spawn('sleep', ['30'], { stdio: 'ignore' });
    const exited = once(child, 'exit');
    trackLive('child', async () => {
      child.kill('SIGKILL');
      await exited;
    });
    return new Promise(() => {});
  });
  assert.equal(result.status, 'capped');
  assert.ok(child.exitCode !== null || child.signalCode !== null, 'the step child is stopped at the cap');
  const done = path.join(home, 'after-capped');
  const { closed } = queueOutsideWaiter(done);
  await waitUntil(async () => (await gate.waiters()).length > 0);
  const record = await gate.yieldIfWaited();
  assert.equal(await closed, 0);
  assert.ok(fs.existsSync(done));
  assert.ok(record !== null);
  await gate.close();
});

// The file each descriptor of a child process names, as the child lists them.
const LIST_FDS = 'for f in /proc/$$/fd/*; do readlink "$f" 2>/dev/null; done; exit 0';

test('the lock descriptor is close-on-exec: no child of the bench holds the lock file', async () => {
  const gate = await openGate();
  assert.equal(fs.readlinkSync(`/proc/self/fd/${gate.descriptor}`), lockFile, 'the gate holds the lock file');
  // A child the bench starts without the descriptor does not list the lock file.
  const plain = spawnSync('bash', ['-c', LIST_FDS], { encoding: 'utf8' });
  assert.ok(!plain.stdout.split('\n').includes(lockFile), 'a child inherited the lock descriptor');
  // The control: a child given the descriptor on purpose lists the lock file, so the check above can see one.
  const given = spawnSync('bash', ['-c', LIST_FDS], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe', gate.descriptor] });
  assert.ok(given.stdout.split('\n').includes(lockFile), 'the control child lists the lock file');
  await gate.close();
});

test('closing the gate releases a lock it still holds', async () => {
  const gate = await openGate();
  await gate.take();
  const done = path.join(home, 'after-close');
  const { closed } = queueOutsideWaiter(done);
  await waitUntil(async () => (await gate.waiters()).length > 0);
  await gate.close();
  assert.equal(await closed, 0);
  assert.ok(fs.existsSync(done), 'the queued process got the lock once the gate closed');
});

test('closeAllLive stops every resource it can and names the one it cannot', async () => {
  const stopped = [];
  trackLive('good', async () => {
    stopped.push('good');
  });
  trackLive('bad', async () => {
    throw new Error('stuck');
  });
  await assert.rejects(closeAllLive(), /could not stop: bad: stuck/);
  assert.deepEqual(stopped, ['good']);
  await closeAllLive();
});
