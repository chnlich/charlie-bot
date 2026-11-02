'use strict';
// Preloaded through NODE_OPTIONS=--require by the local work guard. It refuses node's test runner and the npm test and
// build scripts on this host. Every node process loads it, so it checks only the forms the guard refuses.
const path = require('path');
const { spawnSync } = require('child_process');

const REFUSE = path.join(__dirname, '..', 'refuse.sh');
const NPM_TEST_COMMANDS = ['test', 'tst', 't', 'it', 'cit', 'install-test', 'install-ci-test'];
const NPM_RUN_COMMANDS = ['run', 'run-script', 'rum', 'urn'];
const REFUSED_SCRIPT = /^(pre|post)?(test|build)(:|$)/;

function refuse(tool) {
  spawnSync('bash', [REFUSE, tool], { stdio: 'inherit' });
  process.exit(97);
}

if (process.execArgv.some((arg) => arg === '--test' || arg.startsWith('--test='))) {
  refuse('node --test');
}
const lifecycle = process.env.npm_lifecycle_event || '';
if (REFUSED_SCRIPT.test(lifecycle)) {
  refuse(`npm ${lifecycle}`);
}
const entry = process.argv[1] ? path.basename(process.argv[1]) : '';
if (entry === 'npm-cli.js' || entry === 'npm') {
  const words = process.argv.slice(2).filter((arg) => !arg.startsWith('-'));
  const [command, script = ''] = words;
  if (NPM_TEST_COMMANDS.includes(command) || (NPM_RUN_COMMANDS.includes(command) && REFUSED_SCRIPT.test(script))) {
    refuse(`npm ${words.join(' ')}`);
  }
}
