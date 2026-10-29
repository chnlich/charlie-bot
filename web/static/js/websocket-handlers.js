// ---------------------------------------------------------------------------
// Core's WebSocket message handlers
// ---------------------------------------------------------------------------
// The handlers of the event types core consumes: the chat stream and its
// committed messages, the thinking indicator, and the sidebar's names, groups,
// unread marks and activity. websocket.js dispatches each event of this
// session to the handler registered for its type. This file loads after the
// chat and sidebar files whose functions the handlers call.

function _commitMessage(msg) {
  const engine = Chat.activeTurnEngineFor(document.getElementById('messages'));
  if (msg.role === 'user' && pendingUserRequestIds.has(msg.request_id)) {
    // The matching local send already rendered this message optimistically.
    pendingUserRequestIds.delete(msg.request_id);
    if (engine) {
      engine.rekeyMessage(
          {role: msg.role, id: msg.request_id, content: msg.content},
          msg);
    }
    return;
  }
  if (engine && engine.hasMessage(msg)) return;
  // A committed bubble supersedes the streaming preview: the draft it carries is
  // the same text the preview holds. The local tab did not produce the message;
  // it arrived over the wire, so it never forces the follow. A reader pinned at
  // the bottom follows through their pin intent, and one reading history keeps
  // their place.
  hideStreaming();
  appendMessageObject(msg, SESSION_ID, false);
  // While the Status view covers the list, a message the returning view will
  // show counts for its jump row (chat/status-panel.js).
  noteMessageForStatusView(msg);
  observeWebStatusMessages([msg]);
}

// Session rename can arrive at any time
registerWsMessageHandler('session_renamed', (ev) => {
  const sid = ev.session_id || SESSION_ID;
  if (typeof updateSidebarSessionName === 'function') {
    updateSidebarSessionName(sid, ev.name);
  } else {
    const link = document.getElementById('session-' + sid);
    if (link) link.querySelector('.session-name').textContent = ev.name;
  }
  if (sid === SESSION_ID) {
    const header = document.getElementById('header-session-name');
    if (header) header.textContent = ev.name;
  }
}, {advancesCursor: false});

// A changed task node re-fetches the list the same way a group change does.
// The archived view refreshes its loaded rows in place instead, so the
// broadcast never clears the list.
function refetchSessionList() {
  const searchInput = document.getElementById('sidebar-search');
  const query = searchInput ? searchInput.value.trim() : '';
  if (query) handleSidebarSearch(query);
  else if (currentFilter === 'archived') refreshArchivedView();
  else switchSidebarFilter(currentFilter);
}
registerWsMessageHandler('session_group_changed', refetchSessionList, {advancesCursor: false});
registerWsMessageHandler('task_tree_changed', refetchSessionList, {advancesCursor: false});

// A descendant session set its status: the status panel re-reads its child rows.
registerWsMessageHandler('web_status_subtree', () => refreshSessionStatusPanel(), {advancesCursor: false});

// Sidebar unread indicator. Broadcasts are socket-ordered and always apply
// (they stamp the newest fact, so any in-flight poll or tree-page reply
// captured earlier is refused by the shared gate). The unread marks read the
// facts only, for both row kinds: the dot shows beside any activity icon,
// and the flag survives every paint.
registerWsMessageHandler('unread_changed', (ev) => {
  recordUnreadFact(ev.session_id, ev.has_unread);
  if (ev.session_id === SESSION_ID) return;
  // The row and its collapsed ancestors repaint from the shared facts.
  refreshSessionIndicator(ev.session_id);
}, {advancesCursor: false});

// Sidebar activity update. The status poll and the list paint apply the same
// state mapping. A task-tree row's work_state verdict rides the event when
// the broadcaster knows it. The 3 s poll corrects any drift.
// status.js's applyRunningChanged owns the repaint decision: a worker row
// going idle defers to the poll started below, everything else repaints at
// once.
registerWsMessageHandler('running_changed', (ev) => {
  applyRunningChanged(ev);
  if (ev.session_id === SESSION_ID) {
    if (ev.thinking_since) {
      startThinking({keepSendEnabled: true});
    } else if (!ev.has_running_tasks) {
      stopThinking({preserveSessionIndicator: true});
    }
  }
  refreshSessionStatusNow();
}, {advancesCursor: false});

// A backend switch on the active session: the header badge/dropdown must
// reflect the new value immediately. The initiating tab already did this
// through its own fetch; other tabs reach it through the shared
// session/sidebar channels. `to` is the audit shape (session channel, always
// this session). `backend`+`session_id` is the sidebar broadcast shape, which
// every open tab receives regardless of which session it's viewing. Only
// apply it when it names this tab's own session.
registerWsMessageHandler('backend_switched', (ev) => {
  const sid = ev.session_id || SESSION_ID;
  const newBackend = ev.to || ev.backend;
  if (sid === SESSION_ID && newBackend) {
    setActiveBackendId(newBackend);
    updateActiveBackendBadges();
  }
}, {advancesCursor: false});

registerWsMessageHandler('message', (ev) => _commitMessage(ev.message || {}));
registerWsMessageHandler('stream', (ev) => {
  const draft = ev.message || {};
  if (draft.content || draft.thinking) showStreaming(draft);
});
registerWsMessageHandler('master_done', (ev) => {
  if (!ev.still_thinking) {
    stopThinking({preserveSessionIndicator: true});
  }
});
registerWsMessageHandler('assistant_error', () => {
  hideStreaming();
  stopThinking();
});
registerWsMessageHandler('error', () => hideStreaming());
registerWsMessageHandler('task_delegated', () => refreshSessionStatusNow());
registerWsMessageHandler('worker_summary', () => refreshSessionStatusNow());
// The usage header is a projection over the full event list, so the forced
// poll is what updates it — the WebSocket handler must not write the header.
registerWsMessageHandler('result', () => pollActiveSessionView({force: true}));
