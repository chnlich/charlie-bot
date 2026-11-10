import { spawnSync } from 'node:child_process';
import { appendFileSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import process from 'node:process';
import TOML from '@iarna/toml';
import {
  parseIntegrationPaths,
  renderSelectionSummary,
  selectAllIntegrationTests,
  selectIntegrationTests,
} from './select.mjs';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');

main();

function main() {
  const eventName = requiredEnvironment('GITHUB_EVENT_NAME');
  const reference = requiredEnvironment('GITHUB_REF');
  const mappingPath = path.join(repositoryRoot, 'dev/integration-paths.toml');
  const mapping = parseIntegrationPaths(readFileSync(mappingPath, 'utf8'));
  mapping.trackedFiles = trackedFiles();
  let selection;

  if ((eventName === 'push' || eventName === 'workflow_dispatch') && reference === 'refs/heads/main') {
    selection = selectAllIntegrationTests(
      eventName === 'push' ? 'push to main' : 'workflow_dispatch on main',
      mapping.trackedFiles,
    );
  } else if (eventName === 'pull_request' && process.env.GITHUB_HEAD_REF?.startsWith('charliebot/task-')) {
    // An implement landing PR runs the full suite: its head sha is what CI gates the base branch on.
    selection = selectAllIntegrationTests('implement landing PR', mapping.trackedFiles);
  } else if (eventName === 'pull_request' || eventName === 'workflow_dispatch') {
    const head = eventName === 'pull_request' ? pullRequestHead() : requiredEnvironment('GITHUB_SHA');
    const changedFiles = changedFilesFromMain(head);
    mapping.cratePackages = workspaceCratePackages(mapping.trackedFiles);
    selection = selectIntegrationTests(changedFiles, mapping);
  } else {
    throw new Error(`unsupported Integration event ${eventName} on ${reference}`);
  }

  const summary = renderSelectionSummary(selection);
  process.stdout.write(summary);
  appendFileSync(requiredEnvironment('GITHUB_STEP_SUMMARY'), summary);
  appendFileSync(
    requiredEnvironment('GITHUB_OUTPUT'),
    `rust_filter=${selection.rustFilter ?? ''}\nnode_files=${nodeFileOutput(selection.nodeFiles)}\n`,
  );
}

function pullRequestHead() {
  const eventPath = requiredEnvironment('GITHUB_EVENT_PATH');
  const event = JSON.parse(readFileSync(eventPath, 'utf8'));
  const head = event.pull_request.head.sha;
  if (typeof head !== 'string' || head === '') {
    throw new Error(`pull_request event ${eventPath} has no pull_request.head.sha`);
  }
  return head;
}

function changedFilesFromMain(head) {
  runCommand('git', ['fetch', '--no-tags', 'origin', 'main'], repositoryRoot);
  const base = runCommand('git', ['merge-base', 'origin/main', head], repositoryRoot).trim();
  const paths = runCommand(
    'git',
    ['diff', '--name-only', '-z', '--no-renames', `${base}..${head}`],
    repositoryRoot,
  );
  return paths.split('\0').filter((file) => file !== '').sort();
}

function trackedFiles() {
  return runCommand('git', ['ls-files', '-z'], repositoryRoot).split('\0').filter((file) => file !== '');
}

// The workspace crates, read from the manifests the checkout tracks under rust/crates. Each crate's package
// name is literal in its manifest, so no cargo run is needed: a node-only job has no Rust toolchain.
function workspaceCratePackages(files) {
  return files
    .filter((file) => /^rust\/crates\/.+\/Cargo\.toml$/.test(file))
    .map((file) => {
      const manifest = TOML.parse(readFileSync(path.join(repositoryRoot, file), 'utf8'));
      if (typeof manifest.package?.name !== 'string') throw new Error(`${file} has no literal [package] name`);
      return { path: path.dirname(file), name: manifest.package.name };
    });
}

// The job reads this list as whitespace-separated words, so a file name with whitespace would split in two.
function nodeFileOutput(files) {
  for (const file of files) {
    if (/\s/.test(file)) throw new Error(`node integration file name contains whitespace: ${JSON.stringify(file)}`);
  }
  return files.join(' ');
}

function runCommand(command, args, cwd) {
  const result = spawnSync(command, args, { cwd, encoding: 'utf8' });
  if (result.error !== undefined) {
    throw new Error(`${command} ${args.join(' ')} could not start: ${result.error.message}`);
  }
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(' ')} exited ${result.status}: ${result.stderr.trim()}`);
  }
  return result.stdout;
}

function requiredEnvironment(name) {
  const value = process.env[name];
  if (value === undefined || value === '') throw new Error(`${name} is required`);
  return value;
}
