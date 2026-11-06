import assert from 'node:assert/strict';
import test from 'node:test';
import {
  parseIntegrationPaths,
  renderSelectionSummary,
  selectIntegrationTests,
} from './select.mjs';

function mapping(source, cratePackages = []) {
  return { ...parseIntegrationPaths(source), cratePackages };
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

test('mapped files select their Rust and Node entries', () => {
  const parsed = mapping(
    [
      '[[entry]]',
      'paths = ["scripts/switch-server.sh"]',
      'rust = "binary_id(charliebot-script-tests::switch_server)"',
      'node = ["web/tests/*.integration.test.mjs"]',
      '',
    ].join('\n'),
  );
  const selection = selectIntegrationTests(['scripts/switch-server.sh'], parsed);

  assert.equal(selection.rustFilter, 'binary_id(charliebot-script-tests::switch_server)');
  assert.deepEqual(selection.nodePatterns, ['web/tests/*.integration.test.mjs']);
  assert.equal(selection.decisions[0].kind, 'mapped');
});

test('an unmapped file selects all and appears in the summary', () => {
  const selection = selectIntegrationTests(
    ['LICENSE', 'NOTICE'],
    mapping('[[entry]]\npaths = ["docs/**"]\nnone = true\n'),
  );
  const summary = renderSelectionSummary(selection);

  assert.equal(selection.rustFilter, 'all()');
  assert.deepEqual(selection.allFiles.map(({ file }) => file), ['LICENSE', 'NOTICE']);
  assert.match(summary, /`LICENSE` \(no mapping entry matched\)/);
  assert.match(summary, /`NOTICE` \(no mapping entry matched\)/);
});

test('none entries produce an empty selection and explain that no integration tests are touched', () => {
  const selection = selectIntegrationTests(
    ['docs/README.md'],
    mapping('[[entry]]\npaths = ["docs/**"]\nnone = true\n'),
  );
  const summary = renderSelectionSummary(selection);

  assert.equal(selection.rustFilter, null);
  assert.equal(selection.all, false);
  assert.match(summary, /`docs\/README.md`/);
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
