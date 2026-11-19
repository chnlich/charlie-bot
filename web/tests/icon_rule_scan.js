// ---------------------------------------------------------------------------
// The icon scan behind the icon rule tests. It reads the scanned sources and returns every icon-like
// use that the six detection rules of plan 4.2 find, before the tests' exception list applies. A finding names
// its file, line, rule, matched text (the source text as written) and the source line it sits on. This module
// also holds the failure text the icon rule's four tests print.
// ---------------------------------------------------------------------------
const fs = require('node:fs');
const path = require('node:path');

// The failure text: a count, one line per problem, and the guidance sentence.
const GUIDE = '图标从图标模块取：模板 icon(key, size)，脚本 cbIcon(key, size)，Rust Icons::svg(key, size)；' +
  '新意思在 web/static/icons/icons.json 加一行，并用 dev/icons/fetch-lucide.sh 取文件';

function failure(headline, report) {
  return `${headline}:\n${report.join('\n')}\n${GUIDE}`;
}

const GLYPHS = new Set(['▸', '▾', '▴', '▶', '⚠', '✓', '✗', '✕', '×', '›', '↳', '↑', '↓', '→', '…']);
const NAMED_ENTITIES = {
  times: 0xd7, larr: 0x2190, uarr: 0x2191, rarr: 0x2192, darr: 0x2193, lsaquo: 0x2039, rsaquo: 0x203a,
  hellip: 0x2026, check: 0x2713, cross: 0x2717,
};
// An element name counts only where a tag opens: `<path d=`, `<rect/>`; the placeholder `<path>` in prose does not.
const SVG_ELEMENT = /<svg(?=[\s/>])|<(?:path|circle|rect|line|polyline|polygon|ellipse)(?=[\s/])/g;
const SVG_PATH_CONSTANT = /\b[A-Z][A-Z0-9_]*_SVG_PATH(?=\s*=\s*['"`])/g;
const ASCII_ELEMENT_GLYPH = />\s*[!+]\s*</g;
const TEXT_CONTENT_GLYPH = /textContent\s*=\s*(['"`])\s*[!+]\s*\1/g;
const ANIMATE_SPIN = /\banimate-spin\b/g;
const ROUNDED_FULL = /rounded-full/g;
const SIZE_CLASS = /\b([wh])-(\d+(?:\.\d+)?|\[\d+px\])(?=[\s"'`])/g;
// A dot, handle or spinner rule names its border-radius, pseudo-element or spinner class; the scan reads the rule
// around each such keyword.
const CSS_KEYWORDS = ['border-radius', '::before', '::after', 'spinner'];
const CSS_KEYWORD = /border-radius|::before|::after|spinner/g;
const MACRO_DEFINITION = /\{%-?\s*macro\s+(\w+)\s*\([^%]*%\}([\s\S]*?)\{%-?\s*endmacro\s*-?%\}/g;
const CLASS_CONTEXT = /class=|className|classList/;
// A class written as markup or a DOM class name: class="…", className = '…', classList.add('…').
function rawClassUse(line, name) {
  const opening = '(?:class(?:Name)?\\s*=\\s*|classList\\.(?:add|toggle|remove)\\(\\s*)';
  const pattern = `${opening}(['"\`])[^'"\`\\n]*?(?<![\\w-])${name}(?![\\w-])`;
  return new RegExp(pattern).test(line);
}
// One token per glyph, escape or reference; tokenCodePoints decodes each.
const TOKEN = new RegExp(
  [
    '[▸▾▴▶⚠✓✗✕×›↳↑↓→…]',
    '\\\\u\\{[0-9a-fA-F]{1,6}\\}',
    '\\\\u[dD][89abAB][0-9a-fA-F]{2}\\\\u[dD][c-fC-F][0-9a-fA-F]{2}',
    '\\\\u[0-9a-fA-F]{4}',
    '\\\\[0-9a-fA-F]{4}',
    '&#[xX][0-9a-fA-F]+;|&#\\d+;|&[a-zA-Z]+;',
    '[\\u{1F000}-\\u{1FAFF}]',
  ].join('|'),
  'gu',
);
const SKIPPED_DIRECTORIES = new Set(['node_modules', 'target', '.git']);
// A line that can hold an icon-table call: icon(, cbIcon(, or a Rust Icons method (.svg( or .script().
const CALL_MARK = /(?<![A-Za-z])icon\(|cbIcon\(|\.svg\(|\.script\(/;

// The lines that are only comment text: a whole-line `//`, `///`, `//!` or `#`-less JS and Rust comment, and the
// lines of a block comment `/* */`, `<!-- -->` or Jinja `{# #}`. Comment text is never drawn, so the scan skips it.
function commentLines(text) {
  const lines = text.split('\n');
  const flags = new Array(lines.length).fill(false);
  let close = null;
  lines.forEach((line, index) => {
    const trimmed = line.trim();
    if (close) {
      flags[index] = true;
      if (line.includes(close)) close = null;
      return;
    }
    if (trimmed.startsWith('//')) {
      flags[index] = true;
      return;
    }
    const opener = [['/*', '*/'], ['<!--', '-->'], ['{#', '#}']].find(([open]) => trimmed.startsWith(open));
    if (opener) {
      flags[index] = true;
      if (!trimmed.slice(opener[0].length).includes(opener[1])) close = opener[1];
    }
  });
  return flags;
}

// The files the icon rule scans, sorted, as absolute paths.
function scannedFiles(root) {
  const files = [];
  walk(path.join(root, 'web'), (file) => {
    const relative = path.relative(root, file).split(path.sep).join('/');
    if (relative.startsWith('web/tests/') || relative.startsWith('web/static/vendor/')) return;
    if (!/\.(html|js|css)$/.test(file) || path.basename(file).includes('tailwind')) return;
    files.push(file);
  });
  const promptsRoot = path.join(root, 'prompts');
  for (const name of fs.readdirSync(promptsRoot)) {
    if (/_template\.html$/.test(name)) files.push(path.join(promptsRoot, name));
  }
  walk(path.join(root, 'rust', 'crates'), (file) => {
    if (file.endsWith('.rs') && file.split(path.sep).includes('src')) files.push(file);
  });
  return files.sort();
}

function walk(directory, visit) {
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const full = path.join(directory, entry.name);
    if (entry.isDirectory()) {
      if (!SKIPPED_DIRECTORIES.has(entry.name)) walk(full, visit);
    } else if (entry.isFile()) {
      visit(full);
    }
  }
}

// The code points one token stands for: a glyph or emoji written raw, a \u or CSS \XXXX escape, or a reference.
function tokenCodePoints(token) {
  let match;
  if ((match = /^\\u\{([0-9a-fA-F]+)\}$/.exec(token))) return [parseInt(match[1], 16)];
  if ((match = /^\\u([dD][89abAB][0-9a-fA-F]{2})\\u([dD][c-fC-F][0-9a-fA-F]{2})$/.exec(token))) {
    const high = parseInt(match[1], 16) - 0xd800;
    const low = parseInt(match[2], 16) - 0xdc00;
    return [0x10000 + high * 0x400 + low];
  }
  if ((match = /^\\u([0-9a-fA-F]{4})$/.exec(token))) return [parseInt(match[1], 16)];
  if ((match = /^\\([0-9a-fA-F]{4})$/.exec(token))) return [parseInt(match[1], 16)];
  if ((match = /^&#[xX]([0-9a-fA-F]+);$/.exec(token))) return [parseInt(match[1], 16)];
  if ((match = /^&#(\d+);$/.exec(token))) return [parseInt(match[1], 10)];
  if ((match = /^&([a-zA-Z]+);$/.exec(token))) {
    return NAMED_ENTITIES[match[1]] === undefined ? [] : [NAMED_ENTITIES[match[1]]];
  }
  return [token.codePointAt(0)];
}

// The plain CSS rule whose selector or body holds index `at`: its opening brace, selector start, selector and body.
// A rule with braces inside its body (a media query) is not a dot or spinner rule and yields null.
function cssRuleAt(text, at) {
  const nextOpen = text.indexOf('{', at);
  const nextClose = text.indexOf('}', at);
  let open;
  if (nextOpen !== -1 && (nextClose === -1 || nextOpen < nextClose) && !text.slice(at, nextOpen).includes(';')) {
    open = nextOpen;
  } else {
    open = text.lastIndexOf('{', at);
    if (open === -1 || text.lastIndexOf('}', at) > open) return null;
  }
  const close = text.indexOf('}', open);
  if (close === -1) return null;
  const body = text.slice(open + 1, close);
  if (body.includes('{')) return null;
  const before = open - 1;
  const start = Math.max(...['{', '}', ';'].map((mark) => text.lastIndexOf(mark, before))) + 1;
  return { open, index: start, selector: text.slice(start, open), body };
}

function sizeInPixels(value) {
  const bracket = /^\[(\d+)px\]$/.exec(value);
  return bracket ? Number(bracket[1]) : Number(value) * 4;
}

// The names of the template macros whose body draws raw SVG. Their call sites are icon uses.
function svgMacroNames(templates) {
  const names = new Set();
  for (const text of templates) {
    for (const match of text.matchAll(MACRO_DEFINITION)) {
      if (new RegExp(SVG_ELEMENT.source).test(match[2])) names.add(match[1]);
    }
  }
  return names;
}

// Every finding in one file, with `plainClasses` collecting the single-class dot and spinner rules for the
// class-resolution pass in scanIconUses.
function scanFile(relative, text, macros, plainClasses) {
  let starts = null;
  let comments = null;
  const lineOf = (index) => {
    if (starts === null) {
      starts = [0];
      for (let at = text.indexOf('\n'); at !== -1; at = text.indexOf('\n', at + 1)) starts.push(at + 1);
    }
    let low = 0;
    let high = starts.length - 1;
    while (low < high) {
      const middle = (low + high + 1) >> 1;
      if (starts[middle] <= index) low = middle;
      else high = middle - 1;
    }
    return low + 1;
  };
  const sourceOf = (line) => text.slice(starts[line - 1], (starts[line] ?? text.length + 1) - 1).replace(/\r$/, '');
  const findings = [];
  const seen = new Set();
  const add = (index, rule, match) => {
    const line = lineOf(index);
    comments ??= commentLines(text);
    if (comments[line - 1]) return;
    const key = `${line}\t${rule}\t${match}`;
    if (seen.has(key)) return;
    seen.add(key);
    findings.push({ file: relative, line, rule, match, source: sourceOf(line) });
  };

  // A glyph is non-ASCII; an escape or reference is a backslash or an ampersand. A file with neither has no token.
  const tokens = /[^\x00-\x7f]|\\u|\\[0-9a-fA-F]{4}|&#|&[a-zA-Z]+;/.test(text) ? [...text.matchAll(TOKEN)] : [];
  for (const match of tokens) {
    const token = match[0];
    const codePoints = tokenCodePoints(token);
    if (codePoints.length !== 1) continue;
    const code = codePoints[0];
    if (GLYPHS.has(String.fromCodePoint(code))) {
      add(match.index, token.startsWith('&') ? 'html_entity' : 'unicode_glyph', token);
    } else if (code >= 0x1f000 && code <= 0x1faff) {
      add(match.index, 'emoji', token);
    }
  }

  for (const match of text.matchAll(SVG_ELEMENT)) add(match.index, 'inline_svg', match[0]);
  if (text.includes('_SVG_PATH')) {
    for (const match of text.matchAll(SVG_PATH_CONSTANT)) add(match.index, 'inline_svg', match[0]);
  }
  for (const name of macros) {
    for (const match of text.matchAll(new RegExp(`(?<!\\bmacro\\s+)\\b${name}\\(`, 'g'))) {
      add(match.index, 'inline_svg', `${name}(`);
    }
  }

  if (/[!+]\s*</.test(text)) {
    for (const match of text.matchAll(ASCII_ELEMENT_GLYPH)) {
      add(match.index, 'ascii_glyph', match[0].replace(/\s+/g, ''));
    }
  }
  if (text.includes('textContent')) {
    for (const match of text.matchAll(TEXT_CONTENT_GLYPH)) add(match.index, 'ascii_glyph', match[0]);
  }

  for (const match of text.includes('rounded-full') ? text.matchAll(ROUNDED_FULL) : []) {
    const start = text.lastIndexOf('\n', match.index) + 1;
    const end = text.indexOf('\n', match.index);
    const line = text.slice(start, end === -1 ? text.length : end);
    if (!CLASS_CONTEXT.test(line)) continue;
    const sizes = [...line.matchAll(SIZE_CLASS)];
    const width = sizes.find((size) => size[1] === 'w');
    const height = sizes.find((size) => size[1] === 'h');
    if (!width || !height) continue;
    if (sizeInPixels(width[2]) > 12 || sizeInPixels(height[2]) > 12) continue;
    add(start, 'css_shape', `${width[0]} ${height[0]} rounded-full`);
  }
  for (const match of text.matchAll(ANIMATE_SPIN)) {
    const start = text.lastIndexOf('\n', match.index) + 1;
    const end = text.indexOf('\n', match.index);
    if (rawClassUse(text.slice(start, end === -1 ? text.length : end), 'animate-spin')) {
      add(match.index, 'css_shape', 'animate-spin');
    }
  }

  const blocks = new Map();
  const keywords = CSS_KEYWORDS.some((word) => text.includes(word)) ? [...text.matchAll(CSS_KEYWORD)] : [];
  for (const keyword of keywords) {
    const rule = cssRuleAt(text, keyword.index);
    if (rule && !blocks.has(rule.open)) blocks.set(rule.open, rule);
  }
  for (const block of blocks.values()) {
    const selector = block.selector.trim();
    if (!selector) continue;
    const selectorIndex = block.index + block.selector.search(/\S/);
    const body = block.body;
    const pixels = (property) => {
      const found = new RegExp(`(?:^|[;\\s{])${property}\\s*:\\s*(\\d+(?:\\.\\d+)?)px`).exec(body);
      return found ? Number(found[1]) : null;
    };
    const width = pixels('width');
    const height = pixels('height');
    const small = width !== null && height !== null && width <= 12 && height <= 12;
    const plain = /^\.[\w-]+$/.test(selector);
    if (/border-radius\s*:\s*50%/.test(body) && small) {
      if (plain) plainClasses.push({ file: relative, line: lineOf(selectorIndex), name: selector.slice(1) });
      else add(selectorIndex, 'css_shape', selector);
    } else if (/::(before|after)\b/.test(selector) && /content\s*:/.test(body) && width !== null && height !== null) {
      add(selectorIndex, 'css_shape', selector);
    }
    if (plain && /spinner$/.test(selector)) {
      plainClasses.push({ file: relative, line: lineOf(selectorIndex), name: selector.slice(1), spinner: true });
    } else if (/\.[\w-]*spinner\b/.test(selector)) {
      add(selectorIndex, 'css_shape', selector);
    }
  }

  return findings;
}

// Every icon use the detection rules find under `root`, in file and line order.
function scanIconUses(root) {
  const files = scannedFiles(root);
  const texts = files.map((file) => fs.readFileSync(file, 'utf8'));
  const relatives = files.map((file) => path.relative(root, file).split(path.sep).join('/'));
  const macros = svgMacroNames(texts.filter((_, index) => files[index].endsWith('.html')));
  const plainClasses = [];
  const findings = [];
  files.forEach((_, index) => {
    findings.push(...scanFile(relatives[index], texts[index], macros, plainClasses));
  });
  // A plain class rule is drawn where raw markup uses the class. A class that only icon calls pass, as
  // icon('dot', 7, 'cb-dot'), is drawn by the module; a class with no use at all is drawn by its rule.
  for (const shape of plainClasses) {
    const word = new RegExp(`(?<![\\w-])${shape.name}(?![\\w-])`);
    let used = false;
    texts.forEach((text, index) => {
      if (!text.includes(shape.name) || !word.test(text) || relatives[index].endsWith('.css')) return;
      const lines = text.split('\n');
      const comments = commentLines(text);
      lines.forEach((line, lineIndex) => {
        if (comments[lineIndex] || !word.test(line)) return;
        used = true;
        if (!rawClassUse(line, shape.name)) return;
        const source = line.replace(/\r$/, '');
        const place = { file: relatives[index], line: lineIndex + 1 };
        findings.push({ ...place, rule: 'css_shape', match: `.${shape.name}`, source });
      });
    });
    if (!used) {
      findings.push({ file: shape.file, line: shape.line, rule: 'css_shape', match: `.${shape.name}`, source: '' });
    }
  }
  return findings;
}

// Every icon-table call with a literal key: icon(), cbIcon(), Icons::svg() and the keys of Icons::script().
function iconCallSites(root) {
  const sites = [];
  for (const file of scannedFiles(root)) {
    const relative = path.relative(root, file).split(path.sep).join('/');
    const text = fs.readFileSync(file, 'utf8');
    if (!CALL_MARK.test(text)) continue;
    const lines = text.split('\n');
    lines.forEach((source, index) => {
      if (!CALL_MARK.test(source)) return;
      const add = (key, api) => sites.push({ file: relative, line: index + 1, key, api, source });
      for (const match of source.matchAll(/(?<![\w.])icon\(\s*(['"])([^'"]+)\1/g)) add(match[2], 'icon');
      for (const match of source.matchAll(/\bcbIcon\(\s*(['"])([^'"]+)\1/g)) add(match[2], 'cbIcon');
      for (const match of source.matchAll(/\.svg\(\s*"([^"]+)"/g)) add(match[1], 'Icons::svg');
      for (const match of source.matchAll(/\.script\(Some\(&\[([^\]]*)\]/g)) {
        for (const key of match[1].matchAll(/"([^"]+)"/g)) add(key[1], 'Icons::script');
      }
    });
  }
  return sites;
}

module.exports = { scanIconUses, iconCallSites, failure, GUIDE };
