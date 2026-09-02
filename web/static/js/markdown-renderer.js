// ---------------------------------------------------------------------------
// Fix nested code fences so marked.js doesn't close the outer fence early.
// When an outer ``` fence contains inner ``` fences, upgrade the outer
// delimiter to use more backticks/tildes than any nested fence.
// ---------------------------------------------------------------------------
function fixNestedFences(md) {
  var lines = md.split('\n');
  var stack = [];
  var upgrades = {};  // lineIndex -> newLen
  var fenceRe = /^( {0,3})(`{3,}|~{3,})(.*)/;

  for (var i = 0; i < lines.length; i++) {
    var m = lines[i].match(fenceRe);
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

  // Apply upgrades in reverse line order
  var upgradeLines = Object.keys(upgrades).map(Number).sort(function(a, b) { return b - a; });
  for (var j = 0; j < upgradeLines.length; j++) {
    var lineIdx = upgradeLines[j];
    var newLen = upgrades[lineIdx];
    var line = lines[lineIdx];
    var oldMatch = line.match(fenceRe);
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

// ---------------------------------------------------------------------------
// Marked.js renderer: highlight.js syntax highlighting + code block headers
// ---------------------------------------------------------------------------
(function() {
  // This file loads before chat/shared.js (index.html:472 vs :482), so escapeHtml
  // is unavailable here. Local text/attribute escapers keep message-text tags and
  // attributes from ever becoming DOM nodes (invariant: rendered chat message body
  // contains no tag and no attribute that originated from the message text itself).
  function escapeText(s) {
    return String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  }
  function escapeAttr(s) {
    return escapeText(s).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
  }

  const renderer = new marked.Renderer();
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
    let highlighted;
    if (lang && hljs.getLanguage(lang)) {
      highlighted = hljs.highlight(trimmed, { language: lang }).value;
    } else {
      highlighted = hljs.highlightAuto(trimmed).value;
    }
    const displayLang = escapeText(lang || 'text');
    const isMarkdown = (lang === 'markdown' || lang === 'md');
    const renderBtn = isMarkdown
      ? '<button class="copy-btn" onclick="renderMarkdown(this)">Render</button>'
      : '';
    return `<div class="code-block"><div class="code-header"><span class="code-lang">${displayLang}</span>${renderBtn}<button class="copy-btn" onclick="copyCode(this)">Copy</button></div><pre><code class="hljs">${highlighted}</code></pre></div>`;
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
    }
  }});
})();

function renderChatMath(el) {
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

function toggleMobileSidebar() {
  const sidebar = document.getElementById('sidebar');
  const overlay = document.getElementById('sidebar-overlay');
  const isOpen = sidebar.classList.contains('open');
  if (isOpen) {
    sidebar.classList.remove('open');
    overlay.classList.remove('active');
  } else {
    sidebar.classList.add('open');
    overlay.classList.add('active');
  }
}

// Close sidebar on navigation (mobile)
document.querySelectorAll('#sidebar a[href]').forEach(function(a) {
  a.addEventListener('click', function() {
    if (platform.isMobile) {
      const sidebar = document.getElementById('sidebar');
      const overlay = document.getElementById('sidebar-overlay');
      sidebar.classList.remove('open');
      overlay.classList.remove('active');
    }
  });
});

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
