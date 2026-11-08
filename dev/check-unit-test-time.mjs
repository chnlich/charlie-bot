#!/usr/bin/env node
// Reads the parallel JUnit report and reruns each test above the parallel limit alone, except an allowed test.
// If its first alone time is over 0.1 s, the check runs it alone twice more and judges by the shortest time.
// A missing JUnit row fails.
//
// Two ceilings. The 0.1 s ceiling judges the tests that a change can speed up. A test that waits on a real timeout
// cannot finish sooner than that timeout, so it is listed by name in ALLOWED_OVER_LIMIT, with its reason, and keeps
// the coarser 1.0 s ceiling. The check judges an allowed test by its JUnit time and never reruns it alone.
//
//   dev/check-unit-test-time.mjs --runner rust|node --junit <path> [--parallel-limit <seconds>] [--workspace-dir <dir>]
//
//   --runner            the suite the report came from: rust (nextest) or node (node --test) (required)
//   --junit             the JUnit report of the parallel run (required)
//   --parallel-limit    rerun tests above this parallel time alone, allowed tests excepted (default: 0.1)
//   --workspace-dir     the directory the reruns run in (default: rust for rust, web for node)
//
// A rust rerun is one nextest run, one test thread. A node rerun is one `node --test` per suspect, on the
// suspect's own file, with `--test-name-pattern` set to its name, escaped and anchored at both ends.
// Lines go to stdout, and to $GITHUB_STEP_SUMMARY when the environment sets it.
import { spawnSync } from 'node:child_process';
import { appendFileSync, existsSync, mkdtempSync, readFileSync, rmSync } from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import process from 'node:process';

const LIMIT_S = 0.1;
const ALLOWED_LIMIT_S = 1.0;
const RERUN_TIMEOUT_MS = 55 * 60_000;
const RUNNER_DIRECTORY = { rust: 'rust', node: 'web' };

// The tests allowed over LIMIT_S. Each entry holds the classname (binaryId) and the name that the JUnit report
// gives the test, which the check prints as `<binaryId> <name>`. An allowed test skips the alone rerun and fails
// only above ALLOWED_LIMIT_S. A renamed test has no entry, so it returns to LIMIT_S.
const ALLOWED_OVER_LIMIT = [
  {
    binaryId: 'charliebot-cli::bin/charliebot',
    name: 'commands::router::tests::a_timed_out_attempt_is_retried_once_and_the_answer_records_it',
    reason: 'waits on a real request timeout: the delayed reply lands past the 200 ms bound in timeout_limits()',
  },
  {
    binaryId: 'charliebot-cli::bin/charliebot',
    name: 'commands::router::tests::a_timeout_followed_by_a_rate_limit_exits_non_zero_without_a_third_request',
    reason: 'waits on a real request timeout: the delayed reply lands past the 200 ms bound in timeout_limits()',
  },
  {
    binaryId: 'charliebot-cli::bin/charliebot',
    name: 'commands::router::tests::two_timeouts_exit_non_zero_naming_the_timeout_and_the_retry',
    reason: 'waits on two real request timeouts: each delayed reply lands past the 200 ms bound in timeout_limits()',
  },
];

main();

// ---------- judging, runner-agnostic ----------

function main() {
  const options = parseArgs(process.argv.slice(2));
  let testcases;
  try {
    testcases = readTestcases(options.junit, options.runner);
  } catch (error) {
    console.error(`check-unit-test-time: cannot read JUnit report ${options.junit}: ${error.message}`);
    process.exit(2);
  }
  // A test that already failed in the parallel run is judged by the run itself, not by this check.
  const suspects = testcases.filter((test) => !test.skipped && !test.failed && test.time > options.parallelLimit);
  suspects.sort((a, b) => b.time - a.time);

  const lines = [];
  // An allowed test is judged by its JUnit time. Its time is the timeout it waits on, so a rerun alone adds nothing.
  const allowed = suspects.filter((test) => allowanceOf(test) !== undefined);
  const rerunSuspects = suspects.filter((test) => allowanceOf(test) === undefined);
  let overAllowance = 0;
  for (const test of allowed) {
    const { reason } = allowanceOf(test);
    const named = `${test.binaryId} ${test.name} ${formatTime(test.time)} s parallel`;
    if (test.time > ALLOWED_LIMIT_S) {
      overAllowance += 1;
      lines.push(
        `unit test over the ${ALLOWED_LIMIT_S.toFixed(1)} s allowance: ${named}` +
          ` (allowed over ${LIMIT_S} s: ${reason})`
      );
    } else {
      lines.push(
        `unit test within the ${ALLOWED_LIMIT_S.toFixed(1)} s allowance: ${named}` +
          ` (allowed over ${LIMIT_S} s: ${reason})`
      );
    }
  }
  let stillOverAlone = 0;
  let failedAlone = 0;
  let checkFailed = false;
  if (rerunSuspects.length > 0) {
    const rerun = rerunAlone(rerunSuspects, options);
    if (rerun.error !== undefined) {
      checkFailed = true;
      lines.push(`unit test rerun failed: ${rerun.error}`);
    } else {
      for (const suspect of rerunSuspects) {
        const rows = rerun.byId.get(`${suspect.binaryId} ${suspect.name}`);
        const times = rows.map((row) => row.time);
        const shortest = Math.min(...times);
        const aloneTimes = times.map((time) => `${formatTime(time)} s`).join(', ');
        const failed = rows.some((row) => row.failed);
        if (shortest > LIMIT_S) {
          stillOverAlone += 1;
          const suffix = failed ? ', an isolated run FAILED' : '';
          lines.push(
            `unit test over ${LIMIT_S} s: ${suspect.binaryId} ${suspect.name} ${aloneTimes} alone` +
              ` (shortest ${formatTime(shortest)} s${suffix})`
          );
        } else if (failed) {
          failedAlone += 1;
          lines.push(
            `unit test failed alone: ${suspect.binaryId} ${suspect.name} ${aloneTimes} alone` +
              ` (shortest ${formatTime(shortest)} s)`
          );
        } else {
          lines.push(
            `unit test within ${LIMIT_S} s alone: ${suspect.binaryId} ${suspect.name}` +
              ` ${formatTime(suspect.time)} s parallel, ${aloneTimes} alone` +
              ` (shortest ${formatTime(shortest)} s)`
          );
        }
      }
    }
  }
  if (suspects.length === 0) {
    lines.push(
      `unit-test time check: all ${testcases.length} tests ran within ${options.parallelLimit} s in the parallel run`
    );
  } else {
    lines.push(
      `${suspects.length} tests over ${options.parallelLimit} s in the parallel run;` +
        ` ${stillOverAlone} still over alone; ${failedAlone} failed alone;` +
        ` ${allowed.length} allowed, ${overAllowance} over ${ALLOWED_LIMIT_S.toFixed(1)} s`
    );
  }
  report(lines);
  if (checkFailed || stillOverAlone > 0 || failedAlone > 0 || overAllowance > 0) {
    process.exit(1);
  }
}

// The table entry for a test, or undefined when the test is not allowed over LIMIT_S.
function allowanceOf(test) {
  return ALLOWED_OVER_LIMIT.find((entry) => entry.binaryId === test.binaryId && entry.name === test.name);
}

function rerunAlone(suspects, options) {
  const byId = new Map(suspects.map((suspect) => [`${suspect.binaryId} ${suspect.name}`, []]));
  let pending = suspects;
  for (let runNumber = 1; runNumber <= 3 && pending.length > 0; runNumber += 1) {
    const run = options.runner === 'rust' ? isolateWithRust(pending, options) : isolateWithNode(pending, options);
    if (run.error !== undefined) {
      return { byId, error: `alone run ${runNumber} failed: ${run.error}` };
    }
    for (const suspect of pending) {
      const key = `${suspect.binaryId} ${suspect.name}`;
      const row = run.byId.get(key);
      if (row === undefined) {
        return { byId, error: `alone run ${runNumber} matched nothing: ${key}` };
      }
      byId.get(key).push(row);
    }
    if (runNumber === 1) {
      pending = pending.filter((suspect) => byId.get(`${suspect.binaryId} ${suspect.name}`)[0].time > LIMIT_S);
    }
  }
  return { byId, error: undefined };
}

function parseArgs(argv) {
  const options = { runner: null, junit: null, parallelLimit: LIMIT_S, workspaceDir: null };
  const take = (i) => {
    if (i + 1 >= argv.length) usage(`missing value for ${argv[i]}`);
    return argv[i + 1];
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--runner') {
      options.runner = take(i);
    } else if (arg === '--junit') {
      options.junit = take(i);
    } else if (arg === '--parallel-limit') {
      options.parallelLimit = Number(take(i));
      if (!Number.isFinite(options.parallelLimit) || options.parallelLimit < 0) {
        usage('--parallel-limit must be a non-negative number');
      }
    } else if (arg === '--workspace-dir') {
      options.workspaceDir = take(i);
    } else {
      usage(`unknown argument ${arg}`);
      continue;
    }
    i += 1;
  }
  if (options.runner === null) usage('--runner is required');
  if (!Object.hasOwn(RUNNER_DIRECTORY, options.runner)) usage(`--runner takes rust or node, got ${options.runner}`);
  if (options.junit === null) usage('--junit is required');
  options.workspaceDir ??= RUNNER_DIRECTORY[options.runner];
  return options;
}

function usage(message) {
  console.error(
    'usage: check-unit-test-time.mjs --runner rust|node --junit <path> ' +
      '[--parallel-limit <seconds>] [--workspace-dir <dir>]'
  );
  console.error(`check-unit-test-time: ${message}`);
  process.exit(2);
}

function report(lines) {
  for (const line of lines) console.log(line);
  const summary = process.env.GITHUB_STEP_SUMMARY;
  if (summary) {
    appendFileSync(summary, `## Unit-test time check\n\n\`\`\`\n${lines.join('\n')}\n\`\`\`\n`);
  }
}

function formatTime(seconds) {
  return seconds.toFixed(3);
}

// ---------- JUnit report parsing ----------

// nextest puts the binary id in `classname`. Node 24's JUnit report has no suite elements: each testcase
// carries the absolute path of its test file in `file`, and that path is the binary id.
function readTestcases(file, runner) {
  const xml = readFileSync(file, 'utf8');
  const testcases = [];
  const re = /<testcase\b((?:"[^"]*"|'[^']*'|[^>])*)(\/>|>([\s\S]*?)<\/testcase\s*>)/g;
  let match;
  while ((match = re.exec(xml)) !== null) {
    const attrs = attributes(match[1]);
    const body = match[3] ?? '';
    // A node testcase without a file has no file to rerun. Fail here, with the head of the report, rather
    // than rerun it with an empty path.
    if (runner === 'node' && !attrs.file) {
      throw new Error(`node testcase "${attrs.name ?? ''}" has no file attribute; report starts: ${xml.slice(0, 800)}`);
    }
    testcases.push({
      binaryId: runner === 'node' ? attrs.file : (attrs.classname ?? ''),
      name: attrs.name ?? '',
      time: Number.parseFloat(attrs.time ?? 'NaN'),
      skipped: /<skipped[\s/>]/.test(body),
      failed: /<(?:failure|error)[\s>]/.test(body),
    });
  }
  return testcases;
}

function attributes(text) {
  const attrs = {};
  for (const pair of text.matchAll(/([\w:.-]+)="([^"]*)"/g)) {
    attrs[pair[1]] = unescapeXml(pair[2]);
  }
  return attrs;
}

function unescapeXml(text) {
  return text.replace(/&(lt|gt|quot|apos|amp|#x[0-9a-fA-F]+|#\d+);/g, (entity, body) => {
    switch (body) {
      case 'lt': return '<';
      case 'gt': return '>';
      case 'quot': return '"';
      case 'apos': return "'";
      case 'amp': return '&';
      default:
        return String.fromCodePoint(
          body.startsWith('#x') ? Number.parseInt(body.slice(2), 16) : Number.parseInt(body.slice(1), 10)
        );
    }
  });
}

// ---------- isolated runners ----------

// One `cargo nextest run` under profile check runs the selected tests one at a time in separate processes.
// Remove the previous report so a failed run cannot be mistaken for a fresh result.
function isolateWithRust(suspects, options) {
  const filter = suspects
    .map((suspect) => `(test(=${filterLiteral(suspect.name)}) and binary_id(=${filterLiteral(suspect.binaryId)}))`)
    .join(' or ');
  const junit = path.join(options.workspaceDir, 'target/nextest/check/junit.xml');
  rmSync(junit, { force: true });
  const run = spawnSync(
    'cargo',
    ['nextest', 'run', '--locked', '--color', 'never', '--profile', 'check', '-E', filter],
    { cwd: options.workspaceDir, encoding: 'utf8', timeout: RERUN_TIMEOUT_MS, maxBuffer: 64 * 1024 * 1024 }
  );
  const byId = new Map();
  let error;
  // A missing rerun report means nextest never wrote one (spawn failure or a filter that matched
  // no target); every suspect then reports "matched nothing" and `error` surfaces the cause.
  try {
    if (existsSync(junit)) {
      for (const testcase of readTestcases(junit, 'rust')) {
        byId.set(`${testcase.binaryId} ${testcase.name}`, { time: testcase.time, failed: testcase.failed });
      }
    }
  } catch (readError) {
    error = `cannot read rerun JUnit report ${junit}: ${readError.message}`;
  }
  if (run.error || run.signal) {
    error = run.error ? run.error.message : `killed by ${run.signal}`;
  } else if (error === undefined && run.status !== 0 && byId.size === 0) {
    error = `cargo nextest run exited ${run.status}: ${(run.stderr ?? '').trim().split('\n').pop()}`;
  }
  return { byId, error };
}

// A filterset `=` matcher escapes `\`, `,`, `)`, and the control characters.
function filterLiteral(value) {
  const escapes = { '\n': 'n', '\r': 'r', '\t': 't' };
  return value.replace(/[\\),\n\r\t]/g, (char) => `\\${escapes[char] ?? char}`);
}

// Each suspect runs alone in its file. The anchored name pattern selects that test only.
function isolateWithNode(suspects, options) {
  const byId = new Map();
  const scratch = mkdtempSync(path.join(os.tmpdir(), 'unit-test-time-'));
  try {
    for (const [index, suspect] of suspects.entries()) {
      const junit = path.join(scratch, `${index}.xml`);
      const pattern = `^${escapeRegExp(suspect.name)}$`;
      const run = spawnSync(
        'node',
        [
          '--test',
          '--test-reporter=junit',
          `--test-reporter-destination=${junit}`,
          `--test-name-pattern=${pattern}`,
          suspect.binaryId,
        ],
        { cwd: options.workspaceDir, encoding: 'utf8', timeout: RERUN_TIMEOUT_MS, maxBuffer: 64 * 1024 * 1024 }
      );
      if (run.error || run.signal) {
        return { byId, error: run.error ? run.error.message : `killed by ${run.signal}` };
      }
      // A failing test exits nonzero and still writes its report, so only a missing report is an error here.
      if (!existsSync(junit)) {
        return { byId, error: `node --test exited ${run.status} and wrote no report for ${suspect.name}` };
      }
      for (const testcase of readTestcases(junit, 'node')) {
        byId.set(`${testcase.binaryId} ${testcase.name}`, { time: testcase.time, failed: testcase.failed });
      }
    }
  } catch (readError) {
    return { byId, error: `cannot read rerun JUnit report: ${readError.message}` };
  } finally {
    rmSync(scratch, { recursive: true, force: true });
  }
  return { byId, error: undefined };
}

function escapeRegExp(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}
