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
    document.dispatchEvent(new CustomEvent('charliebot:client-send-echo', {detail: {request_id: msg.request_id}}));
    pendingUserRequestIds.delete(msg.request_id);
    if (engine) {
      engine.rekeyMessage(
          {role: msg.role, id: msg.request_id, content: msg.content},
          msg);
    }
    return;
  }
  if (engine && engine.hasMessage(msg)) return;
  // A committed draft the page streamed arrives without its text, thinking or tool rows: they are what the
  // page already holds. An assistant or plan message closes the draft, and the server starts the next draft's
  // positions at zero, so the page drops the text it held: a next draft that opens like the closed one would
  // otherwise match that text and lose its own.
  if (msg.content_streamed) msg = withStreamedDraft(msg);
  if (msg.role === 'assistant' || msg.role === 'plan') resetStreamDraft();
  // A committed bubble supersedes the streaming preview: the draft it carries is
  // the same text the preview holds. The local tab did not produce the message;
  // it arrived over the wire, so it never forces the follow. A reader pinned at
  // the bottom follows through their pin intent, and one reading history keeps
  // their place.
  hideStreaming();
  appendMessageObject(msg, SESSION_ID, false);
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
  refreshDisplayedSessionName(sid, ev.name);
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
registerWsMessageHandler('session_group_changed', () => deferWhileHidden('session-list', refetchSessionList),
    {advancesCursor: false});
// A worker run's finish reaches the page as a task_tree_changed frame, and the concurrency badge counts that run, so
// the badge refetches with the rows. Its 5 s poll covers a worker run's start, which broadcasts nothing. The frame
// names the session whose row changed: the Workspace fetches that row alone, and the views that keep their own list
// refetch it. A hidden tab holds each request until it is visible.
registerWsMessageHandler('task_tree_changed', (ev) => {
  if (workspaceRowsShown()) queueRowRefresh(ev.session_id);
  else deferWhileHidden('session-list', refetchSessionList);
  deferWhileHidden('workspace-badges', refreshWorkspaceBadges);
  globalThis.ConcurrencyBadge?.refreshSoon();
}, {advancesCursor: false});

// The socket does not replay task_tree_changed frames, so an open that follows a close reloads the list, which
// recovers the rows the gap hid. A switch closes and reopens the page's socket, and a welcome view has no socket at
// all, so those opens sync the list too. A page load's own first socket needs no reload: the page carries the
// server's snapshot in its rows.
registerWsConnectHandler(() => {
  // The network-loss flag feeds the connect handlers that repair their own state (the plan panel); the list resync
  // below covers every open that could have missed a frame.
  networkLossPending = false;
  if (!sessionListResyncPending) return;
  sessionListResyncPending = false;
  deferWhileHidden('session-list', refetchSessionList);
});

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
  // A manager turn starts or ends: the concurrency badge's number moved with it.
  globalThis.ConcurrencyBadge?.refreshSoon();
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

// A subagent event that arrives after its draft committed sends that message again, under its id. The engine
// replaces the message where it stands. An id the list does not hold throws, so the error reaches the console.
// The frame carries the committed message's own event index, not a new one, so the cursor stays where it is.
function _replaceCommittedMessage(msg) {
  const engine = Chat.activeTurnEngineFor(document.getElementById('messages'));
  if (!engine) throw new Error('message_update arrived with no message list to update');
  engine.replaceMessage(msg);
}
registerWsMessageHandler('message', (ev) => _commitMessage(ev.message || {}), {advancesCursor: true});
registerWsMessageHandler('message_update', (ev) => _replaceCommittedMessage(ev.message), {advancesCursor: false});

// A stream frame carries only the text added since the previous frame; stream-draft.js accumulates it. The
// catch-up sends one `reset` frame holding the whole draft, so a page that joins or reconnects mid-reply
// renders the same draft as one that saw every frame.
function withStreamedDraft(msg) {
  const draft = takeStreamedDraft();
  const committed = Object.assign({}, msg, {content: draft.content, thinking: draft.thinking});
  if (draft.tools) committed.tools = draft.tools;
  return committed;
}

registerWsMessageHandler('stream', (ev) => {
  const applied = applyStreamFrame(ev);
  const draft = applied.draft;
  if (applied.changed && (draft.content || draft.thinking)) {
    showStreaming({content: draft.content, thinking: draft.thinking, tools: draft.tools});
  }
}, {advancesCursor: true});
registerWsMessageHandler('master_done', (ev) => {
  if (!ev.still_thinking) {
    stopThinking({preserveSessionIndicator: true});
  }
}, {advancesCursor: true});
registerWsMessageHandler('assistant_error', () => {
  hideStreaming();
  stopThinking();
}, {advancesCursor: true});
registerWsMessageHandler('error', () => hideStreaming(), {advancesCursor: true});
registerWsMessageHandler('task_delegated', () => refreshSessionStatusNow(), {advancesCursor: true});
registerWsMessageHandler('worker_summary', () => refreshSessionStatusNow(), {advancesCursor: true});
// The usage header is a projection over the full event list, so the forced
// poll is what updates it — the WebSocket handler must not write the header.
registerWsMessageHandler('result', () => pollActiveSessionView({force: true}), {advancesCursor: true});
