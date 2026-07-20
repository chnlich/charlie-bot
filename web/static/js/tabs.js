// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------
let _backlogLoaded = false;
let _plansLoaded = false;

function switchTab(tab) {
  const allTabs = ['terminal', 'chat-tex', 'chat', 'workers', 'chat-backlog', 'chat-plans'];
  // chat-tex, chat, chat-backlog, and chat-plans all show the chat content
  const showChat = (tab === 'chat-tex' || tab === 'chat' || tab === 'chat-backlog' || tab === 'chat-plans');
  document.getElementById('tab-chat').classList.toggle('hidden', !showChat);
  document.getElementById('tab-workers').classList.toggle('hidden', tab !== 'workers');
  const terminalTab = document.getElementById('tab-terminal');
  if (terminalTab) {
    terminalTab.classList.toggle('hidden', tab !== 'terminal');
  }

  // LaTeX panel visible only in chat-tex
  const latexPanel = document.getElementById('latex-panel');
  if (latexPanel) {
    latexPanel.classList.toggle('hidden', tab !== 'chat-tex');
  }
  const latexHandle = document.getElementById('latex-resize-handle');
  if (latexHandle) {
    latexHandle.style.display = (tab === 'chat-tex') ? '' : 'none';
  }

  // Backlog panel visible only in chat-backlog
  const backlogPanelEl = document.getElementById('backlog-panel');
  if (backlogPanelEl) {
    backlogPanelEl.classList.toggle('hidden', tab !== 'chat-backlog');
  }
  const backlogHandle = document.getElementById('backlog-resize-handle');
  if (backlogHandle) {
    backlogHandle.style.display = (tab === 'chat-backlog') ? '' : 'none';
  }

  // Plan panel visible only in chat-plans
  const planPanelEl = document.getElementById('plan-panel');
  if (planPanelEl) {
    planPanelEl.classList.toggle('hidden', tab !== 'chat-plans');
  }
  const planHandle = document.getElementById('plan-resize-handle');
  if (planHandle) {
    planHandle.style.display = (tab === 'chat-plans') ? '' : 'none';
  }

  // Mobile: backlog, plan, and latex take full screen, hide chat area
  const isMobile = platform.isMobile;
  if (isMobile) {
    const chatEl = document.getElementById('tab-chat');
    if (tab === 'chat-backlog' || tab === 'chat-plans' || tab === 'chat-tex') {
      chatEl.classList.add('hidden');
    } else if (showChat) {
      chatEl.classList.remove('hidden');
    }
  }

  if (tab === 'chat-tex') {
    latexPanelOpen = true;
    loadLatexPdf();
    loadLatexGitInfo();
  } else {
    latexPanelOpen = false;
  }

  if (tab === 'chat-backlog' && !_backlogLoaded) {
    _backlogLoaded = true;
    backlogPanel.refresh();
  }

  if (tab === 'chat-plans' && !_plansLoaded) {
    _plansLoaded = true;
    planPanel.refresh();
  }

  if (tab === 'workers' && typeof ensureWorkersLoadedForActiveSession === 'function') {
    ensureWorkersLoadedForActiveSession();
  }

  if (globalThis.TerminalPanel) {
    if (tab === 'terminal') globalThis.TerminalPanel.show();
    else globalThis.TerminalPanel.hide();
  }

  // Highlight active tab button
  allTabs.forEach(t => {
    const btn = document.getElementById('btn-' + t);
    if (!btn) return;
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
  el.style.height = Math.min(Math.max(el.scrollHeight, 38), 200) + 'px';
}
