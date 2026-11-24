// Tests of the improve trial side record and its check. Each case runs dev/improve/trial-side.sh, the record check
// and dev/improve/github-post.sh for real, with a stub trial, a stub measurement and a stub gh, all under one scratch
// directory. The bench lock is a scratch file, so no case takes the host's lock. A test file's name ends in .test.mjs,
// so the bench-lock naming check in dev/poor-network/lock.test.mjs reads it as a test.

import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { after, test } from 'node:test';
import { fileURLToPath } from 'node:url';

const IMPROVE_DIR = path.dirname(fileURLToPath(import.meta.url));
const TRIAL_SIDE = path.join(IMPROVE_DIR, 'trial-side.sh');
const SIDE_RECORD = path.join(IMPROVE_DIR, 'side-record.sh');
const POSTING = path.join(IMPROVE_DIR, 'github-post.sh');
const COMMIT = '0123456789abcdef0123456789abcdef01234567';
const HEAD_SHA = 'f'.repeat(40);
const DIRECTION = `side_record_test_${process.pid}`;
const MARKER_ROOT = `/tmp/improve-${DIRECTION}`;
const ROOT = fs.mkdtempSync(path.join(os.tmpdir(), 'side-record-'));
const LOCK = path.join(ROOT, 'bench', '.bench.lock');
const STUB_TRIAL = path.join(ROOT, 'stub-trial.sh');
const MEASURE = path.join(ROOT, 'measure.sh');
const STUB_BIN = path.join(ROOT, 'stub-bin');
const PATTERNS = path.join(ROOT, 'patterns');
const STUB_BODY = path.join(ROOT, 'stub-body.md');
const GH_LOG = path.join(ROOT, 'gh.log');
const LAUNCH = path.join(ROOT, 'launch');
const WORK = path.join(ROOT, 'work');

after(() => {
  fs.rmSync(ROOT, { recursive: true, force: true });
  fs.rmSync(MARKER_ROOT, { recursive: true, force: true });
});

// The stub trial stands in for scripts/release/trial.sh: it runs the command after its -- and exits with its status.
fs.writeFileSync(
  STUB_TRIAL,
  '#!/usr/bin/env bash\nwhile [[ $1 != -- ]]; do\n  shift\ndone\n' +
    'printf \'trial output\\n\' >"$CHARLIEBOT_RELEASE_RUN_DIR/trial-raw.txt"\nshift\nexec "$@"\n',
  {
    mode: 0o755,
  },
);
// The stub measurement writes its label to one raw file in the side's output, then exits with its status.
fs.writeFileSync(MEASURE, '#!/usr/bin/env bash\nprintf \'%s\\n\' "$1" >"$IMPROVE_SIDE_OUTPUT/raw.txt"\nexit "$2"\n', {
  mode: 0o755,
});
// The stub gh logs each call. It answers the draft-state and body reads of pr-merge from STUB_BODY_FILE, and it answers
// the head and the verdict reads with a clean verdict on HEAD_SHA, so a merge reaches the side record check.
fs.mkdirSync(STUB_BIN);
fs.writeFileSync(
  path.join(STUB_BIN, 'gh'),
  [
    '#!/usr/bin/env bash',
    'printf \'%s\\n\' "$*" >>"$GH_LOG"',
    'if [[ "$*" == *"--json isDraft"* ]]; then',
    "  printf 'false\\n'",
    'elif [[ "$*" == *"--json body"* ]]; then',
    '  cat -- "$STUB_BODY_FILE"',
    'elif [[ "$*" == *"--json headRefOid"* ]]; then',
    `  printf '%s\\n' '${HEAD_SHA}'`,
    'elif [[ "$*" == *"--json comments"* ]]; then',
    `  printf 'Review verdict on %s\\n\\nCode review: no issues found.\\n' '${HEAD_SHA}'`,
    'fi',
    '',
  ].join('\n'),
  { mode: 0o755 },
);
fs.writeFileSync(PATTERNS, '# A host pattern for the test.\nzz-no-host-pattern-matches\n');

// gitAt: one git command in the scratch tree. The pinned identity and config keep the commit independent of the
// operator's git configuration.
function gitAt(cwd, args) {
  const env = {
    ...process.env,
    GIT_AUTHOR_NAME: 'Example',
    GIT_AUTHOR_EMAIL: '123+example@users.noreply.github.com',
    GIT_COMMITTER_NAME: 'Example',
    GIT_COMMITTER_EMAIL: '123+example@users.noreply.github.com',
    GIT_CONFIG_GLOBAL: '/dev/null',
    GIT_CONFIG_NOSYSTEM: '1',
  };
  const result = spawnSync('git', args, { cwd, env, encoding: 'utf8' });
  assert.equal(result.status, 0, `git ${args.join(' ')}: ${result.stderr}`);
}

// The round's launch directory with an empty in-flight list, and the scratch checkout whose branch b the pr-create
// cases name: the posting script's claim check reads the list and diffs the head against origin/main.
fs.mkdirSync(LAUNCH);
fs.writeFileSync(path.join(LAUNCH, 'inflight-files.txt'), '');
gitAt(ROOT, ['init', '-q', '-b', 'main', WORK]);
fs.writeFileSync(path.join(WORK, 'seed.txt'), 'seed\n');
gitAt(WORK, ['add', 'seed.txt']);
gitAt(WORK, ['commit', '-q', '-m', 'seed commit']);
gitAt(WORK, ['update-ref', 'refs/remotes/origin/main', 'HEAD']);
gitAt(WORK, ['switch', '-q', '-c', 'b']);

function writeFile(name, content) {
  const file = path.join(ROOT, name);
  fs.writeFileSync(file, content);
  return file;
}

function newRound(prefix) {
  return fs.mkdtempSync(path.join(ROOT, `${prefix}-`));
}

function sha256(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

// The scratch environment of a side: the scratch bench lock and the test direction, with the lock switch unset unless
// the case sets it.
function sideEnv(extra) {
  const env = { ...process.env, IMPROVE_DIRECTION: DIRECTION, BENCH_LOCK_FILE: LOCK };
  delete env.IMPROVE_TRIAL_USE_LOCK;
  return { ...env, ...extra };
}

// runSide: one serve trial side, with the stub trial and measurement, under the given round directory.
function runSide({ run, binary, label, status, env = {} }) {
  return spawnSync(
    'bash',
    [TRIAL_SIDE, 'bash', STUB_TRIAL, '--serve', COMMIT, binary, '--', MEASURE, label, String(status)],
    { env: sideEnv({ CHARLIEBOT_RELEASE_RUN_DIR: run, ...env }), encoding: 'utf8' },
  );
}

// sideRecords: the side record files of a round directory.
function sideRecords(run) {
  if (!fs.existsSync(run)) {
    return [];
  }
  return fs
    .readdirSync(run)
    .filter((name) => name.startsWith('side-record-'))
    .map((name) => path.join(run, name));
}

// recordFields: the key=value lines of one side record, as an object.
function recordFields(file) {
  const fields = {};
  for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
    const at = line.indexOf('=');
    if (at > 0) {
      fields[line.slice(0, at)] = line.slice(at + 1);
    }
  }
  return fields;
}

// checkBodyText: run the record check on a body with the given text. draft is 1 for a draft pull request.
function checkBodyText(run, text, draft) {
  const body = writeFile(`body-${crypto.randomUUID()}.md`, text);
  return spawnSync(
    'bash',
    ['-c', 'source "$1"; side_record_check_body "$2" "$3" "$4"', 'bash', SIDE_RECORD, run, body, String(draft)],
    { encoding: 'utf8' },
  );
}

// checkBody: run the record check on a body that cites the named records. draft is 1 for a draft pull request.
function checkBody(run, citations, draft) {
  return checkBodyText(run, citations.map((name) => `Side record: ${name}\n`).join(''), draft);
}

// The accepted citation form, which each refusal of a near-miss citation names.
const ACCEPTED_FORM = 'Side record: <record file name>';

// post: run the posting script under the stub gh, in the scratch checkout. It returns the exit status, the stderr
// and the stub's gh calls.
function post(args, body = '') {
  fs.writeFileSync(STUB_BODY, body);
  fs.rmSync(GH_LOG, { force: true });
  const result = spawnSync(POSTING, args, {
    cwd: WORK,
    env: {
      ...process.env,
      IMPROVE_LAUNCH_DIR: LAUNCH,
      PATH: `${STUB_BIN}:${process.env.PATH}`,
      GH_LOG,
      STUB_BODY_FILE: STUB_BODY,
      EXTRA_PATTERNS_FILE: PATTERNS,
    },
    encoding: 'utf8',
  });
  const calls = fs.existsSync(GH_LOG) ? fs.readFileSync(GH_LOG, 'utf8').trim().split('\n') : [];
  return { status: result.status, stderr: result.stderr, calls };
}

async function waitFor(check) {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    if (check()) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error('the lock holder did not take the lock');
}

// holdLock: take the scratch bench lock in a child process until the returned release is called. Each holder names its
// own marker files, so a release file of an earlier holder never ends a later one.
async function holdLock() {
  const holder = crypto.randomUUID();
  const held = path.join(ROOT, `lock-held-${holder}`);
  const release = path.join(ROOT, `lock-release-${holder}`);
  fs.mkdirSync(path.dirname(LOCK), { recursive: true });
  const child = spawn(
    'bash',
    ['-c', 'exec 9>"$1"; flock -x 9; : >"$2"; while [[ ! -e "$3" ]]; do sleep 0.05; done', 'bash', LOCK, held, release],
    { stdio: 'ignore' },
  );
  await waitFor(() => fs.existsSync(held));
  return async () => {
    fs.writeFileSync(release, '');
    await new Promise((resolve) => child.once('exit', resolve));
  };
}

const binaryOf = (name, content) => writeFile(name, content);

test('two serve trial sides write two records, each with its own output location', () => {
  const run = newRound('two');
  const binary = binaryOf('server-two', 'server build one\n');
  assert.equal(runSide({ run, binary, label: 'before', status: 0 }).status, 0);
  assert.equal(runSide({ run, binary, label: 'after', status: 0 }).status, 0);
  const fields = sideRecords(run).map(recordFields);
  assert.equal(fields.length, 2);
  for (const record of fields) {
    assert.equal(record.commit, COMMIT);
    assert.equal(record.binary_sha256, sha256(binary));
    assert.equal(record.exit_status, '0');
  }
  const outputs = fields.map((record) => record.output);
  assert.equal(new Set(outputs).size, 2);
  const raw = outputs.map((output) => fs.readFileSync(path.join(output, 'raw.txt'), 'utf8').trim());
  assert.deepEqual(raw.sort(), ['after', 'before']);
  const trialRaw = outputs.map((output) => fs.readFileSync(path.join(output, 'trial-raw.txt'), 'utf8').trim());
  assert.deepEqual(trialRaw, ['trial output', 'trial output']);
});

test('a binary that changes between two sides gives two different digests', () => {
  const run = newRound('changed');
  const binary = binaryOf('server-changed', 'server build one\n');
  const first = sha256(binary);
  assert.equal(runSide({ run, binary, label: 'one', status: 0 }).status, 0);
  fs.writeFileSync(binary, 'server build two\n');
  const second = sha256(binary);
  assert.equal(runSide({ run, binary, label: 'two', status: 0 }).status, 0);
  const digests = sideRecords(run)
    .map((file) => recordFields(file).binary_sha256)
    .sort();
  assert.deepEqual(digests, [first, second].sort());
});

test('a failing side passes its exit status on and records it', () => {
  const run = newRound('exit');
  const binary = binaryOf('server-exit', 'server build\n');
  const side = runSide({ run, binary, label: 'failing', status: 3 });
  assert.equal(side.status, 3, side.stderr);
  const [record] = sideRecords(run).map(recordFields);
  assert.equal(record.exit_status, '3');
});

test('a lock-off ux side writes its side record beside its bench-lock record', () => {
  const run = newRound('lockless');
  const binary = binaryOf('server-lockless', 'server build\n');
  const side = spawnSync(
    'bash',
    [TRIAL_SIDE, 'bash', STUB_TRIAL, '--serve', COMMIT, binary, '--', MEASURE, 'lockless', '0'],
    {
      env: sideEnv({ IMPROVE_DIRECTION: 'ux', IMPROVE_TRIAL_USE_LOCK: '0', CHARLIEBOT_RELEASE_RUN_DIR: run }),
      encoding: 'utf8',
    },
  );
  assert.equal(side.status, 0, side.stderr);
  const records = sideRecords(run);
  assert.equal(records.length, 1);
  const fields = recordFields(records[0]);
  assert.equal(fields.exit_status, '0');
  assert.equal(fs.readFileSync(path.join(fields.output, 'trial-raw.txt'), 'utf8').trim(), 'trial output');
  const benchRecords = fs.readdirSync(run).filter((name) => name.startsWith('bench-lock-'));
  assert.equal(benchRecords.length, 1);
});

test('a lock-off side records a failed binary hash as its exit status', () => {
  const run = newRound('lockless-missing-binary');
  const binary = path.join(run, 'missing-server');
  const side = spawnSync(
    'bash',
    [TRIAL_SIDE, 'bash', STUB_TRIAL, '--serve', COMMIT, binary, '--', MEASURE, 'missing', '0'],
    {
      env: sideEnv({ IMPROVE_DIRECTION: 'ux', IMPROVE_TRIAL_USE_LOCK: '0', CHARLIEBOT_RELEASE_RUN_DIR: run }),
      encoding: 'utf8',
    },
  );
  assert.equal(side.status, 1);
  const records = sideRecords(run);
  assert.equal(records.length, 1);
  const fields = recordFields(records[0]);
  assert.equal(fields.exit_status, '1');
  assert.equal(fields.binary_sha256, undefined);
});

test('a command without --serve writes no side record', () => {
  const run = newRound('plain');
  const side = spawnSync(TRIAL_SIDE, ['true'], { env: sideEnv({ CHARLIEBOT_RELEASE_RUN_DIR: run }), encoding: 'utf8' });
  assert.equal(side.status, 0, side.stderr);
  assert.deepEqual(sideRecords(run), []);
});

test('a serve trial with a malformed commit exits 2 before it takes the lock', () => {
  const run = newRound('malformed');
  const binary = binaryOf('server-malformed', 'server build\n');
  const side = spawnSync(
    'bash',
    [TRIAL_SIDE, 'bash', STUB_TRIAL, '--serve', 'not-a-sha', binary, '--', MEASURE, 'bad', '0'],
    { env: sideEnv({ CHARLIEBOT_RELEASE_RUN_DIR: run }), encoding: 'utf8' },
  );
  assert.equal(side.status, 2);
  assert.deepEqual(sideRecords(run), []);
});

test('a side that waits out the bench lock leaves a record with exit 75 and no binary digest', async () => {
  const release = await holdLock();
  try {
    const run = newRound('timed-out');
    const binary = binaryOf('server-timed-out', 'server build\n');
    const side = runSide({
      run,
      binary,
      label: 'timed-out',
      status: 0,
      env: { IMPROVE_TRIAL_LOCK_WAIT_SECONDS: '1' },
    });
    assert.equal(side.status, 75, side.stderr);
    const [record] = sideRecords(run).map(recordFields);
    assert.equal(record.exit_status, '75');
    assert.equal(record.binary_sha256, undefined);
  } finally {
    await release();
  }
});

test('the record check passes a complete record for a ready and for a draft pull request', () => {
  const run = newRound('complete');
  const binary = binaryOf('server-complete', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'complete', status: 0 }).status, 0);
  const [name] = sideRecords(run).map((file) => path.basename(file));
  assert.equal(checkBody(run, [name], 0).status, 0);
  assert.equal(checkBody(run, [name], 1).status, 0);
});

test('the record check refuses a cited record that is missing', () => {
  const run = newRound('missing');
  const binary = binaryOf('server-missing', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'present', status: 0 }).status, 0);
  const side = checkBody(run, ['side-record-19700101T000000Z-zzzzzz'], 0);
  assert.equal(side.status, 1);
  assert.match(side.stderr, /the cited side record is missing/);
});

test('the record check refuses a cited record whose raw output location is empty', () => {
  const run = newRound('empty-output');
  const binary = binaryOf('server-empty', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'emptied', status: 0 }).status, 0);
  const [record] = sideRecords(run);
  fs.rmSync(recordFields(record).output, { recursive: true });
  fs.mkdirSync(recordFields(record).output);
  const side = checkBody(run, [path.basename(record)], 0);
  assert.equal(side.status, 1);
  assert.match(side.stderr, /the raw output location is empty/);
});

test('the record check refuses a side record that did not end', () => {
  const run = newRound('killed');
  const binary = binaryOf('server-killed', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'killed', status: 0 }).status, 0);
  const [record] = sideRecords(run);
  const kept = fs.readFileSync(record, 'utf8').split('\n').filter((line) => !line.startsWith('exit_status='));
  fs.writeFileSync(record, kept.join('\n'));
  const side = checkBody(run, [path.basename(record)], 0);
  assert.equal(side.status, 1);
  assert.match(side.stderr, /the side did not end/);
});

test('the record check refuses a cited record of a side that exited non-zero', () => {
  const run = newRound('failed');
  const binary = binaryOf('server-failed', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'failed', status: 4 }).status, 4);
  const [record] = sideRecords(run);
  const side = checkBody(run, [path.basename(record)], 0);
  assert.equal(side.status, 1);
  assert.match(side.stderr, /the side exited 4/);
});

test('the record check refuses a timed-out record, which has no binary digest', async () => {
  const release = await holdLock();
  try {
    const run = newRound('timed-out-check');
    const binary = binaryOf('server-timed-out-check', 'server build\n');
    runSide({ run, binary, label: 'timed-out', status: 0, env: { IMPROVE_TRIAL_LOCK_WAIT_SECONDS: '1' } });
    const [record] = sideRecords(run);
    const side = checkBody(run, [path.basename(record)], 0);
    assert.equal(side.status, 1);
    assert.match(side.stderr, /no binary_sha256/);
  } finally {
    await release();
  }
});

test('a ready body names a record when the round directory holds one, and a draft body may name none', () => {
  const run = newRound('names');
  const binary = binaryOf('server-names', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'named', status: 0 }).status, 0);
  const ready = checkBody(run, [], 0);
  assert.equal(ready.status, 1);
  assert.match(ready.stderr, /names no side record/);
  assert.equal(checkBody(run, [], 1).status, 0);
  assert.equal(checkBody(newRound('no-records'), [], 0).status, 0);
});

test('the record check refuses a cited name that is a path rather than a record file in the round directory', () => {
  const run = newRound('path-name');
  const binary = binaryOf('server-path', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'pathed', status: 0 }).status, 0);
  const [record] = sideRecords(run);
  const side = checkBody(run, [`../${path.basename(record)}`], 0);
  assert.equal(side.status, 1);
  assert.match(side.stderr, /a side-record file name in the evidence directory/);
});

test('the record check refuses each near-miss citation in draft and ready bodies, and names the form', () => {
  const run = newRound('near-miss');
  const binary = binaryOf('server-near-miss', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'near', status: 0 }).status, 0);
  const [name] = sideRecords(run).map((file) => path.basename(file));
  const nearMisses = [`- Side record: ${name}`, `Side record: ${name} (after, touch)`, 'Side record:'];
  for (const line of nearMisses) {
    for (const draft of [0, 1]) {
      const side = checkBodyText(run, `${line}\n`, draft);
      assert.equal(side.status, 1, `${line} with draft ${draft}`);
      assert.ok(side.stderr.includes(`"${line}"`), side.stderr);
      assert.ok(side.stderr.includes(ACCEPTED_FORM), side.stderr);
    }
    const withoutDir = checkBodyText('', `${line}\n`, 0);
    assert.equal(withoutDir.status, 1, `${line} without --evidence-dir`);
    assert.ok(withoutDir.stderr.includes(ACCEPTED_FORM), withoutDir.stderr);
  }
});

test('pr-create refuses a body that cites an incomplete record, and gh is not called', () => {
  const run = newRound('post-refuse');
  const binary = binaryOf('server-post-refuse', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'posted', status: 0 }).status, 0);
  const [record] = sideRecords(run);
  const kept = fs.readFileSync(record, 'utf8').split('\n').filter((line) => !line.startsWith('exit_status='));
  fs.writeFileSync(record, kept.join('\n'));
  const body = writeFile('pr-body-refuse.md', `## Evidence\n\nSide record: ${path.basename(record)}\n`);
  const args = [
    'pr-create', '--title', 'change', '--body-file', body, '--evidence-dir', run, '--base', 'main', '--head', 'b',
  ];
  const result = post(args);
  assert.equal(result.status, 2);
  assert.deepEqual(result.calls, []);
});

test('pr-create sends gh the call without the --evidence-dir argument', () => {
  const run = newRound('post-send');
  const binary = binaryOf('server-post-send', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'sent', status: 0 }).status, 0);
  const [record] = sideRecords(run);
  const body = writeFile('pr-body-send.md', `## Evidence\n\nSide record: ${path.basename(record)}\n`);
  const args = [
    'pr-create', '--title', 'change', '--body-file', body, '--evidence-dir', run, '--base', 'main', '--head', 'b',
  ];
  const result = post(args);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.calls.filter((call) => call.startsWith('pr create --title change ')).length, 1);
  assert.equal(result.calls.filter((call) => call.includes('--evidence-dir')).length, 0);
});

test('a draft pr-create sends without a cited record', () => {
  const run = newRound('post-draft');
  const binary = binaryOf('server-post-draft', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'pending', status: 0 }).status, 0);
  const body = writeFile('pr-body-draft.md', '## Evidence\n\nPoor-network rows pending.\n');
  const args = ['pr-create', '--title', 'change', '--body-file', body, '--evidence-dir', run, '--draft'];
  const result = post([...args, '--base', 'main', '--head', 'b']);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.calls.filter((call) => call.includes('--draft')).length, 1);
});

test('pr-create refuses a draft body whose citation has a list marker, and gh is not called', () => {
  const run = newRound('post-list-marked');
  const binary = binaryOf('server-post-list-marked', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'listed', status: 0 }).status, 0);
  const [record] = sideRecords(run);
  const body = writeFile('pr-body-list-marked.md', `## Evidence\n\n- Side record: ${path.basename(record)}\n`);
  const args = [
    'pr-create', '--title', 'change', '--body-file', body, '--evidence-dir', run, '--base', 'main', '--head', 'b',
  ];
  const result = post([...args, '--draft']);
  assert.equal(result.status, 2);
  assert.ok(result.stderr.includes(ACCEPTED_FORM), result.stderr);
  assert.deepEqual(result.calls, []);
});

test('pr-merge refuses a ready body that names no record while the round directory holds one', () => {
  const run = newRound('merge-names');
  const binary = binaryOf('server-merge-names', 'server build\n');
  assert.equal(runSide({ run, binary, label: 'merged', status: 0 }).status, 0);
  const result = post(['pr-merge', '--evidence-dir', run, '7'], '## Evidence\n\nNo record named.\n');
  assert.equal(result.status, 2);
  assert.match(result.stderr, /names no side record/);
  assert.equal(result.calls.filter((call) => call.startsWith('pr merge')).length, 0);
});

test('pr-merge refuses a body that cites a missing record', () => {
  const run = newRound('merge-missing');
  const body = '## Evidence\n\nSide record: side-record-19700101T000000Z-zzzzzz\n';
  const result = post(['pr-merge', '--evidence-dir', run, '7'], body);
  assert.equal(result.status, 2);
  assert.match(result.stderr, /the cited side record is missing/);
  assert.equal(result.calls.filter((call) => call.startsWith('pr merge')).length, 0);
});

test('pr-merge without --evidence-dir refuses a body that cites a side record, and merges nothing', () => {
  const body = '## Evidence\n\nSide record: side-record-19700101T000000Z-zzzzzz\n';
  const result = post(['pr-merge', '7'], body);
  assert.equal(result.status, 2);
  assert.match(result.stderr, /needs --evidence-dir/);
  assert.equal(result.calls.filter((call) => call.startsWith('pr merge')).length, 0);
});

test('pr-create without --evidence-dir sends a body that cites no record', () => {
  const body = writeFile('pr-body-nodir.md', '## Evidence\n\nNo timing figure.\n');
  const result = post(['pr-create', '--title', 'change', '--body-file', body, '--base', 'main', '--head', 'b']);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(result.calls.filter((call) => call.startsWith('pr create --title change ')).length, 1);
});

test('pr-create without --evidence-dir refuses a body that cites a side record, and gh is not called', () => {
  const body = writeFile('pr-body-nodir-cite.md', '## Evidence\n\nSide record: side-record-19700101T000000Z-zzzzzz\n');
  const result = post(['pr-create', '--title', 'change', '--body-file', body, '--base', 'main', '--head', 'b']);
  assert.equal(result.status, 2);
  assert.match(result.stderr, /needs --evidence-dir/);
  assert.deepEqual(result.calls, []);
});
