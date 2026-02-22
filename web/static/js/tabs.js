// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------
function switchTab(tab) {
  const allTabs = ['chat-tex', 'chat', 'workers'];
  // Both chat-tex and chat show the chat content, workers shows workers content
  const showChat = (tab === 'chat-tex' || tab === 'chat');
  document.getElementById('tab-chat').classList.toggle('hidden', !showChat);
  document.getElementById('tab-workers').classList.toggle('hidden', tab !== 'workers');
  // LaTeX panel visible only in chat-tex
  const panel = document.getElementById('latex-panel');
  if (panel) {
    panel.classList.toggle('hidden', tab !== 'chat-tex');
  }
  const latexHandle = document.getElementById('latex-resize-handle');
  if (latexHandle) {
    latexHandle.style.display = (tab === 'chat-tex') ? '' : 'none';
  }
  if (tab === 'chat-tex') {
    latexPanelOpen = true;
    loadLatexPdf();
    loadLatexGitInfo();
  } else {
    latexPanelOpen = false;
  }
  // Highlight active tab button
  allTabs.forEach(t => {
    const btn = document.getElementById('btn-' + t);
    if (t === tab) {
      btn.classList.add('bg-blue-600/20', 'text-blue-300');
      btn.classList.remove('text-slate-400');
    } else {
      btn.classList.remove('bg-blue-600/20', 'text-blue-300');
      btn.classList.add('text-slate-400');
    }
  });
}

// ---------------------------------------------------------------------------
// Chat input
// ---------------------------------------------------------------------------
function autoResize(el) {
  el.style.height = 'auto';
  el.style.height = Math.min(el.scrollHeight, 200) + 'px';
}
