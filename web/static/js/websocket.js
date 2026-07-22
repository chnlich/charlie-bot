// ---------------------------------------------------------------------------
// WebSocket streaming
// ---------------------------------------------------------------------------
// Wire format: the server feeds raw chat events through MessageAggregator and
// broadcasts `message` and `stream` deltas in addition to non-aggregated
// events. Rendering is fully driven by deltas; raw assistant/user events are
// no longer sent. Other raw events (master_done, task_delegated, …) still
// flow but only for state side-effects (stopThinking, spinner, etc).
let ws = null;
let reconnectDelay = 1000;
let reconnectTimer = null;
let catchupDone = false;
let pendingUserMsg = false;
let wsGeneration = 0;

function isStaleSocket(socket, targetSession, generation) {
  return socket !== ws || generation !== wsGeneration || targetSession !== SESSION_ID;
}

function disconnectWS() {
  wsGeneration++;
  const socket = ws;
  ws = null;
  if (!socket) return;
  socket.onopen = null;
  socket.onmessage = null;
  socket.onclose = null;
  socket.onerror = null;
  try { socket.close(); } catch {}
}

function connectWS() {
  if (!SESSION_ID) return;
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
  catchupDone = false;
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const targetSession = SESSION_ID;
  const generation = ++wsGeneration;
  let wsUrl = `${proto}//${location.host}/ws/sessions/${SESSION_ID}`;
  const accessKey = localStorage.getItem('charliebot_access_key');
  if (accessKey) wsUrl += '?token=' + encodeURIComponent(accessKey);
  const socket = new WebSocket(wsUrl);
  ws = socket;

  socket.onopen = () => {
    if (isStaleSocket(socket, targetSession, generation)) {
      socket.onopen = null;
      socket.onmessage = null;
      socket.onclose = null;
      socket.onerror = null;
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
  // Skip separators emitted during catchup -- the chat already shows the
  // committed history without per-turn dividers between historical bubbles.
  if (msg.role === 'separator' && !catchupDone) return;
  if (msg.role === 'user' && pendingUserMsg) {
    // The local tab already rendered this user message optimistically; skip
    // the server-side echo so the bubble doesn't double up.
    pendingUserMsg = false;
    return;
  }
  if (catchupDone) hideStreaming();
  appendMessageObject(msg);
}

function handleWSEvent(ev, socketSessionId, socketGeneration) {
  if (socketSessionId !== SESSION_ID || socketGeneration !== wsGeneration) return;
  const t = ev.type;

  if (t === 'catchup_complete') {
    catchupDone = true;
    return;
  }
  if (t === 'ping') return;

  // Session rename can arrive at any time — handle before catchup guard
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

  if (t === 'session_group_changed') {
    const searchInput = document.getElementById('sidebar-search');
    const query = searchInput ? searchInput.value.trim() : '';
    if (query) handleSidebarSearch(query);
    else switchSidebarFilter(currentFilter);
    return;
  }

  // Sidebar unread indicator — handle before catchup guard
  if (t === 'unread_changed') {
    sessionUnread[ev.session_id] = ev.has_unread;
    if (ev.session_id === SESSION_ID) return;
    const spinner = document.getElementById('spinner-' + ev.session_id);
    const spinnerVisible = spinner && !spinner.classList.contains('hidden');
    const dot = document.getElementById('unread-' + ev.session_id);
    if (dot) dot.classList.toggle('hidden', !ev.has_unread || spinnerVisible);
    return;
  }

  // Sidebar spinner update — handle before catchup guard
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

  _bumpEventCursor(ev);

  if (t === 'message') {
    _commitMessage(ev.message || {});
  } else if (t === 'stream') {
    const draft = ev.message || {};
    if (catchupDone && (draft.content || draft.thinking)) showStreaming(draft);
  } else if (t === 'master_done') {
    if (!ev.still_thinking) {
      stopThinking({preserveSessionIndicator: true});
    }
  } else if (t === 'assistant_error') {
    if (catchupDone) hideStreaming();
    stopThinking();
  } else if (t === 'error') {
    if (catchupDone) hideStreaming();
  } else if (t === 'task_delegated') {
    refreshSessionStatusNow({refreshWorkers: true});
  } else if (t === 'worker_summary') {
    refreshSessionStatusNow({refreshWorkers: true});
  } else if (t === 'result') {
    updateUsageDisplay(ev);
    // Codex result events carry translated usage; refresh from the lazy usage endpoint
    // immediately so fast turns do not leave the header on stale values.
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
