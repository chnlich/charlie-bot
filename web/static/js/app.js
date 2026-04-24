// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
document.addEventListener('DOMContentLoaded', () => {
  initAuth();
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
  } else if (['all', 'starred', 'archived', 'scheduled'].includes(urlFilter)) {
    switchSidebarFilter(urlFilter);
  } else {
    switchSidebarFilter('all');
  }
  updateRelativeTimes();
  // Render markdown for server-rendered assistant messages
  document.querySelectorAll('[data-md]').forEach(el => {
    el.dataset.raw = el.textContent;
    el.innerHTML = marked.parse(fixNestedFences(el.textContent));
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

  // Init scroll-to-top pagination for tail-loaded sessions
  initScrollPagination();

  // Connect WebSocket
  connectWS();

  // Poll sidebar status to correct WS drift (adaptive: 3s when tasks running, 10s idle)
  function scheduleStatusPoll() {
    statusPollInterval = setInterval(() => {
      pollSessionStatus().then(anyRunning => {
        const desired = anyRunning ? 3000 : 10000;
        if (desired !== statusPollMs) {
          statusPollMs = desired;
          clearInterval(statusPollInterval);
          scheduleStatusPoll();
        }
      });
    }, statusPollMs);
  }
  scheduleStatusPoll();
  workersPollInterval = setInterval(pollWorkers, 3000);

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

  // Resume thinking indicator if session was mid-thought.
  // Keep send button enabled on page load — the user may want to type while
  // the master is still processing (especially auto-triggered background runs).
  if (THINKING_SINCE) {
    thinkingStart = new Date(THINKING_SINCE).getTime();
    startThinking({keepSendEnabled: true});
  }
  ensureActiveSessionViewPolling();

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

// ---------------------------------------------------------------------------
// Global input key handler (delegates to slash popup, then Enter-to-send)
// ---------------------------------------------------------------------------
function handleInputKey(e) {
  if (handleSlashPopupKey(e)) return;
  if (e.key === 'Enter' && !e.shiftKey && platform.enterSendsMessage) {
    e.preventDefault();
    if (uploadsInFlight > 0) {
      showToast('Please wait for uploads to finish', true);
      return;
    }
    const input = document.getElementById('msg-input');
    const val = input ? input.value.trim() : '';
    if (val.startsWith('/')) {
      const spaceIdx = val.indexOf(' ');
      const name = spaceIdx === -1 ? val.slice(1) : val.slice(1, spaceIdx);
      const args = spaceIdx === -1 ? '' : val.slice(spaceIdx + 1).trim();
      executeSlashCommand(name, args);
    } else {
      sendMessage();
    }
  }
}

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
