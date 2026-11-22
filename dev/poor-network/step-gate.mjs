// The bench's step gate. The bench holds the host's bench lock one step at a time, so a process queued behind it waits for
// at most one step's cap. A step is one preparation stage, one probe or one measurement. The gate takes and releases the
// lock through lock.sh's functions, caps each step, stops what a capped step started, and gives the lock up after a step
// when a process is queued for it.

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { closeAllLive, liveNames } from './live.mjs';

const LOCK_SH = path.join(path.dirname(fileURLToPath(import.meta.url)), 'lock.sh');
// One step's cap, in ms (design value). The longest step the bench measured before this gate ran 381.7 s (REPORT §1.1),
// and search's broadcast drain stops at 600 s (search.mjs:31).
export const STEP_CAP_MS = 600000;
// How often the gate reads /proc/locks while it holds the lock, so a queued process is seen within this time (design
// value).
export const WAITER_POLL_MS = 5000;
// How long a queued process may take to get the lock after the gate releases it (design value).
export const RELEASE_TAKE_MS = 60000;
// The status flock exits with when its wait runs out (lock.sh BENCH_LOCK_TIMEOUT_EXIT).
export const LOCK_TIMEOUT_EXIT = 75;
// The descriptor the lock functions lock: the child's fd 3, which spawn gives it from this gate's descriptor.
const LOCK_FD_ARG = '3';

export class LockWaitTimeout extends Error {}

const CAPPED = Symbol('capped');

function isoOf(ms) {
  return new Date(ms).toISOString();
}

// Runs one lock.sh function under bash. `fd`, when set, is the gate's lock descriptor, given to the child as its fd 3.
function lockCall(name, args, fd = null) {
  return new Promise((resolve, reject) => {
    const stdio = ['ignore', 'pipe', 'pipe'];
    if (fd !== null) {
      stdio.push(fd);
    }
    const child = spawn('bash', ['-c', 'source "$1"; shift; "$@"', 'bash', LOCK_SH, name, ...args], { stdio });
    let stdout = '';
    let stderr = '';
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', (chunk) => {
      stdout += chunk;
    });
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', (chunk) => {
      stderr += chunk;
    });
    child.once('error', reject);
    child.once('close', (code) => resolve({ code, stdout, stderr }));
  });
}

function pidsIn(stdout) {
  return stdout.split('\n').filter((line) => line.length > 0);
}

export class BenchGate {
  #lockWaitSeconds;
  #capMs;
  #fd = null;
  #held = false;
  #poll = null;
  #pollReading = null;
  #pollError = null;
  #waitSeenAt = null;
  // Every step's record, in run order: the bench adds its own fields to a record it is given.
  units = [];
  // Every yield's record, in run order.
  yields = [];

  constructor({ lockWaitSeconds, capMs = STEP_CAP_MS }) {
    this.#lockWaitSeconds = lockWaitSeconds;
    this.#capMs = capMs;
  }

  get held() {
    return this.#held;
  }

  // The lock descriptor's number in this process, for the tests that check no child inherits it.
  get descriptor() {
    return this.#fd;
  }

  // Opens the lock file's descriptor and starts reading /proc/locks. node opens the file close-on-exec (libuv sets
  // O_CLOEXEC on every file it opens), so no child the bench starts inherits the descriptor: a child that a capped step
  // leaves running cannot keep the lock.
  async open() {
    const { code, stdout, stderr } = await lockCall('bench_lock_path', []);
    if (code !== 0) {
      throw new Error(`bench_lock_path failed (${code}): ${stderr.trim()}`);
    }
    this.#fd = fs.openSync(stdout.trim(), 'a');
    this.#poll = setInterval(() => this.#observe(), WAITER_POLL_MS);
  }

  #requireOpen() {
    if (this.#fd === null) {
      throw new Error('the bench lock descriptor is not open: call open() first');
    }
  }

  // Reads /proc/locks; a queued process seen while this gate holds the lock starts the wait this gate reports.
  #observe() {
    this.#pollReading = lockCall('bench_lock_waiter_pids', [])
      .then(({ code, stdout, stderr }) => {
        if (code !== 0) {
          throw new Error(`bench_lock_waiter_pids failed (${code}): ${stderr.trim()}`);
        }
        if (this.#held && pidsIn(stdout).length > 0 && this.#waitSeenAt === null) {
          this.#waitSeenAt = Date.now();
        }
      })
      .catch((error) => {
        this.#pollError ??= error;
      });
  }

  #checkPoll() {
    if (this.#pollError !== null) {
      throw this.#pollError;
    }
  }

  // Waits for the bench lock, up to the wait. A wait that runs out throws LockWaitTimeout.
  async take() {
    this.#requireOpen();
    const { code, stderr } = await lockCall('bench_lock_fd_take', [LOCK_FD_ARG, String(this.#lockWaitSeconds)], this.#fd);
    if (code === LOCK_TIMEOUT_EXIT) {
      throw new LockWaitTimeout(`the bench lock stayed held for ${this.#lockWaitSeconds} s`);
    }
    if (code !== 0) {
      throw new Error(`taking the bench lock failed (${code}): ${stderr.trim()}`);
    }
    this.#held = true;
  }

  async release() {
    this.#requireOpen();
    const { code, stderr } = await lockCall('bench_lock_fd_release', [LOCK_FD_ARG], this.#fd);
    if (code !== 0) {
      throw new Error(`releasing the bench lock failed (${code}): ${stderr.trim()}`);
    }
    this.#held = false;
    this.#waitSeenAt = null;
  }

  // The pids queued for the lock now.
  async waiters() {
    const { code, stdout, stderr } = await lockCall('bench_lock_waiter_pids', []);
    if (code !== 0) {
      throw new Error(`bench_lock_waiter_pids failed (${code}): ${stderr.trim()}`);
    }
    return pidsIn(stdout).map(Number);
  }

  // Runs one step under the cap. A step that passes the cap is stopped (every browser and proxy it started), and it runs
  // once more; a second pass over the cap ends it as capped. A step that throws fails the caller. Returns
  // { status: 'ok', value, unit } or { status: 'capped', unit }; the unit is the record the gate keeps for the step.
  async step(kind, name, body) {
    this.#checkPoll();
    this.#requireOpen();
    for (let attempt = 1; ; attempt++) {
      const startedAt = Date.now();
      const outcome = await this.#runCapped(body);
      const unit = { kind, name, attempt, status: outcome.status, startedAt: isoOf(startedAt), endedAt: isoOf(Date.now()) };
      if (outcome.status === 'capped') {
        unit.stopped = outcome.stopped;
      }
      this.units.push(unit);
      this.#checkPoll();
      if (outcome.status === 'ok') {
        return { status: 'ok', value: outcome.value, unit };
      }
      if (attempt === 2) {
        return { status: 'capped', unit };
      }
    }
  }

  async #runCapped(body) {
    let timer = null;
    const cap = new Promise((resolve) => {
      timer = setTimeout(() => resolve(CAPPED), this.#capMs);
    });
    const run = Promise.resolve().then(body);
    // A body that finishes after its cap rejects into a race that has already settled; this keeps that rejection from
    // being reported as unhandled. A body that fails before the cap still fails the race, and the caller.
    run.catch(() => {});
    try {
      const outcome = await Promise.race([run.then((value) => ({ value })), cap]);
      if (outcome !== CAPPED) {
        return { status: 'ok', value: outcome.value };
      }
      const stopped = liveNames();
      await closeAllLive();
      return { status: 'capped', stopped };
    } finally {
      clearTimeout(timer);
    }
  }

  // Gives the lock up when a process is queued for it. The gate releases the lock, waits until each queued process has
  // taken it (flock does not queue in arrival order, so a release alone does not hand the lock to the waiter), and queues
  // again behind it. Returns the yield's record, or null when nothing was queued.
  async yieldIfWaited() {
    this.#checkPoll();
    this.#requireOpen();
    const pids = await this.waiters();
    if (pids.length === 0) {
      this.#waitSeenAt = null;
      return null;
    }
    const seenAt = this.#waitSeenAt ?? Date.now();
    await this.release();
    const releasedAt = Date.now();
    await this.#waitForTakers(pids);
    await this.take();
    const record = {
      waiterPids: pids,
      waiterSeenAt: isoOf(seenAt),
      releasedAt: isoOf(releasedAt),
      retakenAt: isoOf(Date.now()),
      seenToReleaseMs: releasedAt - seenAt,
    };
    this.yields.push(record);
    return record;
  }

  async #waitForTakers(pids) {
    const deadline = Date.now() + RELEASE_TAKE_MS;
    for (;;) {
      const queued = (await this.waiters()).filter((pid) => pids.includes(pid));
      if (queued.length === 0) {
        return;
      }
      if (Date.now() >= deadline) {
        throw new Error(`queued process ${queued.join(', ')} did not take the bench lock within ${RELEASE_TAKE_MS} ms of the release`);
      }
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
  }

  // Stops the poll and closes the descriptor. Closing the last copy releases a lock the gate still holds.
  async close() {
    clearInterval(this.#poll);
    this.#poll = null;
    if (this.#pollReading !== null) {
      await this.#pollReading;
    }
    if (this.#fd !== null) {
      fs.closeSync(this.#fd);
      this.#fd = null;
      this.#held = false;
    }
    this.#checkPoll();
  }
}
