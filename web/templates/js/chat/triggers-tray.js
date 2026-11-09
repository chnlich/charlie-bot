// ---------------------------------------------------------------------------
// Pending triggers tray: the loader the document carries in place of the module.
// ---------------------------------------------------------------------------
// The tray lists the open session's pending delayed triggers and cancels them.
// A session with none shows nothing. The module stays at
// web/static/js/chat/triggers-tray.js, which the static route serves and the
// page tests read. This file is what the document carries: the loader's search
// path reads web/templates before web/static, and the page includes the same
// name.
//
// The page asks for the tray on every session open, but only the session's
// pending count tells whether there is anything to show. The first status poll
// carries that count, and the first nonzero count starts the download. The open
// session, the bell's expand request and the poll's handoff are kept here, and
// the module replays them when it runs.
(function() {
  const Chat = globalThis.Chat;
  const SOURCE = '/static/js/chat/triggers-tray.js';

  let loading = null;
  // The session the page opened, and the bell click whose session asked for an
  // expanded tray. The bell may name a session the page switches to after the
  // click, so the two names stay apart.
  let openedSession = null;
  let expandSession = null;

  // A failed download drops the memo, so the next nonzero count or bell click
  // retries. The page keeps its message view, and the tray section stays empty.
  function load() {
    if (loading === null) {
      loading = loadPageScripts('chat-triggers-tray', [SOURCE]);
      loading.then(() => {
        const session = openedSession;
        // A bell for another session marks the tray to open expanded once the
        // page switches there, as the module's own expand does.
        if (expandSession !== null && expandSession !== session) {
          globalThis.expandPendingTriggersTray(expandSession);
        }
        if (session) {
          globalThis.renderPendingTriggersTray(session);
          if (expandSession === session) globalThis.expandPendingTriggersTray(session);
        }
        expandSession = null;
      }, (error) => {
        loading = null;
        console.error('the pending triggers tray did not load: ' + error.message);
      });
    }
    return loading;
  }

  // The session open: the tray asks for nothing until the status poll reports a
  // pending trigger. A bell's expand intent stays until the module consumes it,
  // as the module's own expand flag outlives the session open it starts.
  function renderPendingTriggersTray(sessionId) {
    openedSession = sessionId;
  }

  // The status poll's handoff: the tray follows the open session only, and a
  // nonzero pending count is its first need, so that count starts the download.
  // The module's own definition replaces this one when it runs.
  function setSessionPendingTriggerTrayStatus(sid, status) {
    if (sid !== openedSession) return;
    if ((Number(status && status.pending_trigger_count) || 0) > 0) load();
  }

  // The bell: on the open session the tray expands in place and the answer is
  // true; another session switches there first and takes false. The module's
  // expand replay keeps that answer true once the tray arrives.
  function expandPendingTriggersTray(sid) {
    expandSession = sid;
    load();
    return sid === SESSION_ID && openedSession === sid;
  }

  function togglePendingTriggersTray() {}

  function trayCancelClick() {}

  Chat.wire({
    renderPendingTriggersTray,
    setSessionPendingTriggerTrayStatus,
    togglePendingTriggersTray,
    expandPendingTriggersTray,
    trayCancelClick,
  }, {});
})();
