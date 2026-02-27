// ---------------------------------------------------------------------------
// Thread detail / events
// ---------------------------------------------------------------------------
const loadedThreads = new Set();

async function toggleThreadDetail(threadId, sessionId) {
  const detail = document.getElementById('thread-detail-' + threadId);
  const chevron = document.getElementById('chevron-' + threadId);
  const isHidden = detail.classList.contains('hidden');

  detail.classList.toggle('hidden');
  chevron.style.transform = isHidden ? 'rotate(90deg)' : '';

  if (isHidden && !loadedThreads.has(threadId)) {
    loadedThreads.add(threadId);
    try {
      const res = await fetch(`/api/threads/${sessionId}/threads/${threadId}/events`);
      const events = await res.json();
      renderThreadEvents(threadId, events);
    } catch (err) {
      document.getElementById('thread-events-' + threadId).innerHTML =
        '<p class="text-xs text-red-400">Failed to load events</p>';
    }
  }
}

function renderThreadEvents(threadId, events) {
  const container = document.getElementById('thread-events-' + threadId);

  const SKIP_TYPES = new Set(['ping', 'catchup_complete', 'raw', 'system', 'rate_limit_event']);
  const filtered = events.filter(e => {
    if (SKIP_TYPES.has(e.type)) return false;
    // skip user tool_result events
    if (e.type === 'user') return false;
    return true;
  });

  if (!filtered.length) {
    container.innerHTML = '<p class="text-xs text-slate-500">No events</p>';
    return;
  }

  function fmtTime(ts) {
    if (!ts) return '';
    const d = new Date(ts);
    const h = String(d.getHours()).padStart(2, '0');
    const m = String(d.getMinutes()).padStart(2, '0');
    return `${h}:${m}`;
  }

  function toolSummary(e) {
    const input = e.input || {};
    if (e.tool_name === 'Bash' || e.tool_name === 'bash') {
      const cmd = input.command || '';
      return escapeHtml(cmd.substring(0, 80));
    }
    if (e.tool_name === 'Edit' || e.tool_name === 'Write') {
      return escapeHtml(input.file_path || '');
    }
    if (e.tool_name === 'Read') {
      return escapeHtml(input.file_path || '');
    }
    if (e.tool_name === 'Glob') {
      return escapeHtml(input.pattern || '');
    }
    if (e.tool_name === 'Grep') {
      return escapeHtml((input.pattern || '') + (input.path ? ' in ' + input.path : ''));
    }
    const first = Object.values(input)[0];
    if (!first) return '';
    const display = typeof first === 'object' ? JSON.stringify(first) : String(first);
    return escapeHtml(display.substring(0, 60));
  }

  const parts = filtered.map(e => {
    const ts = fmtTime(e.timestamp);
    const tsHtml = ts ? `<span class="text-slate-600 ml-2 text-xs">${ts}</span>` : '';

    if (e.type === 'assistant') {
      const text = String(e.content || '');
      const short = text.substring(0, 300);
      const hasMore = text.length > 300;
      const id = 'evt-more-' + Math.random().toString(36).slice(2);
      return `<div class="py-2 px-3 my-1 bg-slate-700/50 rounded-lg">
        <div class="text-sm text-slate-300">${escapeHtml(short)}${hasMore ? `<span id="${id}-short">… <button onclick="document.getElementById('${id}-short').style.display='none';document.getElementById('${id}-full').style.display='inline'" class="text-blue-400 hover:underline text-xs">Show more</button></span><span id="${id}-full" style="display:none">${escapeHtml(text.substring(300))}</span>` : ''}</div>
        ${tsHtml}
      </div>`;
    }

    if (e.type === 'tool_use') {
      const name = e.tool_name || 'tool';
      const summary = toolSummary(e);
      return `<div class="py-1.5 px-3 my-0.5 flex items-center gap-2">
        <span class="px-2 py-0.5 rounded-full text-xs font-medium bg-blue-900/60 text-blue-300 border border-blue-700/50">${escapeHtml(name)}</span>
        <span class="text-xs text-slate-400 truncate flex-1">${summary}</span>
        ${tsHtml}
      </div>`;
    }

    if (e.type === 'tool_result') {
      const text = String(e.content || '');
      const short = text.substring(0, 500);
      const hasMore = text.length > 500;
      const id = 'tr-more-' + Math.random().toString(36).slice(2);
      return `<div class="py-1 px-3 ml-6 my-0.5 border-l-2 border-slate-700">
        <pre class="text-xs text-slate-500 whitespace-pre-wrap break-all">${escapeHtml(short)}${hasMore ? `<span id="${id}-short">… <button onclick="document.getElementById('${id}-short').style.display='none';document.getElementById('${id}-full').style.display='inline'" class="text-blue-400 hover:underline">Show more</button></span><span id="${id}-full" style="display:none">${escapeHtml(text.substring(500))}</span>` : ''}</pre>
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

  container.innerHTML = parts.join('');
}

function showTextModal(title, text) {
  document.getElementById('text-modal-title').textContent = title;
  document.getElementById('text-modal-content').textContent = text;
  document.getElementById('text-modal-overlay').style.display = 'flex';
}

function closeTextModal() {
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
