import { appendFileSync } from 'node:fs';
import { spawn } from 'node:child_process';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import process from 'node:process';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const workspaceRoot = path.join(repositoryRoot, 'rust');
const selection = process.env.RUST_INTEGRATION_FILTER;
if (selection === undefined || selection === '') throw new Error('RUST_INTEGRATION_FILTER is required');

const filter = `(${selection}) and not test(/_subprocess_entry$/)`;
const args = [
  'nextest',
  'run',
  '--locked',
  '--profile',
  'integration',
  '--run-ignored',
  'only',
  '--no-tests=warn',
  '--filterset',
  filter,
];
const child = spawn('cargo', args, { cwd: workspaceRoot, env: process.env });
let stderrTail = '';
child.stdout.pipe(process.stdout);
child.stderr.on('data', (chunk) => {
  process.stderr.write(chunk);
  stderrTail = `${stderrTail}${chunk.toString('utf8')}`.slice(-4096);
});

const result = await new Promise((resolve, reject) => {
  child.once('error', reject);
  child.once('close', (code, signal) => resolve({ code, signal }));
});
if (result.code !== 0) {
  const status = result.code ?? 1;
  if (result.signal !== null) console.error(`cargo nextest was terminated by ${result.signal}`);
  process.exit(status);
}

if (stderrTail.includes('no tests to run')) {
  const line = 'No integration tests matched this non-empty selection; rust-integration passed.';
  console.log(line);
  appendFileSync(process.env.GITHUB_STEP_SUMMARY, `\n${line}\n`);
}
