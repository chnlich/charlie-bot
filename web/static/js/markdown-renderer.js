// ---------------------------------------------------------------------------
// Fix nested code fences so marked.js doesn't close the outer fence early.
// When an outer ``` fence contains inner ``` fences, upgrade the outer
// delimiter to use more backticks/tildes than any nested fence.
// The fence-close rule (same char, len >= top.len, bare info string) lives in
// scanFences alone; fixNestedFences and openFenceTail are its two consumers.
// ---------------------------------------------------------------------------
// The fence-line rule both the scanner and the delimiter rewrite share.
var FENCE_RE = /^( {0,3})(`{3,}|~{3,})(.*)/;

function scanFences(lines) {
  var stack = [];
  var upgrades = {};  // lineIndex -> newLen

  for (var i = 0; i < lines.length; i++) {
    var m = lines[i].match(FENCE_RE);
    if (!m) continue;

    var delim = m[2];
    var char = delim[0];
    var len = delim.length;
    var info = m[3].trim();

    // Check if this closes the top-of-stack fence:
    // same char, len >= top.len, and no info string (bare fence).
    var top = stack.length > 0 ? stack[stack.length - 1] : null;
    if (top && char === top.char && len >= top.len && info === '') {
      // Closing fence
      if (top.maxInner > 0 && top.len <= top.maxInner) {
        var newLen = top.maxInner + 1;
        upgrades[top.line] = newLen;
        upgrades[i] = newLen;
      }
      var effectiveLen = (upgrades[top.line] != null) ? upgrades[top.line] : top.len;
      stack.pop();
      // Update parent's maxInner with effective len of the popped fence
      if (stack.length > 0) {
        var parent = stack[stack.length - 1];
        if (effectiveLen > parent.maxInner) parent.maxInner = effectiveLen;
      }
    } else {
      // Opening fence
      if (stack.length > 0) {
        var parent = stack[stack.length - 1];
        if (len > parent.maxInner) parent.maxInner = len;
      }
      stack.push({line: i, char: char, len: len, maxInner: 0});
    }
  }

  return upgrades;
}

function fixNestedFences(md) {
  // The scanner matches only fence lines (three or more backticks or tildes
  // at a line's start), so a draft carrying neither character run is identity
  // before the split-scan-join pass runs.
  if (md.indexOf('```') === -1 && md.indexOf('~~~') === -1) return md;
  var lines = md.split('\n');
  return applyFenceUpgrades(lines, scanFences(lines));
}

// The delimiter rewrite fixNestedFences applies, in reverse line order.
function applyFenceUpgrades(lines, upgrades) {
  // Apply upgrades in reverse line order
  var upgradeLines = Object.keys(upgrades).map(Number).sort(function(a, b) { return b - a; });
  for (var j = 0; j < upgradeLines.length; j++) {
    var lineIdx = upgradeLines[j];
    var newLen = upgrades[lineIdx];
    var line = lines[lineIdx];
    var oldMatch = line.match(FENCE_RE);
    if (!oldMatch) continue;
    var indent = oldMatch[1];
    var oldLen = oldMatch[2].length;
    var charType = oldMatch[2][0];
    var newDelim = '';
    for (var k = 0; k < newLen; k++) newDelim += charType;
    lines[lineIdx] = indent + newDelim + line.slice(indent.length + oldLen);
  }
  return lines.join('\n');
}

// The streaming paint's state, owned here and driven by usage.js's
// paintStreamDraft: null on every render path except the streaming paint's
// parse, where it holds an array that walkTokens fills with the parse's code
// tokens in document order. The block still growing at the draft's end is the
// LAST of them (an unterminated fence runs to EOF), so renderer.code can skip
// its highlight by token identity — no model of marked's block structure
// needed, which line-level fence scanning cannot supply (list and blockquote
// dedent the lines a fence rule would see). That block's content changes
// again before it settles, so a cache miss on it renders escaped-plain
// instead of re-running highlight per paint — the paint where the fence
// closes, and the committed render after the turn, highlight it once and the
// cache serves every later paint.
var streamPaintCodeTokens = null;

// The page parse's deferral state, driven by renderProseMarkdown below: true
// only around one of its parses, with the body text the flush needs to settle
// the memo entry. renderer.code records each deferred block's marker id, its
// (lang, code) highlight key, and the exact block html it emitted, so the
// flush can swap the highlighted bytes into both the DOM node and the memo
// entry without re-parsing the body.
var deferCodeHighlights = false;
var deferredBodyText = '';
var deferredSeq = 0;
var deferredBlocks = new Map();
var highlightFlushScheduled = false;

// Every streaming paint re-parses the whole accumulated draft, so unchanged
// code blocks re-highlight on every paint, and highlightAuto scores the block
// against every registered language (~0.26 s per 24 KB on the served
// highlight.js 11.9.0 common build). Highlight output is a pure function of
// (lang, code), so a bounded LRU serves repeat blocks without re-running it.
const highlightCache = new Map();
const HIGHLIGHT_CACHE_CAP = 32;
// One code-block emission, shared by every render mode so a deferred block's
// settled bytes stay byte-identical to the direct highlight's.
function codeBlockHtml(displayLang, isMarkdown, innerHtml, markerId) {
  const renderBtn = isMarkdown
    ? '<button class="copy-btn" onclick="renderMarkdown(this)">Render</button>'
    : '';
  const marker = markerId === null ? '' : ` data-hl="${markerId}"`;
  return `<div class="code-block"><div class="code-header"><span class="code-lang">${displayLang}</span>${renderBtn}<button class="copy-btn" onclick="copyCode(this)">Copy</button></div><pre><code class="hljs"${marker}>${wrapWideChars(innerHtml)}</code></pre></div>`;
}
// ---------------------------------------------------------------------------
// Wide-character 2ch boxes. The code font stack ('Fira Code', ui-monospace,
// monospace) carries no CJK glyphs, so a wide char falls back to a system
// font whose 1em advance is ~1.667 columns of the Latin mono font — while an
// ASCII diagram authored on a terminal reserves exactly 2 columns per wide
// char, and every CJK label pushes the right edge left by another 1/3
// column. CSS has no terminal-wcwidth equivalent, so the render side wraps
// each East_Asian_Width W/F character in a fixed 2ch inline box; `ch` follows
// the actually-rendered monospace font, so 2ch is exactly two Latin columns
// on every client, with or without Fira Code. The spans are layout-only and
// invisible to textContent, so copy and selection stay byte-identical.
//
// Ambiguous-width characters (box-drawing ─│, ±, ·, arrows) are NOT
// boxed: their glyphs come from the Latin monospace font itself and already
// render one column — which is exactly why the box-drawing rails of a CJK
// diagram align today while its CJK labels drift. Boxing them at 2ch would
// double widths that are already correct.
// ---------------------------------------------------------------------------

// Ordered [lo, hi] code-point ranges of East_Asian_Width W(ide) and
// F(ullwidth) — the terminal wcwidth 2-cell set — from Unicode 16.0.0's
// EastAsianWidth.txt (291 ranges, file order). Membership goes through this
// generated table and a binary search, not a \p{East_Asian_Width} escape:
// ECMAScript property escapes cover General_Category, Script and binary
// properties only, and East_Asian_Width throws SyntaxError in V8 — a regex
// literal is an early error, so it would take this whole file's parse down
// with it. Regenerate after a Unicode upgrade by re-running (output: the
// table below, 4 ranges per line):
//   curl -s https://www.unicode.org/Public/16.0.0/ucd/EastAsianWidth.txt | python3 -c "import re,sys;r=[(m[0],m[1] or m[0]) for m in re.findall(r'^([0-9A-F]{4,6})(?:\.\.([0-9A-F]{4,6}))?\s*;\s*[WF]',sys.stdin.read(),re.M)];print('\n'.join('  '+' '.join('[0x%s,0x%s],'%(r[j][0],r[j][1]) for j in range(i,min(i+4,len(r)))) for i in range(0,len(r),4)))"
var WC2CH_RANGES = [
  [0x1100,0x115F], [0x231A,0x231B], [0x2329,0x2329], [0x232A,0x232A],
  [0x23E9,0x23EC], [0x23F0,0x23F0], [0x23F3,0x23F3], [0x25FD,0x25FE],
  [0x2614,0x2615], [0x2630,0x2637], [0x2648,0x2653], [0x267F,0x267F],
  [0x268A,0x268F], [0x2693,0x2693], [0x26A1,0x26A1], [0x26AA,0x26AB],
  [0x26BD,0x26BE], [0x26C4,0x26C5], [0x26CE,0x26CE], [0x26D4,0x26D4],
  [0x26EA,0x26EA], [0x26F2,0x26F3], [0x26F5,0x26F5], [0x26FA,0x26FA],
  [0x26FD,0x26FD], [0x2705,0x2705], [0x270A,0x270B], [0x2728,0x2728],
  [0x274C,0x274C], [0x274E,0x274E], [0x2753,0x2755], [0x2757,0x2757],
  [0x2795,0x2797], [0x27B0,0x27B0], [0x27BF,0x27BF], [0x2B1B,0x2B1C],
  [0x2B50,0x2B50], [0x2B55,0x2B55], [0x2E80,0x2E99], [0x2E9B,0x2EF3],
  [0x2F00,0x2FD5], [0x2FF0,0x2FFF], [0x3000,0x3000], [0x3001,0x3003],
  [0x3004,0x3004], [0x3005,0x3005], [0x3006,0x3006], [0x3007,0x3007],
  [0x3008,0x3008], [0x3009,0x3009], [0x300A,0x300A], [0x300B,0x300B],
  [0x300C,0x300C], [0x300D,0x300D], [0x300E,0x300E], [0x300F,0x300F],
  [0x3010,0x3010], [0x3011,0x3011], [0x3012,0x3013], [0x3014,0x3014],
  [0x3015,0x3015], [0x3016,0x3016], [0x3017,0x3017], [0x3018,0x3018],
  [0x3019,0x3019], [0x301A,0x301A], [0x301B,0x301B], [0x301C,0x301C],
  [0x301D,0x301D], [0x301E,0x301F], [0x3020,0x3020], [0x3021,0x3029],
  [0x302A,0x302D], [0x302E,0x302F], [0x3030,0x3030], [0x3031,0x3035],
  [0x3036,0x3037], [0x3038,0x303A], [0x303B,0x303B], [0x303C,0x303C],
  [0x303D,0x303D], [0x303E,0x303E], [0x3041,0x3096], [0x3099,0x309A],
  [0x309B,0x309C], [0x309D,0x309E], [0x309F,0x309F], [0x30A0,0x30A0],
  [0x30A1,0x30FA], [0x30FB,0x30FB], [0x30FC,0x30FE], [0x30FF,0x30FF],
  [0x3105,0x312F], [0x3131,0x318E], [0x3190,0x3191], [0x3192,0x3195],
  [0x3196,0x319F], [0x31A0,0x31BF], [0x31C0,0x31E5], [0x31EF,0x31EF],
  [0x31F0,0x31FF], [0x3200,0x321E], [0x3220,0x3229], [0x322A,0x3247],
  [0x3250,0x3250], [0x3251,0x325F], [0x3260,0x327F], [0x3280,0x3289],
  [0x328A,0x32B0], [0x32B1,0x32BF], [0x32C0,0x32FF], [0x3300,0x33FF],
  [0x3400,0x4DBF], [0x4DC0,0x4DFF], [0x4E00,0x9FFF], [0xA000,0xA014],
  [0xA015,0xA015], [0xA016,0xA48C], [0xA490,0xA4C6], [0xA960,0xA97C],
  [0xAC00,0xD7A3], [0xF900,0xFA6D], [0xFA6E,0xFA6F], [0xFA70,0xFAD9],
  [0xFADA,0xFAFF], [0xFE10,0xFE16], [0xFE17,0xFE17], [0xFE18,0xFE18],
  [0xFE19,0xFE19], [0xFE30,0xFE30], [0xFE31,0xFE32], [0xFE33,0xFE34],
  [0xFE35,0xFE35], [0xFE36,0xFE36], [0xFE37,0xFE37], [0xFE38,0xFE38],
  [0xFE39,0xFE39], [0xFE3A,0xFE3A], [0xFE3B,0xFE3B], [0xFE3C,0xFE3C],
  [0xFE3D,0xFE3D], [0xFE3E,0xFE3E], [0xFE3F,0xFE3F], [0xFE40,0xFE40],
  [0xFE41,0xFE41], [0xFE42,0xFE42], [0xFE43,0xFE43], [0xFE44,0xFE44],
  [0xFE45,0xFE46], [0xFE47,0xFE47], [0xFE48,0xFE48], [0xFE49,0xFE4C],
  [0xFE4D,0xFE4F], [0xFE50,0xFE52], [0xFE54,0xFE57], [0xFE58,0xFE58],
  [0xFE59,0xFE59], [0xFE5A,0xFE5A], [0xFE5B,0xFE5B], [0xFE5C,0xFE5C],
  [0xFE5D,0xFE5D], [0xFE5E,0xFE5E], [0xFE5F,0xFE61], [0xFE62,0xFE62],
  [0xFE63,0xFE63], [0xFE64,0xFE66], [0xFE68,0xFE68], [0xFE69,0xFE69],
  [0xFE6A,0xFE6B], [0xFF01,0xFF03], [0xFF04,0xFF04], [0xFF05,0xFF07],
  [0xFF08,0xFF08], [0xFF09,0xFF09], [0xFF0A,0xFF0A], [0xFF0B,0xFF0B],
  [0xFF0C,0xFF0C], [0xFF0D,0xFF0D], [0xFF0E,0xFF0F], [0xFF10,0xFF19],
  [0xFF1A,0xFF1B], [0xFF1C,0xFF1E], [0xFF1F,0xFF20], [0xFF21,0xFF3A],
  [0xFF3B,0xFF3B], [0xFF3C,0xFF3C], [0xFF3D,0xFF3D], [0xFF3E,0xFF3E],
  [0xFF3F,0xFF3F], [0xFF40,0xFF40], [0xFF41,0xFF5A], [0xFF5B,0xFF5B],
  [0xFF5C,0xFF5C], [0xFF5D,0xFF5D], [0xFF5E,0xFF5E], [0xFF5F,0xFF5F],
  [0xFF60,0xFF60], [0xFFE0,0xFFE1], [0xFFE2,0xFFE2], [0xFFE3,0xFFE3],
  [0xFFE4,0xFFE4], [0xFFE5,0xFFE6], [0x16FE0,0x16FE1], [0x16FE2,0x16FE2],
  [0x16FE3,0x16FE3], [0x16FE4,0x16FE4], [0x16FF0,0x16FF1], [0x17000,0x187F7],
  [0x18800,0x18AFF], [0x18B00,0x18CD5], [0x18CFF,0x18CFF], [0x18D00,0x18D08],
  [0x1AFF0,0x1AFF3], [0x1AFF5,0x1AFFB], [0x1AFFD,0x1AFFE], [0x1B000,0x1B0FF],
  [0x1B100,0x1B122], [0x1B132,0x1B132], [0x1B150,0x1B152], [0x1B155,0x1B155],
  [0x1B164,0x1B167], [0x1B170,0x1B2FB], [0x1D300,0x1D356], [0x1D360,0x1D376],
  [0x1F004,0x1F004], [0x1F0CF,0x1F0CF], [0x1F18E,0x1F18E], [0x1F191,0x1F19A],
  [0x1F200,0x1F202], [0x1F210,0x1F23B], [0x1F240,0x1F248], [0x1F250,0x1F251],
  [0x1F260,0x1F265], [0x1F300,0x1F320], [0x1F32D,0x1F335], [0x1F337,0x1F37C],
  [0x1F37E,0x1F393], [0x1F3A0,0x1F3CA], [0x1F3CF,0x1F3D3], [0x1F3E0,0x1F3F0],
  [0x1F3F4,0x1F3F4], [0x1F3F8,0x1F3FA], [0x1F3FB,0x1F3FF], [0x1F400,0x1F43E],
  [0x1F440,0x1F440], [0x1F442,0x1F4FC], [0x1F4FF,0x1F53D], [0x1F54B,0x1F54E],
  [0x1F550,0x1F567], [0x1F57A,0x1F57A], [0x1F595,0x1F596], [0x1F5A4,0x1F5A4],
  [0x1F5FB,0x1F5FF], [0x1F600,0x1F64F], [0x1F680,0x1F6C5], [0x1F6CC,0x1F6CC],
  [0x1F6D0,0x1F6D2], [0x1F6D5,0x1F6D7], [0x1F6DC,0x1F6DF], [0x1F6EB,0x1F6EC],
  [0x1F6F4,0x1F6FC], [0x1F7E0,0x1F7EB], [0x1F7F0,0x1F7F0], [0x1F90C,0x1F93A],
  [0x1F93C,0x1F945], [0x1F947,0x1F9FF], [0x1FA70,0x1FA7C], [0x1FA80,0x1FA89],
  [0x1FA8F,0x1FAC6], [0x1FACE,0x1FADC], [0x1FADF,0x1FAE9], [0x1FAF0,0x1FAF8],
  [0x20000,0x2A6DF], [0x2A6E0,0x2A6FF], [0x2A700,0x2B739], [0x2B73A,0x2B73F],
  [0x2B740,0x2B81D], [0x2B81E,0x2B81F], [0x2B820,0x2CEA1], [0x2CEA2,0x2CEAF],
  [0x2CEB0,0x2EBE0], [0x2EBE1,0x2EBEF], [0x2EBF0,0x2EE5D], [0x2EE5E,0x2F7FF],
  [0x2F800,0x2FA1D], [0x2FA1E,0x2FA1F], [0x2FA20,0x2FFFD], [0x30000,0x3134A],
  [0x3134B,0x3134F], [0x31350,0x323AF], [0x323B0,0x3FFFD],
];

function wc2chIsWide(cp) {
  var lo = 0;
  var hi = WC2CH_RANGES.length - 1;
  while (lo <= hi) {
    var mid = (lo + hi) >> 1;
    var range = WC2CH_RANGES[mid];
    if (cp < range[0]) hi = mid - 1;
    else if (cp > range[1]) lo = mid + 1;
    else return true;
  }
  return false;
}

// Tag/text split of the single-pass scan: the capture group makes
// String.prototype.split keep each tag as an odd-index segment, and tags pass
// through verbatim, so a character inside a tag (attribute value, class name)
// is never wrapped. escapeText and highlight.js emit only ASCII tags and
// entities around escaped text, so wide chars only ever appear in text
// segments.
var WC2CH_TAG_SPLIT_RE = /(<[^>]*>)/g;
// Trailing marks that render inside the wide char's glyph run and share its
// box: General_Category=Mark (a legal \p escape, unlike East_Asian_Width).
// U+FE0F is itself Mn, so the class already carries it; the explicit check
// just mirrors the contract's wording. A mark never OPENS a box — only a
// W/F char does.
var WC2CH_MARK_RE = /\p{M}/u;

function wc2chWrapText(text, out) {
  var i = 0;
  while (i < text.length) {
    var cp = text.codePointAt(i);
    if (!wc2chIsWide(cp)) {
      // One UTF-16 code unit; non-wide chars — and any lone surrogate — pass
      // through byte-identically.
      out.push(text[i]);
      i += 1;
      continue;
    }
    // A wide char opens one 2ch box; trailing combining marks and U+FE0F join
    // it (they shape the same glyph run). Surrogate pairs are consumed whole
    // by the code-point scan, so both units land in the same box.
    var end = i + (cp > 0xFFFF ? 2 : 1);
    while (end < text.length) {
      var tail = text.codePointAt(end);
      if (tail !== 0xFE0F && !WC2CH_MARK_RE.test(String.fromCodePoint(tail))) break;
      end += tail > 0xFFFF ? 2 : 1;
    }
    out.push('<span class="wc2ch">', text.slice(i, end), '</span>');
    i = end;
  }
}

// One linear pass over a code block's inner HTML: tags verbatim, text
// segments with each W/F character in exactly one 2ch box. A block without
// wide chars round-trips byte-identically (split/join plus unit copies), so
// today's bytes are preserved wherever the fix does not apply.
function wrapWideChars(html) {
  var parts = html.split(WC2CH_TAG_SPLIT_RE);
  var out = [];
  for (var i = 0; i < parts.length; i++) {
    if (i % 2 === 1) out.push(parts[i]);  // odd segments are the captured tags
    else wc2chWrapText(parts[i], out);
  }
  return out.join('');
}

function highlightKey(lang, code) {
  return lang + '\u0000' + code;
}
function cachedHighlight(lang, code, run) {
  const key = highlightKey(lang, code);
  let value = highlightCache.get(key);
  if (value !== undefined) {
    highlightCache.delete(key);
    highlightCache.set(key, value);
    return value;
  }
  value = run();
  highlightCache.set(key, value);
  if (highlightCache.size > HIGHLIGHT_CACHE_CAP) {
    highlightCache.delete(highlightCache.keys().next().value);
  }
  return value;
}

// A complete block's raw ends on its closing fence line (marked strips the
// trailing newline from a terminated token's raw but keeps it on an
// unterminated one); an unterminated block's raw ends on content. The one
// shape this misreads — a long-fence draft whose content ends on a shorter
// bare fence line — reads as complete and keeps today's per-paint highlight,
// so a misread costs coverage, never bytes.
function endsOnClosingFence(raw) {
  var body = raw.endsWith('\n') ? raw.slice(0, -1) : raw;
  var lastLine = body.slice(body.lastIndexOf('\n') + 1);
  return /^ {0,3}(?:`{3,}|~{3,})[ \t]*$/.test(lastLine);
}

// The streaming paint's parse: lex once, record the code tokens by plain
// recursion (marked's walkTokens hook routes the same walk through
// Promise.all — ~215k promise allocations per replay on the 98 KB draft
// corpus), then render those same token objects, so the renderer's identity
// check sees what was recorded.
//
// The parse is incremental across paints: the frozen prefix's HTML carries
// forward, and only the tail after the last safe boundary re-lexes. A
// boundary is safe when a blank line ends it and nothing can cross the blank:
// a list continues across blank lines and a code block's unterminated fence
// runs to EOF, so a cut may follow a `space` token only when the token before
// it is neither. Reference definitions resolve document-wide, so the first
// paint whose new text carries one re-parses whole from then on (hasDefs) —
// a frozen prefix could not resolve a tail reference the full parse resolves.
var streamParseState = null;
// The label class matches marked's def tokenizer (an escaped char or any
// non-bracket non-backslash), so a definition the lexer accepts — including
// one with an escaped `]` in the label — can never slip past this guard.
var STREAM_REF_DEF_RE = /^[ \t]{0,3}\[(?:\\.|[^\[\]\n\\])+\]:/m;

function streamSafeCut(tokens, source, base) {
  // Latest absolute end offset of a `space` token whose preceding token is
  // neither a list nor a code block, or -1. Token raws do not tile the input —
  // a link reference definition is consumed into tokens.links with no token
  // emitted — so offsets come from locating each raw in the source, not from
  // summing lengths.
  var cursor = base;
  var cut = -1;
  var index = -1;
  for (var i = 0; i < tokens.length; i++) {
    var found = source.indexOf(tokens[i].raw, cursor);
    if (found < 0) return { cut: -1, index: -1 };
    var end = found + tokens[i].raw.length;
    // A cut is only safe at a line start: a blank line's trailing spaces can
    // carry the next line's indentation (a partially streamed `    code`
    // indent lexes as a code block from the line start but as a paragraph
    // from three spaces in), so a space token that stops mid-line never cuts.
    if (i > 0 && tokens[i].type === 'space' && tokens[i].raw.slice(-1) === '\n') {
      var prev = tokens[i - 1].type;
      if (prev !== 'list' && prev !== 'code') {
        cut = end;
        index = i + 1;
      }
    }
    cursor = end;
  }
  return { cut: cut, index: index };
}

function parseStreamDraft(fixed) {
  if (streamPaintCodeTokens === null) return marked.parse(fixed);
  var state = streamParseState;
  if (state !== null && !state.hasDefs && fixed.startsWith(state.fixed)) {
    var tail = fixed.slice(state.cut);
    if (!STREAM_REF_DEF_RE.test(tail)) {
      var tailTokens = marked.lexer(tail);
      recordCodeTokens(tailTokens);
      var html = state.html;
      var cut = streamSafeCut(tailTokens, fixed, state.cut);
      if (cut.cut < 0) {
        streamParseState = { fixed: fixed, cut: state.cut, html: state.html, hasDefs: false };
        html += tailTokens.length ? marked.parser(tailTokens) : '';
      } else {
        // The absorbed span renders a second time into the frozen prefix; its
        // code blocks are never the recorder's last token (a cut's space token
        // follows a non-code token), so the identity skip cannot hide them.
        var absorbed = tailTokens.slice(0, cut.index);
        absorbed.links = tailTokens.links;
        streamParseState = { fixed: fixed, cut: cut.cut, html: state.html + marked.parser(absorbed), hasDefs: false };
        html += marked.parser(tailTokens);
      }
      return html;
    }
  }
  var tokens = marked.lexer(fixed);
  recordCodeTokens(tokens);
  var fullCut = streamSafeCut(tokens, fixed, 0);
  var hasDefs = STREAM_REF_DEF_RE.test(fixed);
  if (fullCut.cut >= 0) {
    // The state carries the rendered prefix, so the next paint is the frozen
    // HTML plus the tail's render. The prefix re-render hits the highlight
    // cache the full render just filled.
    var prefix = tokens.slice(0, fullCut.index);
    prefix.links = tokens.links;
    streamParseState = { fixed: fixed, cut: fullCut.cut, html: marked.parser(prefix), hasDefs: hasDefs };
  } else {
    streamParseState = null;
  }
  return marked.parser(tokens);
}

function recordCodeTokens(tokens) {
  for (var i = 0; i < tokens.length; i++) {
    var token = tokens[i];
    if (token.type === 'code') streamPaintCodeTokens.push(token);
    if (token.tokens) recordCodeTokens(token.tokens);
    if (token.items) {
      for (var j = 0; j < token.items.length; j++) {
        if (token.items[j].tokens) recordCodeTokens(token.items[j].tokens);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Marked.js renderer: highlight.js syntax highlighting + code block headers
// ---------------------------------------------------------------------------
(function() {
  // This file loads before chat/shared.js, so escapeHtml is unavailable here. Local
  // text/attribute escapers keep message-text tags and
  // attributes from ever becoming DOM nodes (invariant: rendered chat message body
  // contains no tag and no attribute that originated from the message text itself).
  function escapeText(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }
  function escapeAttr(s) {
    return escapeText(s).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  const renderer = new marked.Renderer();
  // The growing tail's escape, carried across paints: escapeText maps each
  // character independently, so escape(prefix + delta) is escape(prefix) +
  // escape(delta) exactly, and a tail that grows by appends re-escapes only
  // the new bytes. The skip path refreshes both strings every paint it runs;
  // any other paint leaves them stale for the next stream's first paint to
  // replace.
  var streamTailRaw = '';
  var streamTailEscaped = '';
  function escapeStreamTail(trimmed) {
    let escaped;
    if (trimmed.startsWith(streamTailRaw)) {
      escaped = streamTailEscaped + escapeText(trimmed.slice(streamTailRaw.length));
    } else {
      escaped = escapeText(trimmed);
    }
    streamTailRaw = trimmed;
    streamTailEscaped = escaped;
    return escaped;
  }
  renderer.html = function(token) {
    // Support both marked v4 (html string) and v5+ ({ text } object), same
    // tolerance as renderer.code. Escape so raw tags render as literal text.
    const text = typeof token === 'object' ? token.text : token;
    return escapeText(text);
  };
  renderer.code = function(token) {
    // Support both marked v4 (code, lang, escaped) and v5+ ({ text, lang })
    const code = typeof token === 'object' ? token.text : token;
    const lang = (typeof token === 'object' ? token.lang : arguments[1]) || '';
    const trimmed = code.replace(/\n$/, '');
    const resolvedLang = (lang && hljs.getLanguage(lang)) ? lang : '';
    // The growing tail skips by token identity: walkTokens recorded this
    // parse's code tokens in document order, the unterminated block is the
    // last of them, and its raw not ending on a closing fence says it never
    // closed. The cache check first keeps a completed block that shares the
    // tail's cache key on today's highlighted bytes.
    const isGrowingTail = streamPaintCodeTokens !== null
      && !highlightCache.has(highlightKey(resolvedLang, trimmed))
      && token === streamPaintCodeTokens[streamPaintCodeTokens.length - 1]
      && !endsOnClosingFence(token.raw);
    const displayLang = escapeText(lang || 'text');
    const isMarkdown = (lang === 'markdown' || lang === 'md');
    // The page parse defers the highlight off the first paint (see
    // renderProseMarkdown); the growing tail's own escape already covers the
    // streaming shape, and the two deferrals never co-occur because
    // streamPaintCodeTokens is non-null only inside the streaming paint.
    if (deferCodeHighlights && !isGrowingTail) {
      const id = String(++deferredSeq);
      deferredBlocks.set(id, {
        lang: resolvedLang,
        code: trimmed,
        text: deferredBodyText,
        displayLang,
        isMarkdown,
        plainBlock: codeBlockHtml(displayLang, isMarkdown, escapeText(trimmed), id),
      });
      return codeBlockHtml(displayLang, isMarkdown, escapeText(trimmed), id);
    }
    const run = () => (resolvedLang
      ? hljs.highlight(trimmed, { language: resolvedLang }).value
      : hljs.highlightAuto(trimmed).value);
    const highlighted = isGrowingTail
      ? escapeStreamTail(trimmed)
      : cachedHighlight(resolvedLang, trimmed, run);
    return codeBlockHtml(displayLang, isMarkdown, highlighted, null);
  };
  renderer.link = function(token) {
    const href = escapeAttr(token.href);
    const title = token.title ? ` title="${escapeAttr(token.title)}"` : '';
    // Parse inline tokens so nested **bold** / inline code keep rendering while a
    // raw tag in link text gets escaped by renderer.html. Fall back to escaped
    // token.text when token.tokens is missing. this.parser is available to a
    // renderer registered via marked.use({ renderer }) on the served marked v15.
    const text = token.tokens
      ? this.parser.parseInline(token.tokens)
      : escapeText(token.text);
    return `<a href="${href}" target="_blank" rel="noopener noreferrer"${title}>${text}</a>`;
  };
  renderer.image = function(token) {
    const alt = escapeAttr(token.text);
    const src = escapeAttr(token.href);
    const title = token.title ? ` title="${escapeAttr(token.title)}"` : '';
    return `<img src="${src}" alt="${alt}"${title}>`;
  };
  // Models write a bare ~ for "approximately"; marked's inline del rule is
  // /^(~~?)/ so two lone tildes cross-pair into one <del>. Only let ~~ enter
  // the default del tokenizer; a lone ~ is plain text. Returning undefined for
  // a source that does not start with ~ lets the normal text tokenizer consume
  // the rest of the prose unchanged. Registered in the same use() as the
  // renderer so a single marked.use drives every chat surface.
  marked.use({ renderer, tokenizer: {
    del(src) {
      if (typeof src === 'string' && src.startsWith('~~')) return false;
      if (typeof src === 'string' && src.startsWith('~')) return { type: 'text', raw: '~', text: '~' };
      return undefined;
    },
    // A chat URL is a maximal printable-ASCII run [\x21-\x7E]+: anything glued
    // onto it that is not printable ASCII (CJK, full-width punctuation, curly
    // quotes, emoji, any non-ASCII prose) is prose, and the link ends just
    // before the first such character. Stock marked stops a bare URL only at
    // whitespace, so a URL before Chinese prose swallows the tail into href and
    // link text, and the polluted href then fails every downstream consumer
    // (artifact-card regex, missing-file probe, click). chat/artifacts.js's
    // FILE_SERVER_LINK_SOURCE stops at the same boundary when it scans code and
    // text carriers for file-server links — two copies of one definition, kept
    // in sync by the comments in both files (the modules load independently, so
    // sharing a constant would couple their load order).
    url(src) {
      const cap = this.rules.inline.url.exec(src);
      if (!cap) return undefined;
      if (cap[2] === '@') {
        // The email branch replicates stock marked byte-for-byte: this override
        // shadows the stock url tokenizer, so without it emails stop autolinking.
        return {
          type: 'link',
          raw: cap[0],
          text: cap[0],
          href: 'mailto:' + cap[0],
          tokens: [{ type: 'text', raw: cap[0], text: cap[0] }],
        };
      }
      // Interleave to a fixed point: _backpedal trims trailing ASCII punctuation
      // (an unbalanced `)` or a sentence-final period) exactly as stock marked
      // does, then the ASCII-boundary cut; a cut can expose a fresh ASCII
      // punctuation tail, so iterate until neither rule moves the boundary. A
      // pure-ASCII match never moves past the backpedal, so every ASCII
      // behavior stays byte-identical to stock marked.
      let run = cap[0];
      let prev;
      do {
        prev = run;
        const backpedal = this.rules.inline._backpedal.exec(run);
        run = (backpedal && backpedal[0]) || '';
        const cut = run.search(/[^\x21-\x7E]/);
        if (cut !== -1) run = run.slice(0, cut);
      } while (run !== prev);
      // Nothing left of the match: no link; the text tokenizer takes the prose.
      if (run === '') return undefined;
      return {
        type: 'link',
        raw: run,
        text: run,
        href: cap[1] === 'www.' ? 'http://' + run : run,
        tokens: [{ type: 'text', raw: run, text: run }],
      };
    }
  }
});
})();

// ---------------------------------------------------------------------------
// Memoized message-body parse for the chat message path.
//
// Every session switch rebuilds the turn engine, so the same page's message
// bodies re-run marked.parse on every re-entry (and every repeat render of an
// unchanged body). The parse is a pure function of the text — the renderer and
// tokenizer are registered once at load — so a bounded LRU serves repeat
// renders. Cap holds the 64 most recently rendered bodies: the tail pages a
// re-entry renders. The streaming draft paint (usage.js) stays off this memo
// on purpose: its content grows every delta, so it would only evict.
//
// A body's first parse defers the code highlight off the first paint: the
// highlight is pure per-block work (highlightAuto scores every registered
// language, ~0.26 s per 24 KB on the served common build — the dominant slice
// of a code-heavy page's cold render), so the parse emits escaped-plain blocks
// with data-hl markers and the scheduled flush swaps the highlighted bytes
// into the DOM nodes and the memo entry, timeboxed across frames. A memo hit
// on a settled entry renders highlighted bytes directly; on a not-yet-flushed
// entry it re-emits the same markers, which the one pending flush covers.
// ---------------------------------------------------------------------------
const PROSE_PARSE_CACHE_CAP = 64;
const proseParseCache = new Map();
function renderProseMarkdown(text) {
  let html = proseParseCache.get(text);
  if (html !== undefined) {
    proseParseCache.delete(text);
    proseParseCache.set(text, html);
    return html;
  }
  deferCodeHighlights = true;
  deferredBodyText = text;
  try {
    html = marked.parse(fixNestedFences(text));
  } finally {
    deferCodeHighlights = false;
  }
  proseParseCache.set(text, html);
  if (proseParseCache.size > PROSE_PARSE_CACHE_CAP) {
    proseParseCache.delete(proseParseCache.keys().next().value);
  }
  scheduleCodeHighlightFlush();
  return html;
}

function scheduleCodeHighlightFlush(root) {
  // The turn-engine prerender post-processes detached fragments; a root
  // handed in here while records are pending joins the flush's sweep, so its
  // markers get the swap even before the fragment attaches.
  if (root && deferredBlocks.size) registerHighlightRoot(root);
  if (highlightFlushScheduled || !deferredBlocks.size) return;
  highlightFlushScheduled = true;
  const run = () => {
    highlightFlushScheduled = false;
    flushDeferredCodeHighlights();
  };
  // The rAF frame is the upgrade slot: the plain first paint is already on
  // screen, and the swap lands before the next one. setTimeout carries the
  // vm harnesses, which define no rAF.
  if (typeof requestAnimationFrame === 'function') requestAnimationFrame(run);
  else setTimeout(run, 0);
}

// Roots whose renders still carry unswapped markers. WeakRefs let a discarded
// render's tree be collected; the WeakSet dedupes repeated registrations of
// the same root.
var highlightRootRefs = new Set();
var highlightRootsSeen = new WeakSet();
var HIGHLIGHT_FLUSH_RETRY_MS = 250;
var HIGHLIGHT_FLUSH_MAX_ATTEMPTS = 120;

function registerHighlightRoot(root) {
  if (!root || highlightRootsSeen.has(root)) return;
  highlightRootsSeen.add(root);
  if (typeof WeakRef === 'function') highlightRootRefs.add(new WeakRef(root));
}

function flushDeferredCodeHighlights() {
  if (!deferredBlocks.size) return;
  // The flush swaps the highlighted bytes into every marker the document and
  // the registered roots still hold — the turn-engine prerenders atoms
  // detached and holds the fragment alive until the segment materializes, so
  // a root sweep settles the block before it ever attaches. The 8 ms timebox
  // bounds each pass's highlight work — highlightAuto on one block can exceed
  // it, so the deadline is checked between records. A record no sweep finds
  // retries on a bounded backoff (the attach can land much later, on a
  // scroll) and gives up after HIGHLIGHT_FLUSH_MAX_ATTEMPTS; the memo entry
  // settles on the first pass either way, so every later render serves the
  // settled bytes.
  const deadline = performance.now() + 8;
  const roots = [];
  for (const ref of highlightRootRefs) {
    const root = ref.deref();
    if (root) roots.push(root);
    else highlightRootRefs.delete(ref);
  }
  for (const [id, rec] of deferredBlocks) {
    // The settled bytes build once per record and the retries only re-sweep
    // for markers: a pass that finds none repeats up to
    // HIGHLIGHT_FLUSH_MAX_ATTEMPTS times, so re-running the block build and
    // the memo swap per pass turns every bounded retry into that work again.
    // The memo swap rides the same first build — plainBlock embeds the
    // record's unique id, so a later pass can never find it again.
    if (rec.settledBlock === undefined) {
      const run = () => (rec.lang
        ? hljs.highlight(rec.code, { language: rec.lang }).value
        : hljs.highlightAuto(rec.code).value);
      rec.highlighted = cachedHighlight(rec.lang, rec.code, run);
      rec.settledBlock = codeBlockHtml(rec.displayLang, rec.isMarkdown, rec.highlighted, null);
      const cached = proseParseCache.get(rec.text);
      if (cached !== undefined && cached.includes(rec.plainBlock)) {
        // The replacer function keeps the highlighted bytes literal: String's
        // replacement string would read $$/$&/$`/$' patterns out of them.
        proseParseCache.set(rec.text, cached.replace(rec.plainBlock, () => rec.settledBlock));
      }
    }
    const selector = `code[data-hl="${id}"]`;
    let found = false;
    for (const el of document.querySelectorAll(selector)) {
      // Same wrap codeBlockHtml applied to the settled memo bytes: the DOM
      // write and the memo entry stay byte-identical.
      el.innerHTML = wrapWideChars(rec.highlighted);
      el.removeAttribute('data-hl');
      found = true;
    }
    for (const root of roots) {
      for (const el of root.querySelectorAll(selector)) {
        el.innerHTML = wrapWideChars(rec.highlighted);
        el.removeAttribute('data-hl');
        found = true;
      }
    }
    if (found) deferredBlocks.delete(id);
    else rec.attempts = (rec.attempts || 0) + 1;
    if (performance.now() >= deadline && deferredBlocks.size) {
      scheduleCodeHighlightFlush();
      return;
    }
  }
  let retry = false;
  for (const [id, rec] of deferredBlocks) {
    if ((rec.attempts || 0) >= HIGHLIGHT_FLUSH_MAX_ATTEMPTS) deferredBlocks.delete(id);
    else retry = true;
  }
  if (retry) {
    highlightFlushScheduled = true;
    setTimeout(() => {
      highlightFlushScheduled = false;
      flushDeferredCodeHighlights();
    }, HIGHLIGHT_FLUSH_RETRY_MS);
  }
}

// KaTeX auto-render can only transform text around its four configured
// delimiters, and every one of them starts with '$', '\(' or '\[' — characters
// marked never synthesizes and escapeHtml never adds or removes, so a source
// carrying none of the three renders byte-identically without the walk. But
// character references decode when the browser parses the rendered HTML, so an
// entity-encoded delimiter initial also reaches the walk's text nodes: any
// numeric reference, or a named reference of the four delimiter characters
// (dollar/bsol/lpar/lparen/lsqb/lbrack), forces the walk — a false walk is the
// safe direction.
const MATH_ENTITY_RE = /&(?:#[0-9]|#[xX][0-9a-fA-F]|dollar|bsol|lparen|lpar|lsqb|lbrack)/;

function hasMathDelimiter(text) {
  return text.indexOf('$') !== -1 || text.indexOf('\\(') !== -1 || text.indexOf('\\[') !== -1
    || MATH_ENTITY_RE.test(text);
}

function renderChatMath(el, sourceText) {
  // The walk's source: the streamed paint passes the draft text; message
  // renders fall to data-raw, the message's own source. An element with
  // neither (raw backend output) keeps the unconditional walk.
  const raw = typeof sourceText === 'string' ? sourceText : el.dataset && el.dataset.raw;
  if (typeof raw === 'string' && !hasMathDelimiter(raw)) return;
  // throwOnError:false keeps stray dollar amounts ("$5 ... $10") from
  // breaking the whole bubble — invalid math renders as red inline text.
  renderMathInElement(el, {
    delimiters: [
      {left: '$$', right: '$$', display: true},
      {left: '\\[', right: '\\]', display: true},
      {left: '\\(', right: '\\)', display: false},
      {left: '$', right: '$', display: false},
    ],
    ignoredTags: ['script', 'noscript', 'style', 'textarea', 'pre', 'code', 'option'],
    ignoredClasses: ['code-block'],
    throwOnError: false,
  });
}

function renderMarkdown(btn) {
  // Try to get full content from raw text stored before marked.parse()
  const proseMsgEl = btn.closest('[data-raw]');
  let raw;
  if (proseMsgEl) {
    raw = extractMarkdownBlock(proseMsgEl.dataset.raw);
  }
  if (!raw) {
    // Fallback: use code block content (may be truncated)
    raw = btn.closest('.code-block').querySelector('pre').textContent;
  }
  const rendered = marked.parse(fixNestedFences(raw));
  const titleEl = document.getElementById('text-modal-title');
  const contentEl = document.getElementById('text-modal-content');
  const overlay = document.getElementById('text-modal-overlay');
  titleEl.textContent = 'Rendered Markdown';
  contentEl.innerHTML = rendered;
  contentEl.classList.add('prose-msg');
  overlay.style.display = 'flex';
}

function extractMarkdownBlock(rawText) {
  // Find the first ```markdown or ```md opening fence
  // Then find its matching close using greedy match (last bare fence of same length)
  var lines = rawText.split('\n');
  var start = -1;
  var startTicks = 0;

  for (var i = 0; i < lines.length; i++) {
    var m = lines[i].match(/^\x60{3,}(?:markdown|md)\s*$/);
    if (m) {
      start = i;
      startTicks = m[0].match(/^\x60+/)[0].length;
      break;
    }
  }
  if (start === -1) return null;

  // Find the LAST bare fence of same backtick length (greedy = intended close)
  var end = -1;
  var tickPattern = new RegExp('^\x60{' + startTicks + '}\\s*$');
  for (var i = lines.length - 1; i > start; i--) {
    if (tickPattern.test(lines[i])) {
      end = i;
      break;
    }
  }
  if (end === -1) return lines.slice(start + 1).join('\n');
  return lines.slice(start + 1, end).join('\n');
}

function copyCode(btn) {
  const pre = btn.closest('.code-block').querySelector('pre');
  navigator.clipboard.writeText(pre.textContent).then(() => {
    btn.textContent = 'Copied!';
    btn.classList.add('copied');
    setTimeout(() => { btn.textContent = 'Copy'; btn.classList.remove('copied'); }, 2000);
  }).catch(() => {
    btn.textContent = 'Error';
    setTimeout(() => { btn.textContent = 'Copy'; }, 2000);
  });
}
