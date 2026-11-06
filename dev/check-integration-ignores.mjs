import { spawnSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import process from 'node:process';

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const tracked = spawnSync('git', ['ls-files', '-z', '--', 'rust'], {
  cwd: repositoryRoot,
  encoding: 'utf8',
});
if (tracked.error !== undefined) throw tracked.error;
if (tracked.status !== 0) throw new Error(`git ls-files exited ${tracked.status}: ${tracked.stderr.trim()}`);

const rustFiles = tracked.stdout.split('\0').filter((file) => file.endsWith('.rs'));
const violations = [];
for (const file of rustFiles) {
  const source = readFileSync(path.join(repositoryRoot, file), 'utf8');
  const ignores = /#\[ignore([^\]]*)\]/g;
  let match;
  while ((match = ignores.exec(source)) !== null) {
    const functionPattern =
      /^(?:\s|#\[[^\]]*\])*/.source +
      /(?:(?:pub(?:\([^)]*\))?|async|unsafe|extern(?:\s+"[^"]+")?)\s+)*fn\s+([A-Za-z_][A-Za-z0-9_]*)/.source;
    const functionMatch = source.slice(match.index + match[0].length).match(functionPattern);
    const testName = functionMatch?.[1] ?? '<unnamed ignored item>';
    const integrationReason = /^\s*=\s*(?:"integration"|r(#+)?"integration"\1)\s*$/.test(match[1]);
    if (!integrationReason && !testName.endsWith('_subprocess_entry')) {
      const line = source.slice(0, match.index).split('\n').length;
      violations.push(`${file}:${line}: ${testName} has an unsupported #[ignore] reason`);
    }
  }
}

if (violations.length > 0) {
  for (const violation of violations) console.error(violation);
  process.exit(1);
}
console.log(`checked ${rustFiles.length} Rust files: every ignored test is an integration test or subprocess entry`);
