import TOML from '@iarna/toml';
import { Minimatch } from 'minimatch';

const ENTRY_FIELDS = new Set(['paths', 'rust', 'node', 'none']);
const NEXTTEST_PREDICATES = new Set([
  'all',
  'none',
  'test',
  'group',
  'package',
  'deps',
  'rdeps',
  'binary_id',
  'kind',
  'binary',
  'platform',
  'default',
]);
const BINARY_KINDS = new Set(['lib', 'test', 'bench', 'proc-macro', 'bin', 'example']);
const WORKSPACE_FILES = new Set([
  'Cargo.lock',
  'rust/Cargo.toml',
  'rust/Cargo.lock',
  'rust/.config/nextest.toml',
  'dev/integration-paths.toml',
]);
const SELECTOR_DIRECTORY = 'dev/integration-selection/';
// A node integration test file is named by its suffix. Every tracked file with that suffix is one, and a node glob in
// the mapping may select only those files.
const NODE_INTEGRATION_FILE = /\.integration\.test\.m?js$/;

export function parseIntegrationPaths(source) {
  let document;
  try {
    document = TOML.parse(source);
  } catch (error) {
    throw new Error(`dev/integration-paths.toml: ${error.message}`);
  }

  for (const field of Object.keys(document)) {
    if (field !== 'entry') {
      throw new Error(`dev/integration-paths.toml: unknown field "${field}" outside an entry`);
    }
  }
  if (!Array.isArray(document.entry) || document.entry.length === 0) {
    throw new Error('dev/integration-paths.toml: expected at least one [[entry]]');
  }

  return { entries: document.entry.map(parseEntry) };
}

function parseEntry(entry, index) {
  const number = index + 1;
  if (entry === null || typeof entry !== 'object' || Array.isArray(entry)) {
    throw new Error(`dev/integration-paths.toml entry ${number}: expected a table`);
  }

  const paths = entry.paths;
  const label = entryLabel(number, paths);
  for (const field of Object.keys(entry)) {
    if (!ENTRY_FIELDS.has(field)) {
      throw new Error(`${label}: unknown field "${field}"`);
    }
  }
  if (!Array.isArray(paths) || paths.length === 0 || paths.some((value) => typeof value !== 'string' || value === '')) {
    throw new Error(`${label}: paths must be a non-empty array of strings`);
  }
  for (const pattern of paths) validatePathPattern(pattern, label);

  const hasRust = Object.hasOwn(entry, 'rust');
  const hasNode = Object.hasOwn(entry, 'node');
  const hasNone = Object.hasOwn(entry, 'none');
  if (hasNone) {
    if (entry.none !== true || hasRust || hasNode) {
      throw new Error(`${label}: none must be true and cannot be combined with rust or node`);
    }
  } else if (!hasRust && !hasNode) {
    throw new Error(`${label}: expected rust, node, or none = true`);
  }

  let rust;
  if (hasRust) {
    if (typeof entry.rust !== 'string' || entry.rust.trim() === '') {
      throw new Error(`${label}: rust must be a non-empty nextest filter`);
    }
    try {
      validateNextestFilterset(entry.rust);
    } catch (error) {
      throw new Error(`${label}: invalid rust filter ${JSON.stringify(entry.rust)}: ${error.message}`);
    }
    rust = entry.rust;
  }

  let node;
  if (hasNode) {
    const invalidNodePatterns = !Array.isArray(entry.node) || entry.node.length === 0 ||
      entry.node.some((value) => typeof value !== 'string' || value === '');
    if (invalidNodePatterns) {
      throw new Error(`${label}: node must be a non-empty array of path globs`);
    }
    for (const pattern of entry.node) validatePathPattern(pattern, label);
    node = [...entry.node];
  }

  return {
    number,
    paths: [...paths],
    rust,
    node,
    none: hasNone,
    label,
    matchers: paths.map((pattern) => new Minimatch(pattern, { dot: true, nocomment: true, nonegate: true })),
  };
}

function entryLabel(number, paths) {
  const displayedPaths = Array.isArray(paths) ? paths.join(', ') : 'missing paths';
  return `dev/integration-paths.toml entry ${number} (paths: ${displayedPaths})`;
}

function validatePathPattern(pattern, label) {
  const hasInvalidPath = pattern.startsWith('/') || pattern.split('/').includes('..') ||
    pattern.includes('\\') || /[\r\n]/.test(pattern);
  if (hasInvalidPath) {
    throw new Error(`${label}: path glob must be a relative repository path: ${JSON.stringify(pattern)}`);
  }
  let matcher;
  try {
    matcher = new Minimatch(pattern, { dot: true, nocomment: true, nonegate: true });
  } catch (error) {
    throw new Error(`${label}: invalid path glob ${JSON.stringify(pattern)}: ${error.message}`);
  }
  if (matcher.makeRe() === false) {
    throw new Error(`${label}: invalid path glob ${JSON.stringify(pattern)}`);
  }
}

export function validateNextestFilterset(filter) {
  if (/\r|\n/.test(filter)) throw new Error('filter must fit on one line');
  const parser = new FiltersetParser(filter);
  parser.parse();
}

class FiltersetParser {
  constructor(source) {
    this.source = source;
    this.offset = 0;
  }

  parse() {
    this.parseUnion();
    this.skipWhitespace();
    if (!this.atEnd()) this.fail('unexpected trailing input');
  }

  parseUnion() {
    this.parseIntersection();
    while (this.consumeWord('or') || this.consumeSymbol('|') || this.consumeSymbol('+')) this.parseIntersection();
  }

  parseIntersection() {
    this.parseUnary();
    while (this.consumeWord('and') || this.consumeSymbol('&') || this.consumeSymbol('-')) this.parseUnary();
  }

  parseUnary() {
    if (this.consumeWord('not') || this.consumeSymbol('!')) {
      this.parseUnary();
      return;
    }
    this.parsePrimary();
  }

  parsePrimary() {
    this.skipWhitespace();
    if (this.consumeSymbol('(')) {
      this.parseUnion();
      this.expectSymbol(')');
      return;
    }

    const predicate = this.readIdentifier();
    if (!NEXTTEST_PREDICATES.has(predicate)) this.fail(`unknown predicate "${predicate}"`);
    this.expectSymbol('(');
    if (['all', 'none', 'default'].includes(predicate)) {
      this.expectSymbol(')');
      return;
    }

    const matcher = this.readMatcher();
    if (matcher === '') this.fail(`${predicate} requires a name matcher`);
    if (predicate === 'platform' && !['host', 'target'].includes(matcher)) {
      this.fail(`platform matcher must be host or target, got "${matcher}"`);
    }
    if (predicate === 'kind' && !BINARY_KINDS.has(matcher)) {
      this.fail(`unknown binary kind "${matcher}"`);
    }
    this.expectSymbol(')');
  }

  readMatcher() {
    this.skipWhitespace();
    if (this.source[this.offset] === '/') return this.readRegexMatcher();

    const start = this.offset;
    let escaped = false;
    while (!this.atEnd()) {
      const character = this.source[this.offset];
      if (!escaped && character === ')') break;
      if (!escaped && character === '(') this.fail('unescaped parenthesis in name matcher');
      if (!escaped && character === '\\') {
        escaped = true;
        this.offset += 1;
        continue;
      }
      if (escaped) {
        this.validateEscape(this.offset - 1);
        escaped = false;
      }
      this.offset += 1;
    }
    if (escaped) this.fail('trailing backslash in name matcher');
    return this.source.slice(start, this.offset).trim();
  }

  readRegexMatcher() {
    this.offset += 1;
    const start = this.offset;
    let escaped = false;
    while (!this.atEnd()) {
      const character = this.source[this.offset];
      if (!escaped && character === '/') {
        const pattern = this.source.slice(start, this.offset);
        this.offset += 1;
        try {
          new RegExp(pattern);
        } catch (error) {
          this.fail(`invalid regular expression: ${error.message}`);
        }
        return `/${pattern}/`;
      }
      if (!escaped && character === '\\') {
        escaped = true;
        this.offset += 1;
        continue;
      }
      escaped = false;
      this.offset += 1;
    }
    this.fail('unterminated regular expression matcher');
  }

  validateEscape(backslashOffset) {
    const escaped = this.source[this.offset];
    if (['n', 'r', 't', '\\', '/', ')', ','].includes(escaped)) return;
    if (escaped === 'u' && this.source[this.offset + 1] === '{') {
      const end = this.source.indexOf('}', this.offset + 2);
      const digits = this.source.slice(this.offset + 2, end);
      if (end > this.offset + 2 && /^[0-9a-fA-F]{1,6}$/.test(digits)) {
        this.offset = end;
        return;
      }
    }
    this.offset = backslashOffset;
    this.fail(`invalid escape sequence \\${escaped}`);
  }

  readIdentifier() {
    this.skipWhitespace();
    const start = this.offset;
    while (/[A-Za-z0-9_]/.test(this.source[this.offset] ?? '')) this.offset += 1;
    if (start === this.offset) this.fail('expected a predicate');
    return this.source.slice(start, this.offset);
  }

  consumeWord(word) {
    this.skipWhitespace();
    if (!this.source.startsWith(word, this.offset)) return false;
    const next = this.source[this.offset + word.length];
    if (next !== undefined && /[A-Za-z0-9_]/.test(next)) return false;
    this.offset += word.length;
    return true;
  }

  consumeSymbol(symbol) {
    this.skipWhitespace();
    if (this.source[this.offset] !== symbol) return false;
    this.offset += 1;
    return true;
  }

  expectSymbol(symbol) {
    if (!this.consumeSymbol(symbol)) this.fail(`expected "${symbol}"`);
  }

  skipWhitespace() {
    while (/\s/.test(this.source[this.offset] ?? '')) this.offset += 1;
  }

  atEnd() {
    return this.offset >= this.source.length;
  }

  fail(message) {
    throw new Error(`${message} at character ${this.offset + 1}`);
  }
}

export function selectIntegrationTests(changedFiles, mapping) {
  const decisions = [];
  const allFiles = [];
  const rustFilters = [];
  const nodePatterns = [];
  const noneFiles = [];

  for (const file of changedFiles) {
    const workspaceReason = workspaceFileReason(file);
    if (workspaceReason !== null) {
      allFiles.push({ file, reason: workspaceReason });
      decisions.push({ file, kind: 'all', reason: workspaceReason });
      continue;
    }

    const entries = mapping.entries.filter((entry) => entry.matchers.some((matcher) => matcher.match(file)));

    // A file under a crate selects its reverse dependencies and every mapping entry that also covers it: tests
    // that read sources across crates (the structure checks) reach their binary through the mapping alone.
    const crate = mapping.cratePackages
      .filter((candidate) => file.startsWith(`${candidate.path}/`))
      .sort((left, right) => right.path.length - left.path.length)[0];
    if (crate !== undefined) {
      const filter = `rdeps(=${crate.name})`;
      rustFilters.push(filter);
      for (const entry of entries) {
        if (entry.none) noneFiles.push({ file, entry: entry.label });
        if (entry.rust !== undefined) rustFilters.push(entry.rust);
        if (entry.node !== undefined) nodePatterns.push(...entry.node);
      }
      decisions.push({ file, kind: 'crate', filter, entries: entries.map((entry) => entry.label) });
      continue;
    }

    if (entries.length > 0) {
      for (const entry of entries) {
        if (entry.none) noneFiles.push({ file, entry: entry.label });
        if (entry.rust !== undefined) rustFilters.push(entry.rust);
        if (entry.node !== undefined) nodePatterns.push(...entry.node);
      }
      decisions.push({ file, kind: 'mapped', entries: entries.map((entry) => entry.label) });
      continue;
    }

    allFiles.push({ file, reason: 'no mapping entry matched' });
    decisions.push({ file, kind: 'all', reason: 'no mapping entry matched' });
  }

  const uniqueRustFilters = [...new Set(rustFilters)];
  const uniqueNodePatterns = [...new Set(nodePatterns)];
  const selectsAll = allFiles.length > 0;
  return {
    all: selectsAll,
    allFiles,
    decisions,
    rustFilters: selectsAll ? ['all()'] : uniqueRustFilters,
    rustFilter: selectsAll ? 'all()' : uniqueRustFilters.length === 0 ? null : uniqueRustFilters.join(' + '),
    nodeFiles: selectsAll
      ? nodeIntegrationFiles(mapping.trackedFiles)
      : resolveNodePatterns(uniqueNodePatterns, mapping.trackedFiles),
    noneFiles,
  };
}

export function selectAllIntegrationTests(reason, trackedFiles) {
  return {
    all: true,
    allReason: reason,
    allFiles: [],
    decisions: [],
    rustFilters: ['all()'],
    rustFilter: 'all()',
    nodeFiles: nodeIntegrationFiles(trackedFiles),
    noneFiles: [],
  };
}

// The node integration files among the tracked files, in path order: what a whole-suite run selects.
export function nodeIntegrationFiles(trackedFiles) {
  return trackedFiles.filter((file) => NODE_INTEGRATION_FILE.test(file)).sort();
}

// The integration files that the mapping's node globs select. A glob that selects a file which is not an
// integration test fails the selection: that file would run in the default suite or be skipped by its name.
function resolveNodePatterns(patterns, trackedFiles) {
  const selected = new Set();
  for (const pattern of patterns) {
    const matcher = new Minimatch(pattern, { dot: true, nocomment: true, nonegate: true });
    for (const file of trackedFiles) {
      if (!matcher.match(file)) continue;
      if (!NODE_INTEGRATION_FILE.test(file)) {
        throw new Error(`node pattern ${JSON.stringify(pattern)} selects ${file}, which is not an integration test file`);
      }
      selected.add(file);
    }
  }
  return [...selected].sort();
}

export function workspaceFileReason(file) {
  if (WORKSPACE_FILES.has(file) || file.startsWith('.github/workflows/') || file.startsWith(SELECTOR_DIRECTORY)) {
    return 'workspace-level file';
  }
  return null;
}

export function renderSelectionSummary(selection) {
  const lines = ['## Integration selection', ''];
  if (selection.allReason !== undefined) {
    lines.push(`- Rust selection: all Rust integration tests (${selection.allReason}).`);
  } else if (selection.rustFilter === null) {
    lines.push('- Rust selection: empty; rust-integration passes without compiling.');
  } else {
    lines.push(`- Rust selection: \`${selection.rustFilter}\`.`);
  }
  if (selection.nodeFiles.length > 0) {
    lines.push(`- Node selection (${selection.nodeFiles.length}): ${selection.nodeFiles.map((file) => `\`${file}\``).join(', ')}.`);
  } else {
    lines.push('- Node selection: empty; node-integration passes without running node.');
  }
  if (selection.rustFilter === null && selection.nodeFiles.length === 0) {
    lines.push('- This change touches no integration tests.');
  }

  if (selection.allFiles.length > 0) {
    lines.push('', '- Files that selected all:');
    for (const { file, reason } of selection.allFiles) lines.push(`  - \`${file}\` (${reason}).`);
  }
  if (selection.allReason === undefined && selection.noneFiles.length > 0) {
    lines.push('', '- Files declared to have no integration tests:');
    for (const { file, entry } of selection.noneFiles) lines.push(`  - \`${file}\` (${entry}).`);
  }
  if (selection.allReason === undefined && selection.decisions.length > 0) {
    lines.push('', '- Changed-file classification:');
    for (const decision of selection.decisions) {
      if (decision.kind === 'all') continue;
      const detail = [decision.filter, ...(decision.entries ?? [])].filter(Boolean).join('; ');
      lines.push(`  - \`${decision.file}\`: ${detail}.`);
    }
  }
  return `${lines.join('\n')}\n`;
}

export function isWorkspaceFile(file) {
  return workspaceFileReason(file) !== null;
}
