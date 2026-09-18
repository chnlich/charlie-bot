'use strict';
// Math pre-render driver for `charliebot artifact wrap`: reads a content
// fragment, replaces every math span of the four KaTeX delimiter classes with
// katex.renderToString markup (pages ship pre-rendered), and writes the
// transformed fragment to stdout.
//
//   node scripts/prerender_math.js <fragment-path> <katex.min.js-path>
//
// The vendored katex.min.js (UMD build) is resolved by the caller
// (src/core/artifact_wrap.py) and passed explicitly.
//
// The delimiter scan is web/static/js/math-scanner.js's mathSpan — the same
// scanner the chat markdown extension tokenizes with. This driver adds the
// protected-range walk (which regions the scan never enters) and the
// katex.renderToString call; the matching case lists in
// tests/chat_math_extension.test.js (chat parse) and
// tests/core/test_artifact_wrap.py (this driver) pin both consumers to one
// behavior.

const fs = require('node:fs');
const { mathSpan } = require('../web/static/js/math-scanner.js');

const [fragmentPath, katexPath] = process.argv.slice(2);
if (!fragmentPath || !katexPath) {
  console.error('usage: node scripts/prerender_math.js <fragment-path> <katex.min.js-path>');
  process.exit(2);
}
const katex = require(katexPath);

function scanMathSpans(text) {
  const spans = [];
  let i = 0;
  while (i < text.length) {
    // Only a position that could open a span pays for the tail slice; prose
    // positions advance without copying.
    const hit = (text[i] === '$' || text[i] === '\\') ? mathSpan(text.slice(i)) : undefined;
    if (hit) {
      spans.push({ start: i, end: i + hit.raw.length, raw: hit.raw, display: hit.display });
      i += hit.raw.length;
    } else if (text[i] === '\\') {
      // Escape pair: the char after a backslash never opens a span (marked's
      // escape tokenizer does this on the chat path): \$ stays literal,
      // \\ consumes both backslashes so a following $ still opens.
      i += 2;
    } else {
      i++;
    }
  }
  return spans;
}

// Regions whose bytes the scan copies verbatim: the protected blocks (same tag
// list as renderChatMath's ignoredTags, minus option), HTML comments, and tag
// interiors — math inside them is never page text, and marked never matches
// there either. An unterminated protected block or comment runs to the end,
// mirroring how the DOM layer closes it.
const PROTECTED_OPEN_RE = /<(script|noscript|style|textarea|pre|code)(\s[^>]*)?>/gi;
const COMMENT_RE = /<!--[\s\S]*?-->/g;
const TAG_RE = /<\/?[A-Za-z][^>]*>/g;

function protectedRanges(html) {
  const ranges = [];
  let m;
  PROTECTED_OPEN_RE.lastIndex = 0;
  while ((m = PROTECTED_OPEN_RE.exec(html)) !== null) {
    const closeRe = new RegExp(`</${m[1]}\\s*>`, 'gi');
    closeRe.lastIndex = m.index;
    const cm = closeRe.exec(html);
    const end = cm ? cm.index + cm[0].length : html.length;
    ranges.push([m.index, end]);
    PROTECTED_OPEN_RE.lastIndex = end;
  }
  for (const re of [COMMENT_RE, TAG_RE]) {
    re.lastIndex = 0;
    while ((m = re.exec(html)) !== null) ranges.push([m.index, m.index + m[0].length]);
  }
  ranges.sort((a, b) => a[0] - b[0]);
  const merged = [];
  for (const range of ranges) {
    const last = merged[merged.length - 1];
    if (last && range[0] <= last[1]) last[1] = Math.max(last[1], range[1]);
    else merged.push([range[0], range[1]]);
  }
  return merged;
}

function prerenderMath(html) {
  const ranges = protectedRanges(html);
  let out = '';
  let cursor = 0;
  for (const [start, end] of ranges) {
    out += renderRun(html.slice(cursor, start));
    out += html.slice(start, end);
    cursor = end;
  }
  out += renderRun(html.slice(cursor));
  return out;
}

function renderRun(text) {
  const spans = scanMathSpans(text);
  let out = '';
  let cursor = 0;
  for (const span of spans) {
    out += text.slice(cursor, span.start);
    // Content between the delimiters, auto-render semantics; throwOnError:false
    // lands an unparseable formula as the deterministic red katex-error span.
    out += katex.renderToString(contentOf(span.raw), {
      throwOnError: false,
      displayMode: span.display,
    });
    cursor = span.end;
  }
  out += text.slice(cursor);
  return out;
}

function contentOf(raw) {
  if (raw.startsWith('$$')) return raw.slice(2, -2);
  if (raw.startsWith('\\')) return raw.slice(2, -2);
  return raw.slice(1, -1);
}

const html = fs.readFileSync(fragmentPath, 'utf8');
process.stdout.write(prerenderMath(html));
