#!/usr/bin/env node
// Yields the production code of Rust files: every line outside a test element, as `path:line:text`, the form `grep -n`
// prints, so a scan's pattern matches production lines only.
//
// A test element is what an attribute marks as test-only: #[cfg(test)] on an item, a field, a variant, a statement or a
// match arm, and #[test] or a path ending in ::test (such as #[tokio::test]) on a function. The element runs from its
// first attribute to its last token, so production code after a test module stays. Also left out: the external module
// file that a #[cfg(test)] `mod name;` names, the module files that file declares, and every file under a `tests/`
// directory. The lexer skips strings, raw strings, chars, lifetimes and nested block comments, so a brace inside them
// moves no element's end.
//
// A file that does not lex, whose delimiters do not balance, whose test element has no end this scan can find, or whose
// module file is missing or ambiguous fails with its path and line on stderr and exit 1. Nothing reaches stdout then.
// The stderr summary line names the tool's revision, a hash of this file's text.
//
// Usage: node dev/improve/rust-production-scan.mjs <file-or-directory>...

import { createHash } from 'node:crypto';
import { existsSync, readdirSync, readFileSync, statSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REVISION = createHash('sha256')
  .update(readFileSync(fileURLToPath(import.meta.url)))
  .digest('hex')
  .slice(0, 12);

const USAGE = 'usage: node rust-production-scan.mjs <file-or-directory>...\n';
const CLOSE_OF = new Map([
  ['(', ')'],
  ['[', ']'],
  ['{', '}'],
]);
const OPEN_OF = new Map([...CLOSE_OF].map(([open, close]) => [close, open]));
const RAW_PREFIXES = new Set(['r', 'br', 'cr']);
const ITEM_KEYWORDS = new Set(['fn', 'mod', 'impl', 'trait', 'struct', 'enum', 'union']);
const BLOCK_KEYWORDS = new Set(['if', 'match', 'for', 'while', 'loop', 'unsafe']);

class ScanError extends Error {}

// One file's text and the offsets where its lines start.
class Source {
  constructor(display, text) {
    this.display = display;
    this.text = text;
    this.lineStarts = [0];
    for (let i = 0; i < text.length; i++) {
      if (text[i] === '\n') this.lineStarts.push(i + 1);
    }
  }

  // The 1-based line that holds `offset`.
  lineOf(offset) {
    let low = 0;
    let high = this.lineStarts.length - 1;
    while (low < high) {
      const mid = (low + high + 1) >> 1;
      if (this.lineStarts[mid] <= offset) low = mid;
      else high = mid - 1;
    }
    return low + 1;
  }

  failAtLine(line, message) {
    return new ScanError(`${this.display}:${line}: ${message}`);
  }

  failAtOffset(offset, message) {
    return this.failAtLine(this.lineOf(offset), message);
  }
}

const UNICODE_WORD_CHAR = /^\p{XID_Continue}$/u;

function isWordChar(ch) {
  if (ch === undefined) return false;
  const code = ch.charCodeAt(0);
  if (code < 0x80) {
    const digit = code >= 0x30 && code <= 0x39;
    const upper = code >= 0x41 && code <= 0x5a;
    const lower = code >= 0x61 && code <= 0x7a;
    return code === 0x5f || digit || upper || lower;
  }
  return UNICODE_WORD_CHAR.test(ch);
}

function isPunct(token, text) {
  return token !== undefined && token.kind === 'punct' && token.text === text;
}

function isWord(token, text) {
  return token !== undefined && token.kind === 'word' && token.text === text;
}

// The index of the closing delimiter that matches the opening delimiter at `index`.
function closeOf(source, tokens, index) {
  const opener = tokens[index].text;
  const closer = CLOSE_OF.get(opener);
  let depth = 0;
  for (let i = index; i < tokens.length; i++) {
    const token = tokens[i];
    if (token.kind !== 'punct') continue;
    if (token.text === opener) depth++;
    else if (token.text === closer && --depth === 0) return i;
  }
  throw source.failAtOffset(tokens[index].start, `\`${opener}\` is never closed`);
}

function skipBlockComment(source, start) {
  const { text } = source;
  let depth = 0;
  let i = start;
  while (i < text.length) {
    if (text[i] === '/' && text[i + 1] === '*') {
      depth++;
      i += 2;
    } else if (text[i] === '*' && text[i + 1] === '/') {
      depth--;
      i += 2;
      if (depth === 0) return i;
    } else {
      i++;
    }
  }
  throw source.failAtOffset(start, 'unterminated block comment');
}

// A string literal that opens at `start` with `"`; returns the offset after its closing quote.
function skipQuoted(source, start) {
  const { text } = source;
  let i = start + 1;
  while (i < text.length) {
    if (text[i] === '\\') i += 2;
    else if (text[i] === '"') return i + 1;
    else i++;
  }
  throw source.failAtOffset(start, 'unterminated string literal');
}

// A raw string whose opening quote is at `quote` after `hashes` number signs; returns the offset after its close.
function skipRaw(source, quote, hashes) {
  const closing = `"${'#'.repeat(hashes)}`;
  const close = source.text.indexOf(closing, quote + 1);
  if (close < 0) throw source.failAtOffset(quote, 'unterminated raw string literal');
  return close + closing.length;
}

// A char literal or a lifetime that starts with the quote at `start`; returns its kind and the offset after it.
function charOrLifetime(source, start) {
  const { text } = source;
  if (text[start + 1] === '\\') {
    const close = text.indexOf("'", start + 3);
    if (close < 0 || text.slice(start, close).includes('\n')) {
      throw source.failAtOffset(start, 'unterminated character literal');
    }
    return ['char', close + 1];
  }
  const width = text.codePointAt(start + 1) > 0xffff ? 2 : 1;
  if (text[start + 1 + width] === "'") return ['char', start + 2 + width];
  let end = start + 1;
  while (isWordChar(text[end])) end++;
  if (end === start + 1) throw source.failAtOffset(start, 'unterminated character literal');
  return ['lifetime', end];
}

// A word, or a literal that a prefix turns into a string or char: r"", br#""#, b"", b'', c"", r#ident.
function lexWord(source, start) {
  const { text } = source;
  let end = start;
  while (isWordChar(text[end])) end++;
  const word = text.slice(start, end);
  if (RAW_PREFIXES.has(word)) {
    let hashes = 0;
    while (text[end + hashes] === '#') hashes++;
    if (text[end + hashes] === '"') return ['str', skipRaw(source, end + hashes, hashes)];
    if (word === 'r' && hashes === 1 && isWordChar(text[end + 1])) {
      let raw = end + 1;
      while (isWordChar(text[raw])) raw++;
      return ['word', raw];
    }
  }
  if ((word === 'b' || word === 'c') && text[end] === '"') return ['str', skipQuoted(source, end)];
  if (word === 'b' && text[end] === "'") return charOrLifetime(source, end);
  return ['word', end];
}

// The tokens of a file. Comments and whitespace are not tokens; each token keeps its offsets in the file text.
function lex(source) {
  const { text } = source;
  const tokens = [];
  let i = 0;
  while (i < text.length) {
    const ch = text[i];
    if (ch === '/' && text[i + 1] === '/') {
      const newline = text.indexOf('\n', i);
      i = newline < 0 ? text.length : newline;
      continue;
    }
    if (ch === '/' && text[i + 1] === '*') {
      i = skipBlockComment(source, i);
      continue;
    }
    if (ch === ' ' || ch === '\n' || ch === '\t' || ch === '\r') {
      i++;
      continue;
    }
    let kind = 'punct';
    let end = i + 1;
    if (ch === '"') {
      kind = 'str';
      end = skipQuoted(source, i);
    } else if (ch === "'") {
      [kind, end] = charOrLifetime(source, i);
    } else if (isWordChar(ch)) {
      [kind, end] = lexWord(source, i);
    }
    tokens.push({ kind, start: i, end, text: text.slice(i, end) });
    i = end;
  }
  return tokens;
}

// Fails at a closing delimiter that does not match the innermost opening one, and at an opening one that nothing
// closes. The scans below rely on these pairs.
function checkDelimiters(source, tokens) {
  const open = [];
  for (const token of tokens) {
    if (token.kind !== 'punct') continue;
    if (CLOSE_OF.has(token.text)) {
      open.push(token);
    } else if (OPEN_OF.has(token.text)) {
      const top = open.pop();
      if (top === undefined) throw source.failAtOffset(token.start, `unexpected \`${token.text}\``);
      if (CLOSE_OF.get(top.text) !== token.text) {
        throw source.failAtOffset(
          token.start,
          `\`${token.text}\` closes \`${top.text}\` from line ${source.lineOf(top.start)}`,
        );
      }
    }
  }
  const top = open.at(-1);
  if (top !== undefined) throw source.failAtOffset(top.start, `\`${top.text}\` is never closed`);
}

// The index of the `;` that ends an item or a `let`, at the element's own delimiter depth. A closing delimiter that
// comes first ends the element before it.
function semicolonEnd(source, tokens, start) {
  return scanEnd(source, tokens, start, false, false);
}

// The index of the `;` or the body's closing brace that ends an item whose body follows its signature.
function bodyOrSemicolonEnd(source, tokens, start) {
  let parens = 0;
  for (let i = start; i < tokens.length; i++) {
    const token = tokens[i];
    if (token.kind !== 'punct') continue;
    if (token.text === '(' || token.text === '[') parens++;
    else if (token.text === ')' || token.text === ']') parens--;
    else if (parens === 0 && token.text === ';') return i;
    else if (parens === 0 && token.text === '{') return closeOf(source, tokens, i);
  }
  throw source.failAtOffset(tokens[start].start, 'the test item has no body or `;` of its own');
}

// The index of the last token of the element that starts at `start`, where the element is a field, a variant, an
// argument, a statement or a match arm, and not an item. `fieldType` marks a `name: Type` field, whose type holds
// angle brackets that contain commas.
function scanEnd(source, tokens, start, commas, fieldType) {
  let depth = 0;
  let angle = 0;
  for (let i = start; i < tokens.length; i++) {
    const token = tokens[i];
    if (token.kind !== 'punct') continue;
    if (CLOSE_OF.has(token.text)) {
      depth++;
    } else if (OPEN_OF.has(token.text)) {
      if (--depth < 0) {
        if (i === start) throw source.failAtOffset(token.start, 'a test attribute marks nothing before this delimiter');
        return i - 1;
      }
    } else if (token.text === '<' && (fieldType || isPunct(tokens[i - 1], ':'))) {
      angle++;
    } else if (token.text === '>' && angle > 0 && !isAdjacentAfter(tokens[i - 1], token, '-')) {
      angle--;
    } else if (depth === 0 && angle === 0 && (token.text === ';' || (commas && token.text === ','))) {
      return i;
    }
  }
  throw source.failAtOffset(tokens[start].start, 'the element a test attribute marks has no end before the file ends');
}

// Whether `before` is the punctuation `text` written directly before `token`, as in `->` or `=>`.
function isAdjacentAfter(before, token, text) {
  return isPunct(before, text) && before.end === token.start;
}

// The index of the `=>` of a match arm that starts at `start`, or -1 when the element has no arm.
function armArrow(tokens, start) {
  let depth = 0;
  for (let i = start; i < tokens.length; i++) {
    const token = tokens[i];
    if (token.kind !== 'punct') continue;
    if (CLOSE_OF.has(token.text)) {
      depth++;
    } else if (OPEN_OF.has(token.text)) {
      if (--depth < 0) return -1;
    } else if (depth === 0 && (token.text === ';' || token.text === ',')) {
      return -1;
    } else if (depth === 0 && token.text === '=' && isAdjacentAfter(token, tokens[i + 1], '>')) {
      return i;
    }
  }
  return -1;
}

// The index of the last token of a block statement: an `if` chain, `match`, a loop, an `unsafe` or plain block, or a
// labelled loop. The block's closing brace ends it unless an `else` follows.
function blockStatementEnd(source, tokens, start) {
  let at = start;
  for (;;) {
    let parens = 0;
    let open = -1;
    for (let i = at; i < tokens.length && open < 0; i++) {
      const token = tokens[i];
      if (token.kind !== 'punct') continue;
      if (token.text === '(' || token.text === '[') parens++;
      else if (token.text === ')' || token.text === ']') parens--;
      else if (parens === 0 && token.text === '{') open = i;
    }
    if (open < 0) throw source.failAtOffset(tokens[start].start, 'the block a test attribute marks has no body');
    const close = closeOf(source, tokens, open);
    if (!isWord(tokens[close + 1], 'else')) return close;
    at = close + 2;
  }
}

// The index of the last token of the element that starts at `start`, when the element is not an item.
function elementEnd(source, tokens, start) {
  const token = tokens[start];
  if (
    isPunct(token, '{') ||
    (token.kind === 'word' && BLOCK_KEYWORDS.has(token.text)) ||
    (token.kind === 'lifetime' && isPunct(tokens[start + 1], ':'))
  ) {
    return blockStatementEnd(source, tokens, start);
  }
  if (token.kind === 'word' && isPunct(tokens[start + 1], '!') && isPunct(tokens[start + 2], '{')) {
    return closeOf(source, tokens, start + 2);
  }
  if (isWord(token, 'let')) return semicolonEnd(source, tokens, start);
  const arrow = armArrow(tokens, start);
  if (arrow >= 0) {
    const body = arrow + 2;
    if (isPunct(tokens[body], '{')) {
      const close = closeOf(source, tokens, body);
      return isPunct(tokens[close + 1], ',') ? close + 1 : close;
    }
    return scanEnd(source, tokens, body, true, false);
  }
  const fieldType = token.kind === 'word' && isPunct(tokens[start + 1], ':') && !isPunct(tokens[start + 2], ':');
  return scanEnd(source, tokens, start, true, fieldType);
}

// The index of the last token of the test element whose first token, after its attributes and visibility, is `start`.
function testEnd(source, tokens, start) {
  const token = tokens[start];
  if (token.kind === 'word') {
    switch (token.text) {
      case 'extern':
        if (isWord(tokens[start + 1], 'crate')) return semicolonEnd(source, tokens, start);
        if (isPunct(tokens[start + 1], '{')) return closeOf(source, tokens, start + 1);
        if (tokens[start + 1]?.kind === 'str' && isPunct(tokens[start + 2], '{')) {
          return closeOf(source, tokens, start + 2);
        }
        if (tokens[start + 1]?.kind === 'str') return testEnd(source, tokens, start + 2);
        return testEnd(source, tokens, start + 1);
      case 'const':
        if (['fn', 'unsafe', 'async', 'extern'].some((word) => isWord(tokens[start + 1], word))) {
          return testEnd(source, tokens, start + 1);
        }
        return semicolonEnd(source, tokens, start);
      case 'unsafe':
      case 'async':
      case 'default':
        if (tokens[start + 1]?.kind === 'word') return testEnd(source, tokens, start + 1);
        break;
      case 'static':
      case 'type':
      case 'use':
        return semicolonEnd(source, tokens, start);
      case 'macro_rules': {
        const delimiter = start + 3;
        if (isPunct(tokens[start + 1], '!') && tokens[start + 2]?.kind === 'word' && tokens[delimiter] !== undefined) {
          const close = closeOf(source, tokens, delimiter);
          if (tokens[delimiter].text === '{') return close;
          if (isPunct(tokens[close + 1], ';')) return close + 1;
        }
        throw source.failAtOffset(tokens[start].start, 'the production scan cannot bound this macro_rules item');
      }
      default:
        if (ITEM_KEYWORDS.has(token.text)) return bodyOrSemicolonEnd(source, tokens, start);
    }
  }
  return elementEnd(source, tokens, start);
}

// The attribute whose `[` is at `open`: its closing `]`, whether it marks a test, whether it is #[cfg(test)], and
// the value of a `#[path = "..."]`.
function readAttribute(source, tokens, open) {
  const close = closeOf(source, tokens, open);
  const body = tokens.slice(open + 1, close);
  const segments = [];
  let at = 0;
  while (at < body.length && (body[at].kind === 'word' || isPunct(body[at], ':'))) {
    if (body[at].kind === 'word') segments.push(body[at].text);
    at++;
  }
  const name = segments.join('::');
  const argument = body
    .slice(at)
    .map((token) => token.text)
    .join('');
  const cfgTest = name === 'cfg' && argument === '(test)';
  const pathAttr =
    name === 'path' && isPunct(body[at], '=') && body[at + 1]?.kind === 'str' ? body[at + 1].text.slice(1, -1) : null;
  return { close, test: cfgTest || segments.at(-1) === 'test', cfgTest, pathAttr };
}

// The index after the visibility that starts at `at` (`pub`, or `pub(...)`).
function visibilityEnd(source, tokens, at) {
  return isPunct(tokens[at + 1], '(') ? closeOf(source, tokens, at + 1) + 1 : at + 1;
}

// The offset where a test element's doc comments start. The `///` lines directly above the element's first attribute
// document that element, so they leave with it.
function docCommentStart(source, offset) {
  const lineOfAttribute = source.lineOf(offset);
  const lineStart = source.lineStarts[lineOfAttribute - 1];
  if (source.text.slice(lineStart, offset).trim() !== '') return offset;
  let line = lineOfAttribute;
  while (line > 1) {
    const above = source.text.slice(source.lineStarts[line - 2], source.lineStarts[line - 1]);
    if (!above.trimStart().startsWith('///')) break;
    line--;
  }
  return source.lineStarts[line - 1];
}

// Marks the test elements of one file. Returns the mask (1 for each offset in a test element), the module
// declarations with their inline-module path, and whether an inner #![cfg(test)] makes the whole file a test.
function analyze(source, tokens) {
  const mask = new Uint8Array(source.text.length);
  const decls = [];
  const inline = [];
  let depth = 0;
  let chain = null;
  let maskedUntil = -1;
  let isTest = false;
  let at = 0;
  while (at < tokens.length) {
    const token = tokens[at];
    const innerAttribute = isPunct(tokens[at + 1], '!') && isPunct(tokens[at + 2], '[');
    if (isPunct(token, '#') && (isPunct(tokens[at + 1], '[') || innerAttribute)) {
      const inner = isPunct(tokens[at + 1], '!');
      const attribute = readAttribute(source, tokens, inner ? at + 2 : at + 1);
      if (inner) {
        isTest ||= attribute.cfgTest;
      } else {
        chain ??= {
          start: docCommentStart(source, token.start),
          line: source.lineOf(token.start),
          test: false,
          pathAttr: null,
        };
        chain.test ||= attribute.test;
        chain.pathAttr ??= attribute.pathAttr;
      }
      at = attribute.close + 1;
      continue;
    }
    if (chain !== null && isWord(token, 'pub')) {
      at = visibilityEnd(source, tokens, at);
      continue;
    }
    const item = chain;
    chain = null;
    if (item !== null && item.test && at > maskedUntil) {
      const end = testEnd(source, tokens, at);
      mask.fill(1, item.start, tokens[end].end);
      maskedUntil = end;
    }
    if (isWord(token, 'mod') && tokens[at + 1]?.kind === 'word' && isPunct(tokens[at + 2], ';')) {
      decls.push({
        name: tokens[at + 1].text,
        pathAttr: item?.pathAttr ?? null,
        inline: inline.map((module) => module.name),
        test: (item?.test ?? false) || at <= maskedUntil,
        line: source.lineOf(token.start),
      });
    }
    if (isPunct(token, '{')) {
      depth++;
      if (isWord(tokens[at - 2], 'mod') && tokens[at - 1]?.kind === 'word') {
        inline.push({ name: tokens[at - 1].text, depth });
      }
    } else if (isPunct(token, '}')) {
      if (inline.at(-1)?.depth === depth) inline.pop();
      depth--;
    }
    at++;
  }
  if (chain !== null) throw source.failAtLine(chain.line, 'an attribute is followed by nothing');
  return { isTest, decls, mask };
}

const pathLoaded = new Set();

// Whether the module file at `display` owns the directory of its child modules: the crate roots, `mod.rs` files and
// the files a #[path] attribute loads. Any other module file owns the directory named after its stem.
function ownsChildDir(display) {
  const base = path.basename(display);
  const parent = path.basename(path.dirname(display));
  return (
    base === 'mod.rs' ||
    base === 'lib.rs' ||
    base === 'main.rs' ||
    ['bin', 'examples', 'benches'].includes(parent) ||
    pathLoaded.has(path.resolve(display))
  );
}

function childDirOf(display) {
  const dir = path.dirname(display);
  return ownsChildDir(display) ? dir : path.join(dir, path.basename(display, '.rs'));
}

// The file that the module declaration `decl` of `owner` loads. A missing or ambiguous file fails at the declaration.
function moduleFile(owner, decl) {
  if (decl.pathAttr !== null) {
    if (decl.inline.length > 0) {
      throw owner.source.failAtLine(decl.line, '#[path] on a module inside an inline module is not supported');
    }
    const file = path.join(path.dirname(owner.display), decl.pathAttr);
    if (!existsSync(file)) throw owner.source.failAtLine(decl.line, `#[path] names no file: ${file}`);
    pathLoaded.add(path.resolve(file));
    return file;
  }
  const dir = path.join(childDirOf(owner.display), ...decl.inline);
  const found = [path.join(dir, `${decl.name}.rs`), path.join(dir, decl.name, 'mod.rs')].filter((file) =>
    existsSync(file),
  );
  if (found.length !== 1) {
    const reason = found.length === 0 ? 'no file' : `both ${found.join(' and ')}`;
    throw owner.source.failAtLine(decl.line, `${reason} for \`mod ${decl.name};\``);
  }
  return found[0];
}

function parseFile(display) {
  const source = new Source(display, readFileSync(display, 'utf8'));
  const tokens = lex(source);
  checkDelimiters(source, tokens);
  return { display, source, ...analyze(source, tokens) };
}

// The `path:line:text` lines of one file's production code. A masked character prints as a space.
function productionLines(owner) {
  const { source, mask } = owner;
  const lines = [];
  source.lineStarts.forEach((start, index) => {
    const end = index + 1 < source.lineStarts.length ? source.lineStarts[index + 1] - 1 : source.text.length;
    let line = '';
    for (let offset = start; offset < end; offset++) line += mask[offset] ? ' ' : source.text[offset];
    line = line.replace(/\r$/, '');
    if (line.trim() !== '') lines.push(`${owner.display}:${index + 1}:${line}`);
  });
  return lines;
}

// Every .rs file under `dir`, sorted, skipping hidden entries and `tests` directories.
function walk(dir, out) {
  for (const name of readdirSync(dir).sort()) {
    if (name.startsWith('.')) continue;
    const entry = path.join(dir, name);
    if (statSync(entry).isDirectory()) {
      if (name !== 'tests') walk(entry, out);
    } else if (name.endsWith('.rs')) {
      out.push(entry);
    }
  }
}

// Whether a path has a component named `tests`, as the path is given.
function underTests(p) {
  return path.normalize(p).split(path.sep).includes('tests');
}

function scan(args) {
  const files = new Map();
  for (const arg of args) {
    if (underTests(arg)) {
      process.stderr.write(`rust-production-scan: skipped ${arg}: under a tests/ directory\n`);
    } else if (statSync(arg).isDirectory()) {
      const found = [];
      walk(arg, found);
      for (const file of found) files.set(path.resolve(file), file);
    } else {
      files.set(path.resolve(arg), arg);
    }
  }

  const analyses = new Map();
  const analysisOf = (display) => {
    const key = path.resolve(display);
    if (!analyses.has(key)) analyses.set(key, parseFile(display));
    return analyses.get(key);
  };
  const scanned = [...files.values()].map(analysisOf);

  const leftOut = new Set();
  const pending = [];
  const leaveOut = (display) => {
    const key = path.resolve(display);
    if (!leftOut.has(key)) {
      leftOut.add(key);
      pending.push(display);
    }
  };
  for (const owner of scanned) {
    if (owner.isTest) leaveOut(owner.display);
  }
  for (const owner of scanned) {
    for (const decl of owner.decls) {
      if (decl.test) leaveOut(moduleFile(owner, decl));
    }
  }
  while (pending.length > 0) {
    const owner = analysisOf(pending.pop());
    for (const decl of owner.decls) leaveOut(moduleFile(owner, decl));
  }

  const kept = scanned.filter((owner) => !leftOut.has(path.resolve(owner.display)));
  if (kept.length === 0) {
    throw new ScanError(`no production Rust file under ${args.join(' ')}`);
  }
  const lines = kept.flatMap(productionLines);
  process.stdout.write(lines.map((line) => `${line}\n`).join(''));
  process.stderr.write(
    `rust-production-scan ${REVISION}: ${kept.length} files, ${lines.length} production lines, ` +
      `${leftOut.size} test-module files left out\n`,
  );
  return 0;
}

function run(args) {
  if (args.length === 0) {
    process.stderr.write(USAGE);
    return 2;
  }
  for (const arg of args) {
    if (!existsSync(arg)) {
      process.stderr.write(`rust-production-scan: no such path: ${arg}\n`);
      return 2;
    }
    if (!statSync(arg).isDirectory() && !arg.endsWith('.rs')) {
      process.stderr.write(`rust-production-scan: not a Rust file or directory: ${arg}\n`);
      return 2;
    }
  }
  try {
    return scan(args);
  } catch (error) {
    if (!(error instanceof ScanError)) throw error;
    process.stderr.write(`rust-production-scan: ${error.message}\n`);
    return 1;
  }
}

process.exitCode = run(process.argv.slice(2));
