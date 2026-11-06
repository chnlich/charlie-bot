import { spawnSync } from 'node:child_process';
import { appendFileSync, readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import process from 'node:process';
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
  let selection;

  if ((eventName === 'push' || eventName === 'workflow_dispatch') && reference === 'refs/heads/main') {
    selection = selectAllIntegrationTests(eventName === 'push' ? 'push to main' : 'workflow_dispatch on main');
  } else if (eventName === 'pull_request' || eventName === 'workflow_dispatch') {
    const head = eventName === 'pull_request' ? pullRequestHead() : requiredEnvironment('GITHUB_SHA');
    const changedFiles = changedFilesFromMain(head);
    mapping.cratePackages = workspaceCratePackages();
    selection = selectIntegrationTests(changedFiles, mapping);
  } else {
    throw new Error(`unsupported Integration event ${eventName} on ${reference}`);
  }

  const summary = renderSelectionSummary(selection);
  process.stdout.write(summary);
  appendFileSync(requiredEnvironment('GITHUB_STEP_SUMMARY'), summary);
  appendFileSync(requiredEnvironment('GITHUB_OUTPUT'), `rust_filter=${selection.rustFilter ?? ''}\n`);
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

function workspaceCratePackages() {
  const output = runCommand(
    'cargo',
    ['metadata', '--manifest-path', 'rust/Cargo.toml', '--format-version', '1', '--no-deps', '--locked'],
    repositoryRoot,
  );
  const metadata = JSON.parse(output);
  return metadata.packages.flatMap((packageMetadata) => {
    const directory = path.relative(repositoryRoot, path.dirname(packageMetadata.manifest_path));
    const relativeDirectory = directory.split(path.sep).join('/');
    return relativeDirectory.startsWith('rust/crates/')
      ? [{ path: relativeDirectory, name: packageMetadata.name }]
      : [];
  });
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
