'use strict';
// ---------------------------------------------------------------------------
// Math-span scanner: one scan step over the four KaTeX delimiter classes
// ($...$, $$...$$, \(...\), \[...\]) in a raw source string.
// ---------------------------------------------------------------------------
// Single home of the delimiter scan both consumers run on raw text: the chat
// markdown extension (markdown-renderer.js's math tokenizer, over marked
// source) and the artifact-wrap pre-render driver (scripts/prerender_math.js,
// over HTML fragments). index.html loads this file before
// markdown-renderer.js; the prerender driver requires it under node. The
// export tail is inert in the browser.

function isWs(c) {
  return c === ' ' || c === '\t' || c === '\n' || c === '\r' || c === '\f' || c === '\v';
}

function isDigit(c) {
  return c >= '0' && c <= '9';
}

// Inline $...$: single line. Open: next char non-whitespace and non-$ (blocks
// "$5 and $10" currency, whose close candidate sits next to whitespace).
// Close: prev char non-whitespace non-$, next char non-digit.
function inlineDollar(src) {
  if (src[1] === undefined || isWs(src[1])) return undefined;
  for (let j = 1; j < src.length; j++) {
    const c = src[j];
    if (c === '\\') { j++; continue; }  // \$ never closes; \x pairs skip as content
    if (c === '\n') return undefined;
    if (c !== '$') continue;
    if (isWs(src[j - 1]) || src[j - 1] === '$') continue;
    if (isDigit(src[j + 1])) continue;
    return src.slice(0, j + 1);
  }
  return undefined;
}

// Display $$...$$: multi-line; the first $$ closes; any $ inside the content
// declines the span (no nested $ — a relaxed rule only adds misreads).
function displayDollar(src) {
  for (let j = 2; j < src.length; j++) {
    if (src[j] !== '$') continue;
    if (src[j + 1] !== '$') return undefined;
    if (j === 2) return undefined;  // empty content
    return src.slice(0, j + 2);
  }
  return undefined;
}

// \(...\) inline single-line, \[...\] display multi-line. The close check runs
// before the escape skip so the delimiter's own backslash is not consumed as
// an escape pair; \[ inside the content never re-opens. A \] immediately
// followed by ']' is not a close (display math with [N, 32] style trailing
// brackets).
function bracket(src, close, singleLine) {
  for (let j = 2; j < src.length; j++) {
    if (src.startsWith(close, j)) {
      if (close.endsWith(']') && src[j + 2] === ']') { j++; continue; }
      if (j === 2) return undefined;  // empty content
      return src.slice(0, j + 2);
    }
    if (src[j] === '\\') { j++; continue; }
    if (src[j] === '\n' && singleLine) return undefined;
  }
  return undefined;
}

// One scan step at a source position: the raw delimiter span and its display
// mode, or undefined when no delimiter class opens here.
function mathSpan(src) {
  let raw;
  let display;
  if (src.startsWith('$$')) {
    raw = displayDollar(src);
    display = true;
  } else if (src.startsWith('$')) {
    raw = inlineDollar(src);
    display = false;
  } else if (src.startsWith('\\[')) {
    raw = bracket(src, '\\]', false);
    display = true;
  } else if (src.startsWith('\\(')) {
    raw = bracket(src, '\\)', true);
    display = false;
  } else {
    return undefined;
  }
  return raw === undefined ? undefined : { raw, display };
}

if (typeof module !== 'undefined') module.exports = { mathSpan };
