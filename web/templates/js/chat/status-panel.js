// ---------------------------------------------------------------------------
// Session status panel: the loader the document carries in place of the module.
// ---------------------------------------------------------------------------
// The panel holds a web session's own status, its history's web_status messages
// and its descendant rows. The page shows it only while the Status view is
// selected, and the status itself puts the waiting count on the view toggle.
// The panel's code stays at web/static/js/chat/status-panel.js, which the static
// route serves and the page tests read. This file is what the document carries:
// the loader's search path reads web/templates before web/static, and the page
// includes the same name.
//
// A web session's open starts the download only when its status has a waiting
// item to count on the toggle; any other session downloads nothing until the
// reader selects the Status view. The open the page asked for is kept here,
// with any web_status message or descendant refresh that lands in the meantime,
// and the module replays them when it runs.
(function() {
  const Chat = globalThis.Chat;
  const SOURCE = '/static/js/chat/status-panel.js';

  let loading = null;
  // The session open the page asked for while the module travelled. A null open
  // means no panel is open, so nothing is kept.
  let opened = null;
  // The newest status the open or a live message carried, and the facts that
  // arrived after the open. The module replays them after the open.
  let newestStatus = null;
  let pendingMessages = [];
  let refreshPending = false;

  // A failed download drops the memo, so the next need retries. The page keeps
  // its message view, and the panel stays empty.
  function load() {
    if (loading === null) {
      loading = loadPageScripts('chat-status-panel', [SOURCE]);
      loading.then(() => {
        const open = opened;
        if (open) {
          globalThis.openSessionStatusPanel(open.session, open.messages);
          if (pendingMessages.length) globalThis.observeWebStatusMessages(pendingMessages);
          pendingMessages = [];
          if (refreshPending) globalThis.refreshSessionStatusPanel();
          refreshPending = false;
        }
      }, (error) => {
        loading = null;
        console.error('the status panel did not load: ' + error.message);
      });
    }
    return loading;
  }

  // The newest status wins, as the panel's own absorb rule reads it: a message
  // older than the status the page holds never moves either surface back.
  function absorb(messages) {
    for (const msg of messages) {
      if (msg.role !== 'web_status') continue;
      const next = msg.web_status;
      if (newestStatus && Date.parse(next.updated_at) < Date.parse(newestStatus.updated_at)) continue;
      newestStatus = next;
    }
  }

  // The status has an item to count on the toggle, so its badge belongs on the
  // first screen and the module that paints it starts now. The module's badge
  // counts the items that are not the reader's own act, so a status of those
  // alone shows no badge and asks for nothing.
  function needsBadge(status) {
    return !!(status && Array.isArray(status.waiting)
      && status.waiting.some((item) => item.tier !== 'user-act'));
  }

  // The session remembers the Status view from an earlier open: the panel is
  // already visible, so its body belongs on this open.
  function statusViewShown() {
    const panel = document.getElementById('status-panel');
    return !!panel && !panel.classList.contains('hidden');
  }

  // The session open: only a web session carries a panel or a badge, and either
  // one of them is what starts the download. The module's own definition
  // replaces this one when it runs, so the calls after that reach the module.
  function openSessionStatusPanel(session, messages) {
    opened = null;
    newestStatus = null;
    pendingMessages = [];
    refreshPending = false;
    if (!Chat.ForYou.isWebSession(session)) return;
    opened = {session, messages};
    newestStatus = session.web_status || null;
    absorb(messages);
    if (needsBadge(newestStatus) || statusViewShown()) load();
  }

  function clearSessionStatusPanel() {
    opened = null;
    newestStatus = null;
    pendingMessages = [];
    refreshPending = false;
  }

  // A live web_status message: the panel absorbs it once the module lands, and
  // a status that adds a waiting item starts the module for its badge. The page
  // hands every committed message here, so only the web_status ones are kept and
  // a session that never opens the panel buffers no chat history.
  function observeWebStatusMessages(messages) {
    if (!opened) return;
    const statusMessages = messages.filter((msg) => msg.role === 'web_status');
    if (!statusMessages.length) return;
    pendingMessages.push(...statusMessages);
    absorb(statusMessages);
    if (needsBadge(newestStatus)) load();
  }

  // A descendant session moved its status: the panel re-reads the child rows
  // once, when the module lands.
  function refreshSessionStatusPanel() {
    if (opened) refreshPending = true;
  }

  // The Status view was selected: the panel's body is now due. The module's own
  // definitions keep this name, so the call stays safe after it lands.
  function loadStatusPanel() {
    if (opened) load();
  }

  Chat.wire({openSessionStatusPanel, clearSessionStatusPanel, observeWebStatusMessages, refreshSessionStatusPanel},
    {loadStatusPanel});
})();
