// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
document.addEventListener('DOMContentLoaded', () => {
  initAuth();
  initSidebarResize();
  initLatexResize();
  initBacklogResize();
  fetchSlashCommands();
  startTuiStatusPolling();
  restoreSidebarFromUrl();
  updateRelativeTimes();

  // Initial chat render uses the server-embedded minimal bootstrap so refresh
  // and SPA switches share the same renderer without a duplicate /view fetch.
  if (SESSION_ID && SESSION_BOOTSTRAP) {
    renderSessionView(SESSION_BOOTSTRAP);
    sessionUnread[SESSION_ID] = false;
    const unreadDot = document.getElementById('unread-' + SESSION_ID);
    if (unreadDot) unreadDot.classList.add('hidden');
  }

  // Belt-and-suspenders: helper already formats these; catch anything Jinja still emits.
  postProcessRenderedMessages(document);

  // Scroll to bottom of messages (in case JS render hasn't fired yet)
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
    startPageTimer('sidebar-status', () => {
      pollSessionStatus().then(anyRunning => {
        const desired = anyRunning ? 3000 : 10000;
        if (desired !== statusPollMs) {
          statusPollMs = desired;
          scheduleStatusPoll();
        }
      });
    }, statusPollMs);
  }
  scheduleStatusPoll();
  scheduleLazySessionDataLoad();

  // Coming back from a hidden tab: one immediate snapshot of everything the
  // paused timers would have refreshed, before their cadences restart.
  onPageResume(() => {
    refreshSessionStatusNow({refreshWorkers: true});
    fetchTuiStatus();
    pollActiveSessionView();
    updateThinkingTime();
  });

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
  if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
    e.preventDefault();
    sendMessage();
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
