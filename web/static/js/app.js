// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
document.addEventListener('DOMContentLoaded', () => {
  normalizeBackendLabels();
  initSidebarResize();
  initLatexResize();
  initBacklogResize();
  fetchSlashCommands();
  // Seed sessionUnread from server-rendered unread dots
  document.querySelectorAll('[id^="unread-"]').forEach(el => {
    sessionUnread[el.id.replace('unread-', '')] = el.dataset.hasUnread === '1';
  });
  const params = new URLSearchParams(location.search);
  const urlFilter = params.get('filter');
  const urlQuery = params.get('q');
  if (urlQuery) {
    const searchInput = document.getElementById('sidebar-search');
    if (searchInput) { searchInput.value = urlQuery; handleSidebarSearch(urlQuery); }
  } else if (['all', 'starred', 'archived', 'waiting', 'scheduled'].includes(urlFilter)) {
    switchSidebarFilter(urlFilter);
  }
  updateRelativeTimes();
  // Render markdown for server-rendered assistant messages
  document.querySelectorAll('[data-md]').forEach(el => {
    el.innerHTML = marked.parse(el.textContent);
  });
  // Render bubble timestamps (server sends raw ISO, JS formats to local TZ)
  document.querySelectorAll('.bubble-time[data-ts]').forEach(el => {
    el.textContent = formatBubbleTime(el.dataset.ts);
  });
  // Format system pill title attributes from ISO to local time
  document.querySelectorAll('#messages .rounded-full[title]').forEach(el => {
    const t = el.getAttribute('title');
    if (t && t.includes('T')) el.title = formatBubbleTime(t);
  });

  // Scroll to bottom of messages
  const msgs = document.getElementById('messages');
  if (msgs) msgs.scrollTop = msgs.scrollHeight;

  // Restore draft message from localStorage
  if (DRAFT_KEY) {
    const draft = localStorage.getItem(DRAFT_KEY);
    if (draft) {
      const inp = document.getElementById('msg-input');
      inp.value = draft;
      autoResize(inp);
    }
  }

  // LaTeX editor: track dirty state + Ctrl+S to compile
  const latexEditor = document.getElementById('latex-editor');
  if (latexEditor) {
    latexEditor.addEventListener('input', () => { latexEditorDirty = true; });
    latexEditor.addEventListener('keydown', (e) => {
      if ((e.ctrlKey || e.metaKey) && e.key === 's') {
        e.preventDefault();
        compileLatex();
      }
    });
  }

  // Re-evaluate mobile layout on platform mode change
  platform.onChange((mode) => {
    const backlogPanel = document.getElementById('backlog-panel');
    const chatEl = document.getElementById('tab-chat');
    if (!backlogPanel || backlogPanel.classList.contains('hidden')) return;
    // Backlog visible: fullscreen on mobile, side-panel on desktop
    if (mode === 'desktop') {
      chatEl.classList.remove('hidden');
    } else {
      chatEl.classList.add('hidden');
    }
  });

  // Connect WebSocket
  connectWS();

  // Poll sidebar status every 3s to correct WS drift
  statusPollInterval = setInterval(pollSessionStatus, 3000);

  // Reconnect immediately on tab becoming visible (mobile Chrome background kills WS)
  document.addEventListener('visibilitychange', () => {
    if (switching) return;
    if (document.visibilityState === 'visible') {
      const inp = document.getElementById('msg-input');
      if (inp) autoResize(inp);
      if (!ws || ws.readyState !== WebSocket.OPEN) {
        if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
        reconnectDelay = 1000;
        connectWS();
      }
    }
  });

  // Resume thinking indicator if session was mid-thought
  if (THINKING_SINCE) {
    thinkingStart = new Date(THINKING_SINCE).getTime();
    startThinking();
  }

  // SPA back/forward navigation
  window.addEventListener('popstate', () => {
    if (switching) return;
    const params = new URLSearchParams(location.search);
    const sid = params.get('session');
    if (sid && sid !== SESSION_ID) {
      switchSession(sid);
    } else if (!sid) {
      location.reload();
    }
  });

});

document.addEventListener('click', function(e) {
  const menu = document.getElementById('overflow-menu');
  const toggle = document.querySelector('.overflow-toggle');
  if (menu && toggle && !menu.contains(e.target) && !toggle.contains(e.target)) {
    menu.classList.remove('show');
  }
  // Hide slash popup on outside click
  const popup = document.getElementById('slash-popup');
  const input = document.getElementById('msg-input');
  if (popup && input && !popup.contains(e.target) && e.target !== input) {
    hideSlashPopup();
  }
});
