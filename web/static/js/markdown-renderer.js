// ---------------------------------------------------------------------------
// Marked.js renderer: highlight.js syntax highlighting + code block headers
// ---------------------------------------------------------------------------
(function() {
  const renderer = new marked.Renderer();
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
    const displayLang = lang || 'text';
    const isMarkdown = (lang === 'markdown' || lang === 'md');
    const renderBtn = isMarkdown
      ? '<button class="copy-btn" onclick="renderMarkdown(this)">Render</button>'
      : '';
    return `<div class="code-block"><div class="code-header"><span class="code-lang">${displayLang}</span>${renderBtn}<button class="copy-btn" onclick="copyCode(this)">Copy</button></div><pre><code class="hljs">${highlighted}</code></pre></div>`;
  };
  renderer.link = function(token) {
    const title = token.title ? ` title="${token.title}"` : '';
    return `<a href="${token.href}" target="_blank" rel="noopener noreferrer"${title}>${token.text}</a>`;
  };
  marked.use({ renderer });
})();

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
  const rendered = marked.parse(raw);
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
