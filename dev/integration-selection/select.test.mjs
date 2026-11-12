import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
import {
  parseIntegrationPaths,
  renderSelectionSummary,
  selectIntegrationTests,
} from './select.mjs';

const COMMITTED_MAPPING = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '../integration-paths.toml',
);

function mapping(source, cratePackages = [], trackedFiles = []) {
  return { ...parseIntegrationPaths(source), cratePackages, trackedFiles };
}

test('workspace files select all before crate and mapped-file rules', () => {
  const parsed = mapping('[[entry]]\npaths = ["rust/**"]\nnone = true\n', [
    { path: 'rust/crates/features/artifacts', name: 'charliebot-artifacts' },
  ]);
  const selection = selectIntegrationTests(['rust/Cargo.toml', 'rust/crates/features/artifacts/src/lib.rs'], parsed);

  assert.equal(selection.rustFilter, 'all()');
  assert.deepEqual(selection.allFiles, [{ file: 'rust/Cargo.toml', reason: 'workspace-level file' }]);
  assert.equal(selection.decisions[1].kind, 'crate');
});

test('crate files select reverse dependencies of the Cargo package', () => {
  const parsed = mapping('[[entry]]\npaths = ["rust/crates/features/**"]\nnone = true\n', [
    { path: 'rust/crates/features/artifacts', name: 'charliebot-artifacts' },
  ]);
  const selection = selectIntegrationTests(['rust/crates/features/artifacts/src/artifact_check.rs'], parsed);

  assert.equal(selection.rustFilter, 'rdeps(=charliebot-artifacts)');
  assert.equal(selection.decisions[0].kind, 'crate');
});

test('a crate file also selects the mapping entries that match it', () => {
  const parsed = mapping(
    '[[entry]]\npaths = ["rust/crates/**/*.rs"]\nrust = "binary_id(charliebot-structure::structure)"\n',
    [{ path: 'rust/crates/features/artifacts', name: 'charliebot-artifacts' }],
  );
  const selection = selectIntegrationTests(['rust/crates/features/artifacts/src/artifact_check.rs'], parsed);

  assert.equal(
    selection.rustFilter,
    'rdeps(=charliebot-artifacts) + binary_id(charliebot-structure::structure)',
  );
  assert.equal(selection.decisions[0].kind, 'crate');
});

test('a mapped file selects its Rust filter and the tracked node integration files its glob matches', () => {
  const parsed = mapping(
    [
      '[[entry]]',
      'paths = ["scripts/switch-server.sh"]',
      'rust = "binary_id(charliebot-script-tests::switch_server)"',
      'node = ["web/tests/*.integration.test.mjs"]',
      '',
    ].join('\n'),
    [],
    ['web/tests/a.integration.test.mjs', 'web/tests/a.test.mjs', 'web/tests/b.integration.test.js'],
  );
  const selection = selectIntegrationTests(['scripts/switch-server.sh'], parsed);

  assert.equal(selection.rustFilter, 'binary_id(charliebot-script-tests::switch_server)');
  assert.deepEqual(selection.nodeFiles, ['web/tests/a.integration.test.mjs']);
  assert.equal(selection.decisions[0].kind, 'mapped');
});

test('an unmapped file selects all and every node integration file, and appears in the summary', () => {
  const selection = selectIntegrationTests(
    ['LICENSE', 'NOTICE'],
    mapping('[[entry]]\npaths = ["docs/**"]\nnone = true\n', [], ['web/tests/a.integration.test.js', 'web/tests/a.test.js']),
  );
  const summary = renderSelectionSummary(selection);

  assert.equal(selection.rustFilter, 'all()');
  assert.deepEqual(selection.allFiles.map(({ file }) => file), ['LICENSE', 'NOTICE']);
  assert.deepEqual(selection.nodeFiles, ['web/tests/a.integration.test.js']);
  assert.match(summary, /`LICENSE` \(no mapping entry matched\)/);
  assert.match(summary, /`NOTICE` \(no mapping entry matched\)/);
  assert.match(summary, /Node selection \(1\): `web\/tests\/a\.integration\.test\.js`/);
});

test('none entries produce an empty selection and say that no integration tests are touched', () => {
  const selection = selectIntegrationTests(
    ['docs/README.md'],
    mapping('[[entry]]\npaths = ["docs/**"]\nnone = true\n', [], ['web/tests/a.integration.test.js']),
  );
  const summary = renderSelectionSummary(selection);

  assert.equal(selection.rustFilter, null);
  assert.equal(selection.all, false);
  assert.deepEqual(selection.nodeFiles, []);
  assert.match(summary, /`docs\/README.md`/);
  assert.match(summary, /This change touches no integration tests\./);
});

test('unknown mapping fields fail with their entry number', () => {
  assert.throws(
    () => mapping('[[entry]]\npaths = ["docs/**"]\nnone = true\nowner = "nobody"\n'),
    /entry 1 .*unknown field "owner"/,
  );
});

test('invalid nextest filters fail with their mapping entry', () => {
  assert.throws(
    () => mapping('[[entry]]\npaths = ["scripts/**"]\nrust = "rdeps("\n'),
    /entry 1 .*invalid rust filter/,
  );
});

// The committed mapping must select the node integration test that reads each changed file. The cases read
// dev/integration-paths.toml itself, so a mapping entry a new reader needs cannot go missing silently.

test('the committed mapping selects the structure test when the package registry changes', () => {
  const parsed = mapping(
    readFileSync(COMMITTED_MAPPING, 'utf8'),
    [{ path: 'rust/crates/app/packages', name: 'charliebot-packages' }],
    ['web/tests/frontend_structure.integration.test.js'],
  );
  const selection = selectIntegrationTests(['rust/crates/app/packages/src/lib.rs'], parsed);

  assert.equal(selection.all, false);
  assert.deepEqual(selection.nodeFiles, ['web/tests/frontend_structure.integration.test.js']);
});

test('the committed mapping selects the client-readback test when the readback script changes', () => {
  const parsed = mapping(
    readFileSync(COMMITTED_MAPPING, 'utf8'),
    [],
    ['web/tests/client_readback.integration.test.mjs'],
  );
  const selection = selectIntegrationTests(['dev/client-readback/readback.mjs'], parsed);

  assert.equal(selection.all, false);
  assert.deepEqual(selection.nodeFiles, ['web/tests/client_readback.integration.test.mjs']);
});

test('the committed mapping selects the plan-panel test when the plan template changes', () => {
  const parsed = mapping(
    readFileSync(COMMITTED_MAPPING, 'utf8'),
    [],
    ['web/tests/plan_panel.integration.test.js'],
  );
  const selection = selectIntegrationTests(['prompts/plan_template.html'], parsed);

  assert.equal(selection.all, false);
  assert.deepEqual(selection.nodeFiles, ['web/tests/plan_panel.integration.test.js']);
});

test('the committed mapping selects the switch-helper test when backends or switch-helper change', () => {
  const parsed = mapping(
    readFileSync(COMMITTED_MAPPING, 'utf8'),
    [
      { path: 'rust/crates/backends/anthropic_compatible', name: 'charliebot-anthropic-compatible' },
      { path: 'rust/crates/app/switch-helper', name: 'charliebot-switch-helper' },
    ],
    [],
  );
  const backendSelection = selectIntegrationTests(
    ['rust/crates/backends/anthropic_compatible/src/options.rs'],
    parsed,
  );
  assert.equal(backendSelection.all, false);
  assert.ok(backendSelection.rustFilter.includes('package(charliebot-switch-helper)'));

  const helperSelection = selectIntegrationTests(
    ['rust/crates/app/switch-helper/src/scan.rs'],
    parsed,
  );
  assert.equal(helperSelection.all, false);
  assert.ok(helperSelection.rustFilter.includes('package(charliebot-switch-helper)'));
});
