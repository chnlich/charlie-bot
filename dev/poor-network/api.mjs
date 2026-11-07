// The bench's own calls to the trial server: the sessions the bench reads, the messages it sends, the task-tree changes
// that stand for a live turn, and the cancel that ends a held turn. These calls go to the server directly, not through
// the shaping proxy: they are the load, not the browser's traffic.

import crypto from 'node:crypto';
import http from 'node:http';

import { STATUS_CHURN, TASK_TREE_LOAD } from './profile.mjs';

const IDLE_QUIET_MS = 3000;
// The bench's own calls answer promptly: a message is queued, a rename and a cancel are single writes, and the widest
// answer is a session's run records. A call that runs past this is a server that stopped answering, and the repetition
// fails with its name rather than holding the trial open.
const REQUEST_TIMEOUT_MS = 60000;

export class ServerApi {
  #base;
  #headers;

  // `url` is the trial's CHARLIEBOT_SERVE_URL; `authHeader` is its CHARLIEBOT_SERVE_AUTH_HEADER, `Name: value`.
  constructor({ url, authHeader }) {
    this.#base = url.replace(/\/$/, '');
    const separator = authHeader.indexOf(':');
    if (separator < 0) {
      throw new Error('CHARLIEBOT_SERVE_AUTH_HEADER must read `Name: value`');
    }
    this.#headers = { [authHeader.slice(0, separator).trim()]: authHeader.slice(separator + 1).trim() };
  }

  get base() {
    return this.#base;
  }

  // The access key the browser needs, from the same header.
  get accessKey() {
    return Object.values(this.#headers)[0].replace(/^Bearer\s+/, '');
  }

  async request(method, path, body) {
    const headers = { ...this.#headers };
    if (body !== undefined) {
      headers['content-type'] = 'application/json';
    }
    const response = await fetch(`${this.#base}${path}`, {
      method,
      headers,
      body: body === undefined ? undefined : JSON.stringify(body),
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    const text = await response.text();
    return { status: response.status, body: text };
  }

  async json(method, path, body) {
    const { status, body: text } = await this.request(method, path, body);
    if (status < 200 || status >= 300) {
      throw new Error(`${method} ${path} answered ${status}: ${text.slice(0, 200)}`);
    }
    return text.length > 0 ? JSON.parse(text) : null;
  }

  // The bytes a session's bootstrap moves on the wire, counted before any decoding: the body's own
  // size, plus the untrimmed size the server names in `x-bootstrap-untrimmed-bytes`. The first
  // screen's trim keeps the served body near 10 KB; the untrimmed size is the session's content
  // size, and the measure the switch targets are sized against. A server without the header (the
  // baseline) serves the untrimmed body, so the body's size is both.
  bootstrapWireBytes(sessionId) {
    const url = new URL(`${this.#base}/api/sessions/${sessionId}/bootstrap`);
    const headers = { ...this.#headers, 'accept-encoding': 'gzip, deflate, br, zstd' };
    return new Promise((resolve, reject) => {
      const request = http.request(url, { headers }, (response) => {
        let bytes = 0;
        response.on('data', (chunk) => {
          bytes += chunk.length;
        });
        response.on('end', () => {
          if (response.statusCode !== 200) {
            reject(new Error(`GET bootstrap of ${sessionId} answered ${response.statusCode}`));
          } else {
            const untrimmed = Number(response.headers['x-bootstrap-untrimmed-bytes']);
            resolve({ wireBytes: bytes, untrimmedBytes: Number.isFinite(untrimmed) && untrimmed > 0 ? untrimmed : bytes });
          }
        });
        response.on('error', reject);
      });
      request.on('error', reject);
      request.end();
    });
  }

  // Creates a manager root on `backend`, a task-tree node that accepts messages. `slots` are registered metadata slot
  // values, such as a Slack origin. Returns its id and the key of the sidebar group that lists it: the row's `group`,
  // or '' for a row without one, as groupSessionsBySortedKeys in web/static/js/sidebar/groups.js keys it.
  async createSession(name, backend, slots = {}) {
    const meta = await this.json('POST', '/api/sessions/', { name, backend, ...slots });
    return { id: meta.id, group: meta.group || '' };
  }

  // Sends one user message, as the composer does. Returns the server's answer time in milliseconds.
  async sendMessage(sessionId, content) {
    const started = performance.now();
    await this.json('POST', `/api/chat/${sessionId}/message`, {
      content,
      request_id: crypto.randomUUID(),
    });
    return performance.now() - started;
  }

  // Waits until the session has run a turn and stays without an active run for IDLE_QUIET_MS. A finished run can start
  // a follow-on turn about half a second later, so one idle poll is not enough.
  async waitForIdle(sessionId, timeoutMs) {
    const deadline = performance.now() + timeoutMs;
    let idleSince = null;
    for (;;) {
      const runs = await this.runs(sessionId);
      const idle = runs.length > 0 && runs.every((run) => run.ended_at !== null);
      if (!idle) {
        idleSince = null;
      } else if (idleSince === null) {
        idleSince = performance.now();
      } else if (performance.now() - idleSince >= IDLE_QUIET_MS) {
        return;
      }
      if (performance.now() > deadline) {
        throw new Error(`session ${sessionId} was not idle for ${IDLE_QUIET_MS} ms within ${timeoutMs} ms`);
      }
      await pause(250);
    }
  }

  // Stops the session's active turn, as the chat's stop control does, and waits until none of its runs is still active.
  // The stop is refused, with 404, when no master run is active, so it is sent only while one is.
  async cancelRuns(sessionId) {
    if ((await this.runs(sessionId)).every((run) => run.ended_at !== null)) {
      return;
    }
    await this.json('POST', `/api/chat/${sessionId}/cancel`);
    for (let attempt = 0; attempt < 60; attempt++) {
      const runs = await this.runs(sessionId);
      if (runs.every((run) => run.ended_at !== null)) {
        return;
      }
      await pause(500);
    }
    throw new Error(`runs of ${sessionId} still active 30 s after cancel`);
  }

  // The session's run records, oldest first. A run's `ended_at` is null while the run is active.
  async runs(sessionId) {
    const page = await this.json('GET', `/api/sessions/${sessionId}/runs?order=asc&limit=500`);
    return page.items;
  }

  // Registers one delayed trigger on the session, as a worker's wake-up does, and returns its id. It fires only after
  // `delaySeconds`, so a window the bench measures ends long before it could.
  async scheduleTrigger(sessionId, delaySeconds, message) {
    const reply = await this.json('POST', '/api/internal/schedule-trigger', {
      session_id: sessionId,
      delay_seconds: delaySeconds,
      message,
    });
    return reply.trigger_id;
  }

  // Removes a delayed trigger the bench scheduled, before it fires.
  async cancelTrigger(sessionId, triggerId) {
    await this.json('POST', `/api/internal/triggers/${sessionId}/${triggerId}/cancel`);
  }
}

function pause(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// Changes a session's name at the task-tree rate of a live turn: TASK_TREE_LOAD.changesPerWindow renames every
// windowMs. Each rename is a task-tree change, which the server pushes to every open tab. Returns {stop}; stop waits
// for the last rename in flight.
export function startTaskTreeDriver(api, sessionId) {
  const intervalMs = TASK_TREE_LOAD.windowMs / TASK_TREE_LOAD.changesPerWindow;
  const started = performance.now();
  let count = 0;
  let stopped = false;
  let inFlight = Promise.resolve();
  let timer = null;
  const tick = () => {
    if (stopped) {
      return;
    }
    count += 1;
    const name = `bench rename ${count}`;
    inFlight = api.json('PATCH', `/api/sessions/${sessionId}`, { name }).then(() => {});
    const due = started + count * intervalMs;
    timer = setTimeout(tick, Math.max(0, due - performance.now()));
  };
  tick();
  return {
    async stop() {
      stopped = true;
      clearTimeout(timer);
      await inFlight;
      return count;
    },
  };
}

// The status churn a live tab meets with no event to tell it: a delayed trigger appears on a painted row and is
// cancelled, so `has_pending_trigger`, `pending_trigger_count` and `next_trigger_at` move and nothing broadcasts the
// move. The page learns of it on its next poll — the poll that still costs a whole body — which is the churn the delta
// exists for. A step that fails fails the repetition: a churn that stopped early would report a share the load did not
// produce.
//
// Returns {stop, flips}; stop cancels the trigger it holds and waits for the last step.
export function startTriggerChurn(api, sessionId) {
  const intervalMs = STATUS_CHURN.windowMs / STATUS_CHURN.triggerFlipsPerWindow;
  let held = null;
  let stopped = false;
  let flips = 0;
  let failure = null;
  let step = Promise.resolve();
  const loop = async () => {
    while (!stopped) {
      const startedAt = performance.now();
      if (held) {
        await api.cancelTrigger(sessionId, held);
        held = null;
      } else {
        held = await api.scheduleTrigger(sessionId, STATUS_CHURN.triggerDelaySeconds, `bench-churn ${flips}`);
      }
      flips += 1;
      await pause(Math.max(50, intervalMs - (performance.now() - startedAt)));
    }
  };
  step = loop().catch((error) => {
    failure = error;
  });
  return {
    async stop() {
      stopped = true;
      await step;
      if (failure) {
        throw new Error(`the trigger churn stopped after ${flips} flips: ${failure.message}`);
      }
      if (held) {
        await api.cancelTrigger(sessionId, held);
      }
      return flips;
    },
  };
}

// The status churn a live turn broadcasts: a held turn starts, and the next step cancels it, so `thinking_since`,
// `has_running_tasks` and `work_state` move and the page is told at once. A step that fails fails the repetition.
//
// Returns {stop, flips}; stop cancels a held turn and waits for the last step.
export function startTurnChurn(api, sessionId) {
  const intervalMs = STATUS_CHURN.windowMs / STATUS_CHURN.turnFlipsPerWindow;
  let running = false;
  let stopped = false;
  let flips = 0;
  let failure = null;
  let step = Promise.resolve();
  const loop = async () => {
    while (!stopped) {
      const startedAt = performance.now();
      if (running) {
        await api.cancelRuns(sessionId);
      } else {
        await api.sendMessage(sessionId, `bench-hold churn ${flips} ${crypto.randomUUID()}`);
      }
      running = !running;
      flips += 1;
      await pause(Math.max(50, intervalMs - (performance.now() - startedAt)));
    }
  };
  step = loop().catch((error) => {
    failure = error;
  });
  return {
    async stop() {
      stopped = true;
      await step;
      if (failure) {
        throw new Error(`the turn churn stopped after ${flips} flips: ${failure.message}`);
      }
      if (running) {
        await api.cancelRuns(sessionId);
      }
      return flips;
    },
  };
}
