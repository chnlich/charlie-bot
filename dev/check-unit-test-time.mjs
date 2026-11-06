#!/usr/bin/env node
// The 0.1 s unit-test check, one entry for CI and local runs. It reads a nextest JUnit report,
// reruns each test over the limit alone (exact filterset, one test thread), and judges by the
// isolated time. Parallel-run queuing makes a fast test look slow; the alone rerun has no queue.
// A rerun that matches no test fails the check and names the test.
//
//   dev/check-unit-test-time.mjs --junit <path> [--workspace-dir <dir>] [--fail-over-limit 0|1]
//
//   --junit            the JUnit report of the parallel run (required)
//   --workspace-dir    the cargo workspace directory the reruns run in (default: rust)
//   --fail-over-limit  exit nonzero when a test is still over the limit alone (default: 1)
//
// Lines go to stdout, and to $GITHUB_STEP_SUMMARY when the environment sets it.
import { spawnSync } from 'node:child_process';
import { appendFileSync, readFileSync } from 'node:fs';
import process from 'node:process';

const LIMIT_S = 0.1;
const RERUN_TIMEOUT_MS = 120_000;

main();

// ---------- judging, runner-agnostic ----------

function main() {
  const options = parseArgs(process.argv.slice(2));
  const testcases = readTestcases(options.junit);
  const suspects = testcases.filter((test) => !test.skipped && test.time > LIMIT_S);
  suspects.sort((a, b) => b.time - a.time);

  const lines = [];
  let confirmed = 0;
  let checkFailed = false;
  for (const suspect of suspects) {
    const rerun = isolateWithRust(suspect, options);
    if (rerun === null) {
      checkFailed = true;
      lines.push(`unit test rerun matched nothing: ${suspect.binaryId} ${suspect.name}`);
      continue;
    }
    if (rerun.error !== undefined) {
      checkFailed = true;
      lines.push(`unit test rerun failed: ${suspect.binaryId} ${suspect.name} (${rerun.error})`);
      continue;
    }
    const suffix = rerun.status === 'PASS' ? '' : `, isolated run ${rerun.status}`;
    if (rerun.time > LIMIT_S) {
      confirmed += 1;
      lines.push(`unit test over ${LIMIT_S} s: ${suspect.binaryId} ${suspect.name} ${formatTime(rerun.time)} s (alone${suffix})`);
    } else {
      lines.push(
        `unit test within ${LIMIT_S} s alone: ${suspect.binaryId} ${suspect.name}` +
          ` ${formatTime(suspect.time)} s parallel, ${formatTime(rerun.time)} s alone${suffix}`
      );
    }
  }
  if (suspects.length === 0) {
    lines.push(`unit-test time check: all ${testcases.length} tests ran within ${LIMIT_S} s in the parallel run`);
  } else {
    lines.push(`${suspects.length} tests over ${LIMIT_S} s in the parallel run; ${confirmed} still over alone`);
  }
  report(lines);
  if (checkFailed || (options.failOverLimit && confirmed > 0)) {
    process.exit(1);
  }
}

function parseArgs(argv) {
  const options = { junit: null, workspaceDir: 'rust', failOverLimit: true };
  const take = (i) => {
    if (i + 1 >= argv.length) usage(`missing value for ${argv[i]}`);
    return argv[i + 1];
  };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--junit') {
      options.junit = take(i);
    } else if (arg === '--workspace-dir') {
      options.workspaceDir = take(i);
    } else if (arg === '--fail-over-limit') {
      const value = take(i);
      if (!['0', '1', 'false', 'true'].includes(value)) usage(`--fail-over-limit takes 0 or 1, got ${value}`);
      options.failOverLimit = value === '1' || value === 'true';
    } else {
      usage(`unknown argument ${arg}`);
      continue;
    }
    i += 1;
  }
  if (options.junit === null) usage('--junit is required');
  return options;
}

function usage(message) {
  console.error('usage: check-unit-test-time.mjs --junit <path> [--workspace-dir <dir>] [--fail-over-limit 0|1]');
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

function readTestcases(path) {
  let xml;
  try {
    xml = readFileSync(path, 'utf8');
  } catch (error) {
    console.error(`check-unit-test-time: cannot read JUnit report ${path}: ${error.message}`);
    process.exit(2);
  }
  const testcases = [];
  const re = /<testcase\b([^>]*?)(\/>|>([\s\S]*?)<\/testcase\s*>)/g;
  let match;
  while ((match = re.exec(xml)) !== null) {
    const attrs = {};
    for (const pair of match[1].matchAll(/([\w:.-]+)="([^"]*)"/g)) {
      attrs[pair[1]] = unescapeXml(pair[2]);
    }
    testcases.push({
      binaryId: attrs.classname ?? '',
      name: attrs.name ?? '',
      time: Number.parseFloat(attrs.time ?? 'NaN'),
      skipped: /<skipped[\s/>]/.test(match[3] ?? ''),
    });
  }
  return testcases;
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

// ---------- the rust runner: one test, alone, through nextest ----------

function isolateWithRust(suspect, options) {
  const filter = `test(=${filterLiteral(suspect.name)}) and binary_id(=${filterLiteral(suspect.binaryId)})`;
  const run = spawnSync(
    'cargo',
    ['nextest', 'run', '--locked', '--color', 'never', '--test-threads=1', '-E', filter],
    { cwd: options.workspaceDir, encoding: 'utf8', timeout: RERUN_TIMEOUT_MS, maxBuffer: 64 * 1024 * 1024 }
  );
  const wanted = `${suspect.binaryId} ${suspect.name}`;
  for (const line of `${run.stdout ?? ''}\n${run.stderr ?? ''}`.split('\n')) {
    const match = line.match(/^\s*(PASS|FAIL)\s+\[\s*([0-9.]+)s\]\s+(.*?)\s*$/);
    if (match && match[3] === wanted) {
      return { status: match[1], time: Number.parseFloat(match[2]) };
    }
  }
  if (run.error || run.signal) {
    return { error: run.error ? run.error.message : `killed by ${run.signal}` };
  }
  return null;
}

// A filterset equality matcher escapes `\`, `)`, `,`, `/`, and the control characters.
function filterLiteral(value) {
  const escapes = { '\n': 'n', '\r': 'r', '\t': 't' };
  return value.replace(/[\\),\n\r\t/]/g, (char) => `\\${escapes[char] ?? char}`);
}
