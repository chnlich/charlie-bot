// One segment of the bench: the command a serve trial runs.
//
// The trial starts one build's server on its own home, then runs this command with CHARLIEBOT_SERVE_URL,
// CHARLIEBOT_SERVE_AUTH_HEADER and CHARLIEBOT_HOME set. This command registers the waits that start inside the
// segment, starts their jobs inside the container, and ends the segment at its scheduled offset. Trigger
// records carry the server's own creation and fire times, so the report reads the wake straight from them.
//
// The segment ends by copying three things out of the trial tree, which the trial deletes when the command ends:
// the trigger records (the wake times), the server log (the per-wake ssh process count), and the whole session
// directory (the next segment's home, so pending triggers survive the restart).

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

const API_TIMEOUT_MS = 180000;
const SESSIONS_PER_SESSION_LIMIT = 4;
const RECORD_POLL_MS = 250;

function parseArgs(argv) {
  const args = {};
  for (let index = 0; index < argv.length; index += 2) {
    args[argv[index].replace(/^--/, '')] = argv[index + 1];
  }
  for (const required of ['run-dir', 'build', 'segment', 'container', 'user', 'alias', 'backend']) {
    if (args[required] === undefined) {
      throw new Error(`--${required} is required`);
    }
  }
  args.segment = Number(args.segment);
  return args;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function epochMs() {
  return Date.now();
}

function docker(args, options = {}) {
  return execFileSync('docker', args, { encoding: 'utf8', maxBuffer: 16 * 1024 * 1024, ...options });
}

function jsonRead(file) {
  return JSON.parse(fs.readFileSync(file, 'utf8'));
}

function jsonWrite(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, `${JSON.stringify(value, null, 2)}\n`);
}

function jsonlAppend(file, value) {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.appendFileSync(file, `${JSON.stringify(value)}\n`);
}

async function httpJson(url, authHeader, method, route, body) {
  const headers = { authorization: authHeader.replace(/^Authorization:\s*/i, '') };
  if (body !== undefined) {
    headers['content-type'] = 'application/json';
  }
  const response = await fetch(`${url}${route}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(API_TIMEOUT_MS),
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`${method} ${route} answered ${response.status}: ${text.slice(0, 500)}`);
  }
  return text.length > 0 ? JSON.parse(text) : null;
}

async function waitForT0(runDir) {
  const file = path.join(runDir, 't0');
  const deadline = Date.now() + 300000;
  for (;;) {
    if (fs.existsSync(file)) {
      return Number(fs.readFileSync(file, 'utf8'));
    }
    if (Date.now() > deadline) {
      throw new Error(`the orchestrator did not write ${file}`);
    }
    await sleep(100);
  }
}

async function createSessions(api, args, plan, count) {
  const file = path.join(args['run-dir'], `sessions-${args.build}.json`);
  if (fs.existsSync(file)) {
    return jsonRead(file);
  }
  const sessions = [];
  for (let index = 0; index < count; index++) {
    const name = `rw-${args.build}-${index}`;
    const created = await api('POST', '/api/sessions/', { name, backend: args.backend });
    sessions.push({ id: created.id, name });
  }
  jsonWrite(file, sessions);
  return sessions;
}

function startJob(args, wait) {
  const exitFile = wait.kind === 'cmd' ? `/home/${args.user}/charliebot_runs/${wait.jobId}/exit` : '';
  docker([
    'exec', '-u', args.user, args.container, 'sh', '-c',
    'setsid sh /usr/local/bin/bench-job "$1" "$2" "$3" </dev/null >/dev/null 2>&1 & echo $!',
    'sh', String(wait.jobId), String(wait.length), exitFile,
  ]);
  return exitFile;
}

async function jobPid(args, wait) {
  const file = `/run/bench/${args.user}/jobs/${wait.jobId}/pid`;
  const deadline = Date.now() + 5000;
  for (;;) {
    try {
      const pid = docker(['exec', '-u', args.user, args.container, 'cat', file]).trim();
      if (pid.length > 0) {
        return Number(pid);
      }
    } catch {
      // The job has not written its pid file yet; a missing file is the one expected error here.
    }
    if (Date.now() > deadline) {
      throw new Error(`job ${wait.jobId} wrote no pid file in ${file}`);
    }
    await sleep(50);
  }
}

function watchTarget(args, wait, pid, exitFile) {
  if (wait.kind === 'slurm') {
    return { kind: 'slurm_job', host: args.alias, job_id: wait.jobId };
  }
  const target = { kind: 'remote_pid', host: args.alias, pid };
  if (wait.kind === 'cmd') {
    target.exit_file = exitFile;
  }
  return target;
}

// Wait until the trigger record exists, so its server-side created_at is the wait's registration time.
async function registrationRecord(api, args, sessionId, triggerId) {
  const file = path.join(process.env.CHARLIEBOT_HOME, 'sessions', sessionId, 'triggers', `${triggerId}.json`);
  const deadline = Date.now() + 10000;
  for (;;) {
    if (fs.existsSync(file)) {
      return jsonRead(file);
    }
    if (Date.now() > deadline) {
      throw new Error(`trigger ${triggerId} wrote no record at ${file}`);
    }
    await sleep(50);
  }
}

async function registerWait(api, args, plan, wait, sessions) {
  const session = sessions[wait.index % sessions.length];
  const exitFile = startJob(args, wait);
  const pid = await jobPid(args, wait);
  const message = `rw ${args.build} wait ${wait.index} ${wait.kind}`;
  // The request time opens the wake's connection window: the verify-on-create probe is made while the request is in
  // flight, before the server writes the record's own creation time.
  const requestedAt = new Date().toISOString();
  const reply = await api('POST', '/api/internal/schedule-trigger', {
    session_id: session.id,
    delay_seconds: wait.maxWait,
    message,
    watch_targets: [watchTarget(args, wait, pid, exitFile)],
  });
  const record = await registrationRecord(api, args, session.id, reply.trigger_id);
  jsonlAppend(path.join(args['run-dir'], `registrations-${args.build}.jsonl`), {
    build: args.build,
    segment: args.segment,
    wait: wait.index,
    kind: wait.kind,
    length: wait.length,
    start_offset: wait.startOffset,
    job_id: wait.jobId,
    pid,
    exit_file: exitFile,
    session_id: session.id,
    trigger_id: reply.trigger_id,
    created_at: record.created_at,
    registered_at: requestedAt,
  });
}

async function applyFaults(args, plan, t0, segment) {
  const tasks = plan.faults
    .filter((fault) => fault.owner === args.build)
    .filter((fault) => fault.startOffset >= segment.startOffset && fault.startOffset < segment.endOffset)
    .map(async (fault) => {
      const startAt = t0 + fault.startOffset * 1000;
      if (Date.now() < startAt) {
        await sleep(startAt - Date.now());
      }
      const wait = plan.waits.find((item) => item.index === fault.waitIndex);
      const elapsed = (Date.now() - t0) / 1000;
      if (!(wait.startOffset < elapsed && elapsed < wait.startOffset + wait.length)) {
        throw new Error(`fault ${fault.kind} on wait ${fault.waitIndex} started outside its job window`);
      }
      if (fault.kind === 'cut') {
        docker(['exec', args.container, 'iptables', '-I', 'INPUT', '-p', 'tcp', '--dport', '22', '-j', 'DROP']);
      } else {
        docker([
          'exec', args.container, 'iptables', '-I', 'INPUT', '-p', 'tcp', '--dport', '22', '--syn', '-j', 'DROP',
        ]);
      }
      jsonlAppend(path.join(args['run-dir'], 'faults.jsonl'), {
        kind: fault.kind,
        wait_index: fault.waitIndex,
        started_at: new Date().toISOString(),
        seconds: fault.seconds,
        applied: true,
      });
      await sleep(fault.seconds * 1000);
      if (fault.kind === 'cut') {
        docker(['exec', args.container, 'iptables', '-D', 'INPUT', '-p', 'tcp', '--dport', '22', '-j', 'DROP']);
      } else {
        docker([
          'exec', args.container, 'iptables', '-D', 'INPUT', '-p', 'tcp', '--dport', '22', '--syn', '-j', 'DROP',
        ]);
      }
      jsonlAppend(path.join(args['run-dir'], 'faults.jsonl'), {
        kind: fault.kind,
        wait_index: fault.waitIndex,
        started_at: new Date().toISOString(),
        seconds: fault.seconds,
        applied: false,
      });
    });
  await Promise.all(tasks);
}

function triggerDir(args, sessionId) {
  return path.join(process.env.CHARLIEBOT_HOME, 'sessions', sessionId, 'triggers');
}

function readRecords(args, sessions) {
  const records = [];
  for (const session of sessions) {
    const dir = triggerDir(args, session.id);
    if (!fs.existsSync(dir)) {
      continue;
    }
    for (const name of fs.readdirSync(dir)) {
      if (!name.endsWith('.json')) {
        continue;
      }
      const record = jsonRead(path.join(dir, name));
      records.push({ ...record, session_id: session.id });
    }
  }
  return records;
}

// Every record this build registered has reached a terminal state, so no wake is still waiting on the probe
// schedule. A record stays pending when its job has not ended yet, which the caller's deadline covers.
function allSettled(args, sessions) {
  return readRecords(args, sessions).every((record) => record.status !== 'pending');
}

function copyTree(source, target) {
  fs.rmSync(target, { recursive: true, force: true });
  fs.cpSync(source, target, { recursive: true, preserveTimestamps: true });
}

// The next segment copies these session directories back, so a turn still writing its chat log would hand over a
// truncated file. The scripted turns end within a second, so a short poll settles every run first.
async function waitForIdleRuns(api, sessions) {
  const deadline = Date.now() + 120000;
  for (;;) {
    const running = [];
    for (const session of sessions) {
      const page = await api('GET', `/api/sessions/${session.id}/runs?order=asc&limit=500`);
      if (page.items.some((run) => run.ended_at === null)) {
        running.push(session.id);
      }
    }
    if (running.length === 0 || Date.now() > deadline) {
      return;
    }
    await sleep(500);
  }
}

async function collect(api, args, plan, sessions, segment) {
  await waitForIdleRuns(api, sessions);
  const runDir = args['run-dir'];
  const snapshotDir = path.join(runDir, 'snapshots', `${args.build}-${args.segment}`);
  fs.mkdirSync(snapshotDir, { recursive: true });
  jsonWrite(path.join(snapshotDir, 'triggers.json'), readRecords(args, sessions));

  const tree = path.dirname(path.dirname(process.env.CHARLIEBOT_HOME));
  const logs = fs.readdirSync(tree).filter((name) => /^server-\d+\.log$/.test(name));
  const logDir = path.join(runDir, 'logs');
  fs.mkdirSync(logDir, { recursive: true });
  for (const name of logs) {
    fs.copyFileSync(path.join(tree, name), path.join(logDir, `${args.build}-${args.segment}-${name}`));
  }

  if (segment.index + 1 < plan.segments.length) {
    // The next trial's --seed-home copies each immediate child directory as one session, so the session directories go
    // directly under the seed home.
    const seed = path.join(runDir, 'seed', args.build, String(args.segment + 1));
    fs.mkdirSync(seed, { recursive: true });
    copyTree(path.join(process.env.CHARLIEBOT_HOME, 'sessions'), seed);
  }
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const runDir = args['run-dir'];
  const plan = jsonRead(path.join(runDir, 'plan.json'));
  const segment = plan.segments[args.segment];
  if (!segment) {
    throw new Error(`the plan has no segment ${args.segment}`);
  }
  const api = (method, route, body) => httpJson(process.env.CHARLIEBOT_SERVE_URL, process.env.CHARLIEBOT_SERVE_AUTH_HEADER, method, route, body);

  // The command runs only after the trial's server answers, so this marker is also the server's start time.
  fs.mkdirSync(path.join(runDir, 'started'), { recursive: true });
  fs.writeFileSync(path.join(runDir, 'started', `${args.build}-${args.segment}`), String(epochMs()));

  const t0 = await waitForT0(runDir);
  const sessionCount = Math.max(1, Math.ceil(plan.waits.length / SESSIONS_PER_SESSION_LIMIT));
  const sessions = await createSessions(api, args, plan, sessionCount);

  fs.mkdirSync(path.join(runDir, 'ready'), { recursive: true });
  fs.writeFileSync(path.join(runDir, 'ready', `${args.build}-${args.segment}`), String(epochMs()));
  const startedAt = new Date().toISOString();
  const registrations = [];
  const faultRun = applyFaults(args, plan, t0, segment);
  for (const wait of plan.waits) {
    if (wait.startOffset < segment.startOffset || wait.startOffset >= segment.endOffset) {
      continue;
    }
    const dueAt = t0 + wait.startOffset * 1000;
    if (Date.now() < dueAt) {
      await sleep(dueAt - Date.now());
    }
    await registerWait(api, args, plan, wait, sessions);
    registrations.push(wait.index);
  }
  await faultRun;

  const last = args.segment === plan.segments.length - 1;
  if (last) {
    while (!allSettled(args, sessions) && Date.now() < t0 + segment.endOffset * 1000) {
      await sleep(RECORD_POLL_MS);
    }
  } else {
    const endAt = t0 + segment.endOffset * 1000;
    if (Date.now() < endAt) {
      await sleep(endAt - Date.now());
    }
  }

  await collect(api, args, plan, sessions, segment);
  jsonWrite(path.join(runDir, 'segments', `${args.build}-${args.segment}.json`), {
    build: args.build,
    segment: args.segment,
    started_at: startedAt,
    ended_at: new Date().toISOString(),
    registrations,
  });
}

main().catch((error) => {
  console.error(`remote-wake segment: ${error.stack ?? error}`);
  process.exit(1);
});
