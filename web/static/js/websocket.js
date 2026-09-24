// ---------------------------------------------------------------------------
// WebSocket streaming
// ---------------------------------------------------------------------------
// Wire format: the server feeds raw chat events through MessageAggregator and
// broadcasts `message` and `stream` deltas in addition to non-aggregated
// events. Rendering is fully driven by deltas; raw assistant/user/
// scheduled_trigger events are not sent. Other raw events (master_done,
// task_delegated, …) flow but only for state side-effects (stopThinking,
// spinner, etc).
let ws = null;
let reconnectDelay = 1000;
let reconnectTimer = null;
let pendingUserMsg = false;
let wsGeneration = 0;

function isStaleSocket(socket, targetSession, generation) {
  return socket !== ws || generation !== wsGeneration || targetSession !== SESSION_ID;
}

// Teardown paths detach all four handlers before close, so a closing socket
// cannot fire events into the session that replaced it. The onerror leg keeps
// its handlers attached: its onclose drives reconnect backoff.
function detachSocketHandlers(socket) {
  socket.onopen = null;
  socket.onmessage = null;
  socket.onclose = null;
  socket.onerror = null;
}

function disconnectWS() {
  wsGeneration++;
  const socket = ws;
  ws = null;
  if (!socket) return;
  detachSocketHandlers(socket);
  try { socket.close(); } catch {}
}

// Session teardown and connectWS re-entry call this so a pending backoff
// timer cannot fire connectWS into the session that was replaced.
function cancelReconnect() {
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
}

function connectWS() {
  if (!SESSION_ID) return;
  cancelReconnect();
  const targetSession = SESSION_ID;
  const generation = ++wsGeneration;
  const wsUrl = wsUrlWithToken(`/ws/sessions/${SESSION_ID}`);
  const socket = new WebSocket(wsUrl);
  ws = socket;

  socket.onopen = () => {
    if (isStaleSocket(socket, targetSession, generation)) {
      detachSocketHandlers(socket);
      try { socket.close(); } catch {}
      return;
    }
    console.log('WS connected');
    reconnectDelay = 1000;
    // Send cursor so the server only replays events beyond this index.
    socket.send(JSON.stringify({type: 'cursor', index: eventCursor}));
    // If this is a TUI session, make sure the xterm.js terminal is mounted
    // and a fresh resize is sent (server has spawned a new PTY for this WS).
    if (globalThis.TuiSession && globalThis.TuiSession.isTuiActive()) {
      globalThis.TuiSession.onWsOpenIfTui();
    }
    // Re-sync plan panel state on (re)connect.
    if (typeof planPanel !== 'undefined') planPanel.invalidate();
    if (typeof planPanel !== 'undefined') planPanel.onReconnect();
  };

  socket.onmessage = (e) => {
    if (isStaleSocket(socket, targetSession, generation)) return;
    let data;
    try { data = JSON.parse(e.data); } catch { return; }
    handleWSEvent(data, targetSession, generation);
  };

  socket.onclose = () => {
    if (isStaleSocket(socket, targetSession, generation)) return;
    console.log('WS closed, reconnecting in', reconnectDelay, 'ms');
    reconnectTimer = setTimeout(connectWS, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 30000);
  };

  socket.onerror = () => { if (!isStaleSocket(socket, targetSession, generation)) socket.close(); };
}

function _bumpEventCursor(ev) {
  // Track the highest event_index we've observed so reconnection asks the
  // server for events strictly after it. The index lives on the top level
  // for raw events and inside `message` for aggregator deltas.
  let idx = null;
  if (typeof ev.event_index === 'number') idx = ev.event_index;
  else if (ev.message && typeof ev.message.event_index === 'number') idx = ev.message.event_index;
  if (idx !== null) {
    eventCursor = Math.max(eventCursor, idx + 1);
  }
}

function _commitMessage(msg) {
  // Catchup frames are rendered exactly like live frames: the cursor the client
  // reports is the snapshot the first paint was built from, so every frame the
  // server replays is one the client does not already have. There is no
  // connection-phase state to gate rendering on.
  if (msg.role === 'user' && pendingUserMsg) {
    // The local tab already rendered this user message optimistically; skip
    // the server-side echo so the bubble doesn't double up.
    pendingUserMsg = false;
    return;
  }
  // A committed bubble supersedes the streaming preview: the draft it carries is
  // the same text the preview holds.
  hideStreaming();
  appendMessageObject(msg);
}

function handleWSEvent(ev, socketSessionId, socketGeneration) {
  if (socketSessionId !== SESSION_ID || socketGeneration !== wsGeneration) return;
  const t = ev.type;

  if (t === 'catchup_complete') {
    // Still part of the wire protocol; carries no client state any more.
    return;
  }
  if (t === 'ping') return;

  // Session rename can arrive at any time
  if (t === 'session_renamed') {
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
    return;
  }

  // Explain (btw-style) state change for one divider: repaint the button from the
  // frame; a frame carries no body, so an open panel re-fetches the single entry.
  if (t === 'explain_status') {
    updateExplainStatus(SESSION_ID, ev.upto, ev.state, ev.backend);
    return;
  }

  // A changed task node re-fetches the list the same way a group change does.
  if (t === 'session_group_changed' || t === 'task_tree_changed') {
    const searchInput = document.getElementById('sidebar-search');
    const query = searchInput ? searchInput.value.trim() : '';
    if (query) handleSidebarSearch(query);
    else switchSidebarFilter(currentFilter);
    return;
  }

  // Sidebar unread indicator. Broadcasts are socket-ordered and always apply
  // (they stamp the newest fact, so any in-flight poll or tree-page reply
  // captured earlier is refused by the shared gate). The dot is an idle-state
  // cue for both row kinds: activity (spinner or delegated gear) hides it
  // without discarding the flag.
  if (t === 'unread_changed') {
    recordUnreadFact(ev.session_id, ev.has_unread);
    if (ev.session_id === SESSION_ID) return;
    // The row and its collapsed ancestors repaint from the shared facts.
    refreshSessionIndicator(ev.session_id);
    return;
  }

  // Sidebar spinner update
  if (t === 'running_changed') {
    setSessionIndicator(ev.session_id, getSessionIndicatorState({
      thinking_since: ev.thinking_since,
      has_running_tasks: ev.has_running_tasks,
    }));
    if ('has_pending_trigger' in ev) {
      setSessionPendingTriggerIndicator(ev.session_id, ev);
    }
    if (ev.session_id === SESSION_ID) {
      if (ev.thinking_since) {
        startThinking({keepSendEnabled: true});
      } else if (!ev.has_running_tasks) {
        stopThinking({preserveSessionIndicator: true});
      }
    }
    refreshSessionStatusNow();
    return;
  }

  // A backend switch on the active session: the header badge/dropdown must
  // reflect the new value immediately (the initiating tab already did this via
  // its own fetch; other tabs reach this through the shared session/sidebar
  // channels). `to` is the audit shape (session channel, always this session);
  // `backend`+`session_id` is the sidebar broadcast shape, which every open
  // tab receives regardless of which session it's viewing — only apply it
  // when it names this tab's own session.
  if (t === 'backend_switched') {
    const sid = ev.session_id || SESSION_ID;
    const newBackend = ev.to || ev.backend;
    if (sid === SESSION_ID && newBackend) {
      setActiveBackendId(newBackend);
      updateActiveBackendBadges();
    }
    return;
  }

  _bumpEventCursor(ev);

  if (t === 'message') {
    _commitMessage(ev.message || {});
  } else if (t === 'stream') {
    const draft = ev.message || {};
    if (draft.content || draft.thinking) showStreaming(draft);
  } else if (t === 'master_done') {
    if (!ev.still_thinking) {
      stopThinking({preserveSessionIndicator: true});
    }
  } else if (t === 'assistant_error') {
    hideStreaming();
    stopThinking();
  } else if (t === 'error') {
    hideStreaming();
  } else if (t === 'task_delegated') {
    refreshSessionStatusNow({refreshWorkers: true});
  } else if (t === 'worker_summary') {
    refreshSessionStatusNow({refreshWorkers: true});
  } else if (t === 'result') {
    // The usage header is a projection over the full event list, so the forced
    // poll is what updates it — the WebSocket handler must not write the header.
    pollActiveSessionView({force: true});
  } else if (t === 'tex_edit_proposed') {
    showDiffModal();
  } else if (t === 'ext_usage') {
    renderExtUsage(ev);
  } else if (t === 'pty_output') {
    if (globalThis.TuiSession) globalThis.TuiSession.onPtyOutput(ev.data || '');
  } else if (t === 'pty_exit') {
    if (globalThis.TuiSession) globalThis.TuiSession.onPtyExit();
  } else if (t === 'plan_updated') {
    if (typeof planPanel !== 'undefined') planPanel.onPlanUpdated(ev.plan_id);
  }
}
