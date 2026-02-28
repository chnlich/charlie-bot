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
    return `<div class="code-block"><div class="code-header"><span class="code-lang">${displayLang}</span><button class="copy-btn" onclick="copyCode(this)">Copy</button></div><pre><code class="hljs">${highlighted}</code></pre></div>`;
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
