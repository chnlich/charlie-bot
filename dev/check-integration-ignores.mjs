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
const crateNames = new Map();
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

  for (const violation of unmarkedLaunchViolations(file, source)) {
    violations.push(violation);
  }
}

if (violations.length > 0) {
  for (const violation of violations) console.error(violation);
  process.exit(1);
}
console.log(
  `checked ${rustFiles.length} Rust files: every ignored test is an integration test or subprocess` +
    ' entry, and no unmarked test launches a process'
);

// ---------------------------------------------------------------------------
// The unmarked-process-launch scan.

// A test whose own body launches a process is an integration test under the repo's classification
// rule, and one without #[ignore] stays in the default suite until someone notices. This scan fails
// on each unmarked test whose body directly calls Command::new or current_exe. A launch routed
// through a helper escapes it; so does network use, which the rule allows inside one process.
function unmarkedLaunchViolations(file, source) {
  const text = stripRustSource(source);
  const violations = [];
  const functionPattern = /\bfn\s+([A-Za-z_][A-Za-z0-9_]*)/g;
  let match;
  while ((match = functionPattern.exec(text)) !== null) {
    const attributes = attributesBefore(text, match.index);
    if (!attributes.some((attribute) => /#\[(?:test|tokio::test)\s*[(\]]/.test(attribute))) continue;
    if (attributes.some((attribute) => /#\[ignore\b/.test(attribute))) continue;
    const body = functionBody(text, match.index + match[0].length);
    if (body === null) continue;
    const call = launchCall(body, text);
    if (call === null) continue;
    const line = source.slice(0, match.index).split('\n').length;
    violations.push(
      `${file}:${line}: unmarked integration test: ${binaryId(file)} ${match[1]} calls ${call};` +
        ' add #[ignore = "integration"]'
    );
  }
  return violations;
}

// The direct process-launch call a test body makes, or null. `current_exe` re-launches the test
// binary; `std::process::Command::new` names a qualified path, and a bare `Command::new` counts when
// a use item in the file imports `Command` through a `process` path (a `clap::Command` builder does
// not launch).
function launchCall(body, text) {
  if (/\bcurrent_exe\s*\(/.test(body)) return 'current_exe';
  if (/\bprocess::Command::new\s*\(/.test(body)) return 'Command::new';
  if (/(?<![\w:])Command::new\s*\(/.test(body) && importsProcessCommand(text)) return 'Command::new';
  return null;
}

function importsProcessCommand(text) {
  for (const match of text.matchAll(/\buse\s+([^;]*);/g)) {
    if (/\bprocess::(?:\*|Command\b|\{[^}]*\bCommand\b[^}]*\})/.test(match[1])) return true;
  }
  return false;
}

// The attribute list a fn carries, walked backward from the fn keyword over its qualifiers
// (`pub`, `async`, `unsafe`, `extern`) and `#[...]` items. Comments and literals are already blanked,
// so a `#[test]` or `#[ignore]` here is a real attribute.
function attributesBefore(text, fnIndex) {
  const attributes = [];
  let index = fnIndex;
  for (;;) {
    while (index > 0 && /\s/.test(text[index - 1])) index -= 1;
    if (text[index - 1] === ']') {
      const open = attributeOpen(text, index - 1);
      if (open === -1) break;
      attributes.unshift(text.slice(open, index));
      index = open;
      continue;
    }
    if (text[index - 1] === ')') {
      const open = matchBackward(text, index - 1, '(', ')');
      if (open === -1) break;
      index = open;
      continue;
    }
    const word = /[A-Za-z_][A-Za-z0-9_]*$/.exec(text.slice(0, index));
    if (word !== null && ['pub', 'async', 'unsafe', 'extern', 'const'].includes(word[0])) {
      index -= word[0].length;
      continue;
    }
    break;
  }
  return attributes;
}

// The index of the '#' opening the attribute that closes at `close` (the index of its ']'), or -1
// when the brackets match something that is not an attribute.
function attributeOpen(text, close) {
  const open = matchBackward(text, close, '[', ']');
  return open !== -1 && text[open - 1] === '#' ? open - 1 : -1;
}

// The index of `open` matching the `close` at index `closeIndex`, or -1 on no match.
function matchBackward(text, closeIndex, open, close) {
  let depth = 0;
  for (let index = closeIndex; index >= 0; index -= 1) {
    if (text[index] === close) depth += 1;
    else if (text[index] === open) {
      depth -= 1;
      if (depth === 0) return index;
    }
  }
  return -1;
}

// The body text of the fn whose name match ends at `nameEnd`, or null for a declaration without a
// body (a trait item ends in ';'). The first '{' at parenthesis depth 0 opens the body: generics,
// argument lists and where clauses add parens and angles but no braces.
function functionBody(text, nameEnd) {
  let open = -1;
  let depth = 0;
  for (let index = nameEnd; index < text.length; index += 1) {
    const char = text[index];
    if (char === '(') depth += 1;
    else if (char === ')') depth -= 1;
    else if (depth === 0 && char === '{') {
      open = index;
      break;
    } else if (depth === 0 && char === ';') {
      return null;
    }
  }
  if (open === -1) return null;
  let braces = 1;
  for (let index = open + 1; index < text.length; index += 1) {
    if (text[index] === '{') braces += 1;
    else if (text[index] === '}') {
      braces -= 1;
      if (braces === 0) return text.slice(open + 1, index);
    }
  }
  return null;
}

// The nextest binary id the file compiles to: `<crate>::<test>` for a tests/<test>.rs target, the
// crate name for a lib target.
function binaryId(file) {
  const crateDir = file.split('/').slice(0, 4).join('/');
  if (!crateNames.has(crateDir)) {
    const manifest = readFileSync(path.join(repositoryRoot, crateDir, 'Cargo.toml'), 'utf8');
    const name = /name\s*=\s*"([^"]+)"/.exec(manifest)?.[1];
    if (name === undefined) throw new Error(`${crateDir}/Cargo.toml has no literal [package] name`);
    crateNames.set(crateDir, name);
  }
  const crateName = crateNames.get(crateDir);
  const inner = file.split('/').slice(4).join('/');
  const testTarget = /^tests\/([^/]+)\.rs$/.exec(inner) ?? /^tests\/([^/]+)\/main\.rs$/.exec(inner);
  return testTarget === null ? crateName : `${crateName}::${testTarget[1]}`;
}

// Comments and the contents of string, byte and char literals blanked to spaces so only real tokens
// remain. The result keeps the input's length: offsets and line numbers still match the source.
function stripRustSource(source) {
  const chars = source.split('');
  const blank = (from, to) => {
    for (let index = from; index < to; index += 1) {
      if (chars[index] !== '\n') chars[index] = ' ';
    }
  };
  const rawStringEnd = (index) => {
    // A raw (byte) string at index: r"..", r#+".."#+, br".." or br#+".."#+.
    let cursor = source[index] === 'b' && source[index + 1] === 'r' ? index + 2 : index + 1;
    let hashes = 0;
    while (source[cursor] === '#') {
      hashes += 1;
      cursor += 1;
    }
    if (source[cursor] !== '"') return -1;
    const closer = `"${'#'.repeat(hashes)}`;
    const end = source.indexOf(closer, cursor + 1);
    return end === -1 ? source.length : end + closer.length;
  };
  const escapedStringEnd = (index) => {
    let cursor = index + 1;
    while (cursor < source.length && source[cursor] !== '"') {
      cursor += source[cursor] === '\\' ? 2 : 1;
    }
    return Math.min(cursor + 1, source.length);
  };
  let index = 0;
  while (index < source.length) {
    const char = source[index];
    const pair = source.slice(index, index + 2);
    const wordBefore = index > 0 && /[A-Za-z0-9_]/.test(source[index - 1]);
    if (pair === '//') {
      const end = source.indexOf('\n', index);
      const stop = end === -1 ? source.length : end;
      blank(index, stop);
      index = stop;
    } else if (pair === '/*') {
      // Rust block comments nest.
      let depth = 1;
      let cursor = index + 2;
      while (cursor < source.length && depth > 0) {
        const two = source.slice(cursor, cursor + 2);
        if (two === '/*') {
          depth += 1;
          cursor += 2;
        } else if (two === '*/') {
          depth -= 1;
          cursor += 2;
        } else {
          cursor += 1;
        }
      }
      blank(index, cursor);
      index = cursor;
    } else if (!wordBefore && char === 'r' && (source[index + 1] === '"' || source[index + 1] === '#')) {
      // An r# without a quote is a raw identifier like r#match, not a string.
      const end = rawStringEnd(index);
      if (end === -1) {
        index += 1;
      } else {
        blank(index, end);
        index = end;
      }
    } else if (!wordBefore && char === 'b' && (source[index + 1] === '"' || source[index + 1] === 'r')) {
      // A b".." byte string keeps escapes; a br".." or br#+".."#+ raw byte string does not.
      const end = source[index + 1] === '"' ? escapedStringEnd(index + 1) : rawStringEnd(index);
      if (end === -1) {
        index += 1;
      } else {
        blank(index, end);
        index = end;
      }
    } else if (char === '"') {
      const stop = escapedStringEnd(index);
      blank(index, stop);
      index = stop;
    } else if (char === "'") {
      // A char literal is a quote, one character or escape, a quote; anything else is a lifetime.
      if (source[index + 1] === '\\') {
        const end = source.indexOf("'", index + 2);
        const stop = end === -1 ? source.length : end + 1;
        blank(index, stop);
        index = stop;
      } else if (source[index + 1] !== '\n' && source[index + 1] !== undefined && source[index + 2] === "'") {
        blank(index, index + 3);
        index += 3;
      } else {
        index += 1;
      }
    } else {
      index += 1;
    }
  }
  return chars.join('');
}
