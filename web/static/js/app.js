// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
document.addEventListener('DOMContentLoaded', () => {
  initAuth();
  initSidebarResize();
  initLatexResize();
  initBacklogResize();
  restoreSidebarFromUrl();
  updateRelativeTimes();

  // Initial chat render uses the server-embedded minimal bootstrap so refresh
  // and SPA switches share the same renderer without a duplicate /view fetch.
  if (SESSION_ID && SESSION_BOOTSTRAP) {
    renderSessionView(SESSION_BOOTSTRAP);
    recordUnreadFact(SESSION_ID, false);
    // The same paint clears the opened row's dot and every ancestor's subtree mark.
    refreshSessionIndicator(SESSION_ID);
    // "Read" means rendered: a render throw above skips this POST.
    markSessionRead(SESSION_ID);
  }

  // Belt-and-suspenders: helper already formats these; catch anything Jinja still emits.
  postProcessRenderedMessages(document);

  // Scroll to bottom of messages (in case JS render hasn't fired yet). A turn
  // engine owns the position instead — its mount pinned the bottom or
  // restored the reading anchor, and this write would undo the anchor.
  const msgs = document.getElementById('messages');
  const mountedEngine = globalThis.Chat && Chat.TurnEngine
    ? Chat.TurnEngine.activeFor(msgs) : null;
  if (msgs && !mountedEngine) msgs.scrollTop = msgs.scrollHeight;

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
    refreshSessionStatusNow();
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
        cancelReconnect();
        reconnectDelay = 1000;
        connectWS();
      }
    }
  });

  resumeThinkingIfMidThought();
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
// Global input key handler (Enter-to-send)
// ---------------------------------------------------------------------------
function handleInputKey(e) {
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
});

// ---------------------------------------------------------------------------
// Sidebar chrome (mobile drawer: hamburger toggle + close on navigation)
// ---------------------------------------------------------------------------
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
