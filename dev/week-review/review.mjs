// The week-one review entry point: label a time window of parent-to-child messages, tally the labels, and re-judge a
// sample with a model from another vendor. Usage and the recovered method are in README.md beside this file.
//
//   node dev/week-review/review.mjs label --from <iso> --to <iso> --reports-until <iso> --out <dir> [--home <dir>]
//                                         [--model claude-opus-5-5] [--parallel 6]
//   node dev/week-review/review.mjs tally --out <dir>
//   node dev/week-review/review.mjs rejudge --out <dir> --model <gateway model> --gateway <base URL>
//                                           [--sample 20] [--seed 1] [--credentials <file>]
//                                           [--credential <entry name>] [--parallel 4]

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import { labelWindow, writeTally } from './lib/label.mjs';
import { rejudgeSample } from './lib/rejudge.mjs';
import { formatTally } from './lib/tally.mjs';

const HERE = path.dirname(fileURLToPath(import.meta.url));

// The flags each command takes, with the value that stands when the flag is absent (`null` means required).
const COMMANDS = {
  label: {
    flags: {
      '--from': null,
      '--to': null,
      '--reports-until': null,
      '--out': null,
      '--home': process.env.CHARLIEBOT_HOME || path.join(os.homedir(), '.charliebot'),
      '--model': 'claude-opus-5-5',
      '--parallel': '6',
    },
    run: runLabel,
  },
  tally: {
    flags: { '--out': null },
    run: runTally,
  },
  rejudge: {
    flags: {
      '--out': null,
      '--model': null,
      '--sample': '20',
      '--seed': '1',
      '--gateway': null,
      '--credentials': path.join(os.homedir(), '.charliebot', 'credentials.yaml'),
      '--credential': 'aigw_charliebot_agents',
      '--parallel': '4',
    },
    run: runRejudge,
  },
};

function parseFlags(command, argv) {
  const spec = COMMANDS[command].flags;
  const values = {};
  for (let index = 0; index < argv.length; index += 2) {
    const flag = argv[index];
    if (!(flag in spec)) throw new Error(`${command}: unknown flag ${flag}`);
    if (index + 1 >= argv.length) throw new Error(`${command}: ${flag} needs a value`);
    values[flag] = argv[index + 1];
  }
  for (const [flag, fallback] of Object.entries(spec)) {
    if (flag in values) continue;
    if (fallback === null) throw new Error(`${command}: ${flag} is required`);
    values[flag] = fallback;
  }
  return values;
}

function integerFlag(name, text, minimum) {
  const value = Number(text);
  if (!Number.isInteger(value) || value < minimum) {
    throw new Error(`${name} must be a whole number of at least ${minimum}, got ${JSON.stringify(text)}`);
  }
  return value;
}

function loadPrompts() {
  return {
    manager: fs.readFileSync(path.join(HERE, 'judge_prompt.md'), 'utf8'),
    worker: fs.readFileSync(path.join(HERE, 'judge_prompt_worker.md'), 'utf8'),
  };
}

async function runLabel(argv) {
  const flags = parseFlags('label', argv);
  const { run, records } = await labelWindow({
    home: flags['--home'],
    from: flags['--from'],
    to: flags['--to'],
    reportsUntil: flags['--reports-until'],
    out: flags['--out'],
    model: flags['--model'],
    parallel: integerFlag('--parallel', flags['--parallel'], 1),
    prompts: loadPrompts(),
  });
  process.stdout.write(writeTally(flags['--out'], run, records));
}

function runTally(argv) {
  const flags = parseFlags('tally', argv);
  const out = flags['--out'];
  const run = JSON.parse(fs.readFileSync(path.join(out, 'run.json'), 'utf8'));
  const records = fs
    .readFileSync(path.join(out, 'messages.jsonl'), 'utf8')
    .split('\n')
    .filter((line) => line !== '')
    .map((line) => JSON.parse(line));
  const text = formatTally({ run, records });
  fs.writeFileSync(path.join(out, 'tally.txt'), text);
  process.stdout.write(text);
}

async function runRejudge(argv) {
  const flags = parseFlags('rejudge', argv);
  const text = await rejudgeSample({
    out: flags['--out'],
    model: flags['--model'],
    sample: integerFlag('--sample', flags['--sample'], 1),
    seed: integerFlag('--seed', flags['--seed'], 0),
    gateway: flags['--gateway'],
    credentialsPath: flags['--credentials'],
    credentialName: flags['--credential'],
    prompts: loadPrompts(),
    parallel: integerFlag('--parallel', flags['--parallel'], 1),
  });
  process.stdout.write(text);
}

async function main(argv) {
  const [command, ...rest] = argv;
  if (!Object.hasOwn(COMMANDS, command)) {
    process.stderr.write(`usage: review.mjs <${Object.keys(COMMANDS).join('|')}> [flags]\n`);
    process.exitCode = 2;
    return;
  }
  try {
    await COMMANDS[command].run(rest);
  } catch (error) {
    process.stderr.write(`review: ${error.message}\n`);
    process.exitCode = 1;
  }
}

await main(process.argv.slice(2));
