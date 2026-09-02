// ---------------------------------------------------------------------------
// Thread detail / events
// ---------------------------------------------------------------------------
const loadedThreads = new Set();
// Raw projected-event counts behind the incremental fetch: a poll ships only
// the events past what the container already renders.
const loadedEventCounts = new Map();
// Threads with a registered events poll; the intervals live in the page-timers
// registry so a hidden tab polls nothing.
const threadPollTimers = new Set();

function threadPollTimerName(threadId) {
  return 'thread-events-' + threadId;
}

async function fetchAndRenderEvents(threadId, sessionId) {
  const known = loadedEventCounts.get(threadId) || 0;
  const [eventsRes, metadataRes] = await Promise.all([
    fetch(`/api/threads/${sessionId}/threads/${threadId}/events?after=${known}`),
    fetch(`/api/threads/${sessionId}/threads/${threadId}`)
  ]);
  const payload = await eventsRes.json();
  const metadata = await metadataRes.json();
  renderAttachCommand(threadId, metadata);
  if (payload.reset || known === 0) {
    renderThreadEvents(threadId, payload.events);
  } else {
    appendThreadEvents(threadId, payload.events);
  }
  loadedEventCounts.set(threadId, payload.total);
}

function isWorkerRunning(threadId) {
  const dot = document.getElementById('thread-dot-' + threadId);
  return dot && dot.classList.contains('bg-blue-500');
}

function startThreadPoll(threadId, sessionId) {
  stopThreadPoll(threadId);
  threadPollTimers.add(threadId);
  startPageTimer(threadPollTimerName(threadId), async () => {
    const detail = document.getElementById('thread-detail-' + threadId);
    if (!detail || detail.classList.contains('hidden')) {
      stopThreadPoll(threadId);
      return;
    }
    if (!isWorkerRunning(threadId)) {
      // Worker finished — do one final fetch then stop
      try { await fetchAndRenderEvents(threadId, sessionId); } catch (e) { console.warn('Poll fetch failed:', e); }
      stopThreadPoll(threadId);
      return;
    }
    try { await fetchAndRenderEvents(threadId, sessionId); } catch (e) { console.warn('Poll fetch failed:', e); }
  }, 5000);
}

function stopThreadPoll(threadId) {
  if (!threadPollTimers.has(threadId)) return;
  threadPollTimers.delete(threadId);
  stopPageTimer(threadPollTimerName(threadId));
}

function stopAllThreadPolls() {
  for (const threadId of threadPollTimers) {
    stopPageTimer(threadPollTimerName(threadId));
  }
  threadPollTimers.clear();
}

function ensureThreadAttachContainer(threadId) {
  let container = document.getElementById('thread-attach-' + threadId);
  if (container) return container;
  const eventsContainer = document.getElementById('thread-events-' + threadId);
  if (!eventsContainer || !eventsContainer.parentElement) return null;
  container = document.createElement('div');
  container.id = 'thread-attach-' + threadId;
  container.className = 'px-4 pt-4 hidden';
  eventsContainer.parentElement.insertBefore(container, eventsContainer);
  return container;
}

function renderAttachCommand(threadId, metadata) {
  const container = ensureThreadAttachContainer(threadId);
  if (!container) return;
  const command = metadata && metadata.attach_command;
  if (!command) {
    container.classList.add('hidden');
    container.innerHTML = '';
    return;
  }
  const muted = metadata.attach_available === false;
  container.classList.remove('hidden');
  container.innerHTML = `
    <div class="mb-3">
      <div class="text-xs font-medium text-slate-400 mb-1">Attach from terminal</div>
      <div class="code-block ${muted ? 'opacity-50' : ''}">
        <div class="code-header"><span class="code-lang">terminal</span><button class="copy-btn" onclick="copyCode(this)">Copy</button></div>
        <pre><code>${escapeHtml(command)}</code></pre>
      </div>
      ${muted ? '<p class="mt-1 text-xs text-slate-500">Session is no longer live (worktree removed or tmux session ended).</p>' : ''}
    </div>`;
}

async function toggleThreadDetail(threadId, sessionId) {
  const detail = document.getElementById('thread-detail-' + threadId);
  const chevron = document.getElementById('chevron-' + threadId);
  const isHidden = detail.classList.contains('hidden');

  detail.classList.toggle('hidden');
  chevron.style.transform = isHidden ? 'rotate(90deg)' : '';

  if (isHidden) {
    // Expanding — fetch if not already loaded (prevents double-fetch on rapid clicks)
    if (!loadedThreads.has(threadId)) {
      loadedThreads.add(threadId);
      try {
        await fetchAndRenderEvents(threadId, sessionId);
      } catch (err) {
        document.getElementById('thread-events-' + threadId).innerHTML =
          '<p class="text-xs text-red-400">Failed to load events</p>';
      }
    }
    // Start auto-poll for running workers
    if (isWorkerRunning(threadId)) {
      startThreadPoll(threadId, sessionId);
    }
  } else {
    // Collapsing — stop poll and clear cache so re-expand fetches fresh data
    stopThreadPoll(threadId);
    loadedThreads.delete(threadId);
    loadedEventCounts.delete(threadId);
    finalFetchDone.delete(threadId);
  }
}

const THREAD_EVENT_SKIP_TYPES = new Set(['ping', 'catchup_complete', 'raw', 'system', 'rate_limit_event']);
const NO_EVENTS_HTML = '<p class="text-xs text-slate-500">No events</p>';

function filterThreadEvents(events) {
  return events.filter(e => {
    if (THREAD_EVENT_SKIP_TYPES.has(e.type)) return false;
    // skip user tool_result events
    if (e.type === 'user') return false;
    return true;
  });
}

function renderThreadEvents(threadId, events) {
  paintThreadEvents(threadId, document.getElementById('thread-events-' + threadId), events);
}

function appendThreadEvents(threadId, events) {
  const container = document.getElementById('thread-events-' + threadId);
  if (!container || !filterThreadEvents(events).length) return;
  if (container.dataset.empty === '1') {
    paintThreadEvents(threadId, container, events);
    return;
  }
  // Scratch paint: insertAdjacentHTML parses only the tail, never the existing list.
  const scratch = document.createElement('div');
  paintThreadEvents(threadId, scratch, events);
  container.insertAdjacentHTML('beforeend', scratch.innerHTML);
}

function paintThreadEvents(threadId, container, events) {
  // Inject refresh button toolbar above events (the append path's scratch
  // container has no parent; only the real list gets one).
  const refreshBtnId = 'refresh-btn-' + threadId;
  let toolbar = container.parentElement && container.parentElement.querySelector('.thread-events-toolbar');
  if (!toolbar && container.parentElement) {
    toolbar = document.createElement('div');
    toolbar.className = 'thread-events-toolbar flex justify-end px-3 pt-2';
    toolbar.innerHTML = `<button id="${refreshBtnId}" onclick="refreshThreadEvents('${threadId}')"
      class="p-1 rounded-full text-slate-400 hover:text-slate-200 hover:bg-slate-700 transition-colors" title="Refresh events">
      <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
        <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M4 4v5h5M20 20v-5h-5M4 9a9 9 0 0115.4-4.4M20 15a9 9 0 01-15.4 4.4"/>
      </svg>
    </button>`;
    container.parentElement.insertBefore(toolbar, container);
  }

  const filtered = filterThreadEvents(events);

  if (!filtered.length) {
    container.dataset.empty = '1';
    container.innerHTML = NO_EVENTS_HTML;
    return;
  }

  function toolSummary(e) {
    const input = e.input || {};
    if (e.tool_name === 'Bash' || e.tool_name === 'bash') {
      return {text: input.command || '', limit: 80};
    }
    if (e.tool_name === 'Edit' || e.tool_name === 'Write') {
      return {text: input.file_path || '', limit: 0};
    }
    if (e.tool_name === 'Read') {
      return {text: input.file_path || '', limit: 0};
    }
    if (e.tool_name === 'Glob') {
      return {text: input.pattern || '', limit: 0};
    }
    if (e.tool_name === 'Grep') {
      return {text: (input.pattern || '') + (input.path ? ' in ' + input.path : ''), limit: 0};
    }
    const first = Object.values(input)[0];
    if (!first) return {text: '', limit: 0};
    const display = typeof first === 'object' ? JSON.stringify(first) : String(first);
    return {text: display, limit: 60};
  }

  const parts = filtered.map(e => {
    const ts = Chat.formatBubbleTime(e.timestamp);
    const tsHtml = ts ? `<span class="text-slate-600 ml-2 text-xs">${ts}</span>` : '';

    if (e.type === 'assistant') {
      const text = String(e.content || '');
      const short = text.substring(0, 300);
      const hasMore = text.length > 300;
      const id = 'evt-more-' + Math.random().toString(36).slice(2);
      return `<div class="py-2 px-3 my-1 bg-slate-700/50 rounded-lg">
        <div class="text-sm text-slate-300">${escapeHtml(short)}${hasMore ? showMoreToggleHtml(id, escapeHtml(text.substring(300))) : ''}</div>
        ${tsHtml}
      </div>`;
    }

    if (e.type === 'tool_use') {
      const name = e.tool_name || 'tool';
      const {text, limit} = toolSummary(e);
      const hasMore = limit > 0 && text.length > limit;
      const short = hasMore ? text.substring(0, limit) : text;
      let summaryHtml;
      if (hasMore) {
        const id = 'tu-' + Math.random().toString(36).slice(2);
        summaryHtml = escapeHtml(short) + showMoreToggleHtml(id, escapeHtml(text.substring(limit)));
      } else {
        summaryHtml = escapeHtml(short);
      }
      return `<div class="py-1.5 px-3 my-0.5 flex items-center gap-2">
        ${toolNameChipHtml(name)}
        <span class="text-xs text-slate-400 ${hasMore ? '' : 'truncate '}flex-1">${summaryHtml}</span>
        ${tsHtml}
      </div>`;
    }

    if (e.type === 'tool_result') {
      const text = String(e.content || '');
      const short = text.substring(0, 500);
      const hasMore = text.length > 500;
      const id = 'tr-more-' + Math.random().toString(36).slice(2);
      return `<div class="py-1 px-3 ml-6 my-0.5 border-l-2 border-slate-700">
        <pre class="text-xs text-slate-500 whitespace-pre-wrap break-all">${escapeHtml(short)}${hasMore ? showMoreToggleHtml(id, escapeHtml(text.substring(500))) : ''}</pre>
      </div>`;
    }

    if (e.type === 'file_write') {
      const lines = e.lines_added != null ? ` +${e.lines_added}` : '';
      return `<div class="py-1.5 px-3 my-0.5 flex items-center gap-2">
        <svg class="w-3.5 h-3.5 text-green-400 flex-shrink-0" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 12h6m-6 4h6m2 5H7a2 2 0 01-2-2V5a2 2 0 012-2h5.586a1 1 0 01.707.293l5.414 5.414a1 1 0 01.293.707V19a2 2 0 01-2 2z"/></svg>
        <span class="text-xs text-green-400 truncate flex-1">${escapeHtml(e.path || '')}<span class="text-slate-500">${lines}</span></span>
        ${tsHtml}
      </div>`;
    }

    if (e.type === 'error') {
      return `<div class="py-2 px-3 my-1 bg-red-900/40 border border-red-700/50 rounded-lg">
        <span class="text-sm text-red-300">${escapeHtml(String(e.message || e.content || 'error'))}</span>
        ${tsHtml}
      </div>`;
    }

    if (e.type === 'complete') {
      const ok = e.status !== 'failed';
      const cls = ok ? 'bg-green-900/40 border-green-700/50 text-green-300' : 'bg-red-900/40 border-red-700/50 text-red-300';
      const label = ok ? 'Completed' : 'Failed';
      return `<div class="py-2 px-3 my-1 ${cls} border rounded-lg text-sm font-medium">
        ${label}${e.message ? ': ' + escapeHtml(String(e.message)) : ''}
        ${tsHtml}
      </div>`;
    }

    if (e.type === 'thinking') {
      const id = 'think-' + Math.random().toString(36).slice(2);
      return `<div class="py-1 px-3 my-0.5">
        <button onclick="const el=document.getElementById('${id}');el.style.display=el.style.display==='none'?'block':'none'" class="text-xs text-slate-600 hover:text-slate-500 italic">Thinking…</button>
        <div id="${id}" style="display:none" class="mt-1 text-xs text-slate-600 whitespace-pre-wrap">${escapeHtml(String(e.content || ''))}</div>
        ${tsHtml}
      </div>`;
    }

    // fallback
    const text = e.content || e.message || e.path || e.type;
    return `<div class="py-1.5 px-3 my-0.5 text-xs text-slate-500">
      <span class="text-slate-600 mr-2">${escapeHtml(e.type)}</span>${escapeHtml(String(text).substring(0, 300))}${tsHtml}
    </div>`;
  });

  container.dataset.empty = '';
  container.innerHTML = parts.join('');
}

async function refreshThreadEvents(threadId) {
  try {
    loadedEventCounts.delete(threadId);
    await fetchAndRenderEvents(threadId, SESSION_ID);
  } catch (err) {
    console.error('Refresh events failed:', err);
  }
}

function showTextModal(title, text) {
  document.getElementById('text-modal-title').textContent = title;
  document.getElementById('text-modal-content').textContent = text;
  document.getElementById('text-modal-overlay').style.display = 'flex';
}

function closeTextModal() {
  const contentEl = document.getElementById('text-modal-content');
  contentEl.classList.remove('prose-msg');
  contentEl.innerHTML = '';
  document.getElementById('text-modal-overlay').style.display = 'none';
}

async function cancelThread(threadId, sessionId) {
  try {
    await fetch('/api/threads/' + sessionId + '/threads/' + threadId + '/cancel', { method: 'POST' });
    updateWorkerStatus(threadId, 'cancelled');
  } catch (err) {
    console.error('Cancel failed:', err);
  }
}
