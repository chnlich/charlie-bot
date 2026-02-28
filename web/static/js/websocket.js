// ---------------------------------------------------------------------------
// WebSocket streaming
// ---------------------------------------------------------------------------
let ws = null;
let streamBuf = '';
let reconnectDelay = 1000;
let reconnectTimer = null;
let catchupDone = false;
let pendingUserMsg = false;

function connectWS() {
  if (!SESSION_ID) return;
  if (reconnectTimer) { clearTimeout(reconnectTimer); reconnectTimer = null; }
  catchupDone = false;
  hideStreaming();
  const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
  const targetSession = SESSION_ID;
  ws = new WebSocket(`${proto}//${location.host}/ws/sessions/${SESSION_ID}`);

  ws.onopen = () => {
    if (targetSession !== SESSION_ID) { ws.onclose = null; ws.close(); ws = null; return; }
    console.log('WS connected');
    reconnectDelay = 1000;
    // Send cursor so the server only replays events beyond this index.
    ws.send(JSON.stringify({type: 'cursor', index: eventCursor}));
  };

  ws.onmessage = (e) => {
    let data;
    try { data = JSON.parse(e.data); } catch { return; }
    handleWSEvent(data);
  };

  ws.onclose = () => {
    console.log('WS closed, reconnecting in', reconnectDelay, 'ms');
    reconnectTimer = setTimeout(connectWS, reconnectDelay);
    reconnectDelay = Math.min(reconnectDelay * 2, 30000);
  };

  ws.onerror = () => { ws.close(); };
}

function handleWSEvent(ev) {
  const t = ev.type;

  if (t === 'catchup_complete') {
    catchupDone = true;
    if (streamBuf) showStreaming(streamBuf);
    return;
  }
  if (t === 'ping') return;

  // Session rename can arrive at any time — handle before catchup guard
  if (t === 'session_renamed') {
    const sid = ev.session_id || SESSION_ID;
    const link = document.getElementById('session-' + sid);
    if (link) link.querySelector('.session-name').textContent = ev.name;
    if (sid === SESSION_ID) {
      const header = document.getElementById('header-session-name');
      if (header) header.textContent = ev.name;
    }
    return;
  }

  // Sidebar unread indicator — handle before catchup guard
  if (t === 'unread_changed') {
    sessionUnread[ev.session_id] = ev.has_unread;
    const spinner = document.getElementById('spinner-' + ev.session_id);
    const spinnerVisible = spinner && !spinner.classList.contains('hidden');
    const dot = document.getElementById('unread-' + ev.session_id);
    if (dot) dot.classList.toggle('hidden', !ev.has_unread || spinnerVisible);
    return;
  }

  // Sidebar spinner update for non-current sessions — handle before catchup guard
  if (t === 'running_changed') {
    if (ev.session_id !== SESSION_ID) setSessionSpinner(ev.session_id, ev.has_running_tasks);
    return;
  }

  // Track every substantive event for the reconnection cursor
  eventCursor++;

  if (t === 'user') {
    // Skip CC-internal user events (tool results) — they have "message" but no "content"
    if (ev.message && !ev.content) return;
    // Flush pending assistant text before showing user message (matches backend)
    if (streamBuf) {
      if (catchupDone) hideStreaming();
      appendMessage('assistant', streamBuf);
      streamBuf = '';
    }
    // Only show if sent from another tab (this tab already added it optimistically)
    if (!pendingUserMsg) appendMessage('user', ev.content || '', ev.is_voice);
    pendingUserMsg = false;
  } else if (t === 'assistant') {
    const blocks = (ev.message || {}).content || [];
    for (const b of blocks) {
      if (b.type === 'tool_use' && b.name === 'ExitPlanMode' && b.input && b.input.plan) {
        if (streamBuf) { if (catchupDone) hideStreaming(); appendMessage('assistant', streamBuf); streamBuf = ''; }
        appendMessage('plan', b.input.plan);
      }
    }
    let text = '';
    for (const b of blocks) {
      if (b.type === 'text') text += b.text || '';
    }
    if (text && streamBuf) {
      if (catchupDone) hideStreaming();
      appendMessage('assistant', streamBuf);
      streamBuf = '';
    }
    streamBuf += text;
    if (catchupDone && streamBuf) showStreaming(streamBuf);
  } else if (t === 'master_done') {
    if (streamBuf) {
      if (catchupDone) hideStreaming();
      appendMessage('assistant', streamBuf);
      streamBuf = '';
    }
    if (!ev.still_thinking) {
      const elapsed = ev.thinking_seconds != null
        ? ev.thinking_seconds
        : (thinkingStart ? Math.floor((Date.now() - thinkingStart) / 1000) : null);
      stopThinking();
      if (catchupDone) appendSeparator(elapsed);
    }
  } else if (t === 'assistant_error') {
    if (catchupDone) hideStreaming();
    if (streamBuf) {
      appendMessage('assistant', streamBuf);
      streamBuf = '';
    }
    stopThinking();
    appendMessage('system', 'Error: ' + (ev.content || ''));
  } else if (t === 'task_delegated') {
    // Flush pending assistant text before system message (matches backend)
    if (streamBuf) {
      if (catchupDone) hideStreaming();
      appendMessage('assistant', streamBuf);
      streamBuf = '';
    }
    appendMessage('system', `Task delegated: ${ev.description || ''}`);
    setSessionSpinner(SESSION_ID, true);
    if (catchupDone) addWorkerCard(ev.thread_id, ev.description || '');
  } else if (t === 'worker_summary') {
    // Flush pending assistant text before worker summary (matches backend)
    if (streamBuf) {
      if (catchupDone) hideStreaming();
      appendMessage('assistant', streamBuf);
      streamBuf = '';
    }
    const wDiv = document.createElement('div');
    wDiv.className = 'flex justify-start';
    const fullContent = (ev.full_content || '').replace(/"/g, '&quot;').replace(/'/g, '&#39;');
    wDiv.innerHTML = `<div class="max-w-[90%] overflow-hidden bg-emerald-900/40 border border-emerald-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300 prose-msg cursor-pointer" data-full="${fullContent}" onclick="showTextModal('Worker Result', this.dataset.full)">${marked.parse(ev.content || '')}</div>`;
    const streamEl = document.getElementById('streaming-msg');
    document.getElementById('messages').insertBefore(wDiv, streamEl);
    document.getElementById('messages').scrollTop = document.getElementById('messages').scrollHeight;
    updateWorkerStatus(ev.thread_id, ev.status || 'completed');
    updateSpinner();
  } else if (t === 'handler_result') {
    const icon = ev.status === 'ok' ? '✓' : '✗';
    appendMessage('system', `${icon} ${ev.task}: ${ev.message || ''}`);
  } else if (t === 'result') {
    updateUsageDisplay(ev);
  } else if (t === 'tex_edit_proposed') {
    showDiffModal();
  } else if (t === 'context_compacted') {
    const trigger = ev.trigger || 'auto';
    const preTokens = ev.pre_tokens;
    let msg = 'Context compacted';
    if (trigger) msg += ' (' + trigger + ')';
    if (preTokens) msg += ' — was ' + Math.round(preTokens / 1000) + 'k tokens';
    appendMessage('system', msg);
  }
}
