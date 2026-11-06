#!/usr/bin/env node
// The 0.1 s unit-test check, one entry for the Rust and node suites, in CI and local runs. It reads the
// JUnit report of a parallel run and reruns every test over the limit alone, then judges by the isolated
// times. Parallel-run queuing makes a fast test look slow; the alone rerun has no queue. A suspect the
// rerun does not report means its filter matched nothing, which fails the check and names the test.
//
//   dev/check-unit-test-time.mjs --runner rust|node --junit <path> [--workspace-dir <dir>]
//
//   --runner            the suite the report came from: rust (nextest) or node (node --test) (required)
//   --junit             the JUnit report of the parallel run (required)
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
const RERUN_TIMEOUT_MS = 55 * 60_000;
const RUNNER_DIRECTORY = { rust: 'rust', node: 'web' };

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
  const suspects = testcases.filter((test) => !test.skipped && !test.failed && test.time > LIMIT_S);
  suspects.sort((a, b) => b.time - a.time);

  const lines = [];
  let confirmed = 0;
  let checkFailed = false;
  if (suspects.length > 0) {
    const rerun = options.runner === 'rust' ? isolateWithRust(suspects, options) : isolateWithNode(suspects, options);
    if (rerun.error !== undefined) {
      checkFailed = true;
      lines.push(`unit test rerun failed: ${rerun.error}`);
    }
    for (const suspect of suspects) {
      const row = rerun.byId.get(`${suspect.binaryId} ${suspect.name}`);
      if (row === undefined) {
        checkFailed = true;
        lines.push(`unit test rerun matched nothing: ${suspect.binaryId} ${suspect.name}`);
        continue;
      }
      const suffix = row.failed ? ', isolated run FAILED' : '';
      if (row.failed || row.time > LIMIT_S) {
        confirmed += 1;
        lines.push(
          `unit test over ${LIMIT_S} s: ${suspect.binaryId} ${suspect.name} ${formatTime(row.time)} s (alone${suffix})`
        );
      } else {
        lines.push(
          `unit test within ${LIMIT_S} s alone: ${suspect.binaryId} ${suspect.name}` +
            ` ${formatTime(suspect.time)} s parallel, ${formatTime(row.time)} s alone${suffix}`
        );
      }
    }
  }
  if (suspects.length === 0) {
    lines.push(`unit-test time check: all ${testcases.length} tests ran within ${LIMIT_S} s in the parallel run`);
  } else {
    lines.push(`${suspects.length} tests over ${LIMIT_S} s in the parallel run; ${confirmed} still over alone`);
  }
  report(lines);
  if (checkFailed || confirmed > 0) {
    process.exit(1);
  }
}

function parseArgs(argv) {
  const options = { runner: null, junit: null, workspaceDir: null };
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
  console.error('usage: check-unit-test-time.mjs --runner rust|node --junit <path> [--workspace-dir <dir>]');
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

// ---------- the rust runner: one isolated run for every suspect ----------

// One `cargo nextest run` under profile check: the filterset names the suspects exactly, and the
// profile's one test thread runs them one at a time, each in its own process. The profile's own
// JUnit report (target/nextest/check/junit.xml) carries every rerun's time; a suspect absent from
// it matched nothing.
function isolateWithRust(suspects, options) {
  const filter = suspects
    .map((suspect) => `(test(=${filterLiteral(suspect.name)}) and binary_id(=${filterLiteral(suspect.binaryId)}))`)
    .join(' or ');
  const junit = path.join(options.workspaceDir, 'target/nextest/check/junit.xml');
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

// ---------- the node runner: one `node --test` per suspect, on its own file ----------

// Each suspect runs alone in its file, so the file's other tests do not queue behind it. The name pattern
// is the test name escaped for a regular expression and anchored at both ends, so it matches that test only.
// A suspect whose run writes no testcase for it matched nothing.
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
