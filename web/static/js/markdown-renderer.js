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
  return `<div class="code-block"><div class="code-header"><span class="code-lang">${displayLang}</span>${renderBtn}<button class="copy-btn" onclick="copyCode(this)">Copy</button></div><pre><code class="hljs"${marker}>${innerHtml}</code></pre></div>`;
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
function parseStreamDraft(fixed) {
  if (streamPaintCodeTokens === null) return marked.parse(fixed);
  var tokens = marked.lexer(fixed);
  recordCodeTokens(tokens);
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
    const run = () => (rec.lang
      ? hljs.highlight(rec.code, { language: rec.lang }).value
      : hljs.highlightAuto(rec.code).value);
    const highlighted = cachedHighlight(rec.lang, rec.code, run);
    const settledBlock = codeBlockHtml(rec.displayLang, rec.isMarkdown, highlighted, null);
    const cached = proseParseCache.get(rec.text);
    if (cached !== undefined && cached.includes(rec.plainBlock)) {
      // The replacer function keeps the highlighted bytes literal: String's
      // replacement string would read $$/$&/$`/$' patterns out of them.
      proseParseCache.set(rec.text, cached.replace(rec.plainBlock, () => settledBlock));
    }
    const selector = `code[data-hl="${id}"]`;
    let found = false;
    for (const el of document.querySelectorAll(selector)) {
      el.innerHTML = highlighted;
      el.removeAttribute('data-hl');
      found = true;
    }
    for (const root of roots) {
      for (const el of root.querySelectorAll(selector)) {
        el.innerHTML = highlighted;
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
