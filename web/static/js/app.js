// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
document.addEventListener('DOMContentLoaded', () => {
  initAuth();
  initSidebarResize();
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
  const mountedEngine = Chat.activeTurnEngineFor(msgs);
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
function closeMobileSidebar() {
  const sidebar = document.getElementById('sidebar');
  const overlay = document.getElementById('sidebar-overlay');
  sidebar.classList.remove('open');
  overlay.classList.remove('active');
}

function toggleMobileSidebar() {
  const sidebar = document.getElementById('sidebar');
  if (sidebar.classList.contains('open')) {
    closeMobileSidebar();
  } else {
    sidebar.classList.add('open');
    document.getElementById('sidebar-overlay').classList.add('active');
  }
}

// A phone's sidebar is a drawer over the chat, so a tap that opens a session
// or a tool page dismisses it. The session rows are rebuilt from a fetch after
// load, so the listener matches each link as it appears instead of binding the
// rows once at page load.
document.addEventListener('click', function(e) {
  if (platform.isMobile && e.target.closest('#sidebar a[href]')) closeMobileSidebar();
});

// A create button opens a session without a link click, and a tap on the open
// session has already closed the drawer above. Both create and switch announce
// the new active session; the drawer leaves on that signal too.
document.addEventListener('charliebot:active-session-changed', function() {
  if (platform.isMobile) closeMobileSidebar();
});
