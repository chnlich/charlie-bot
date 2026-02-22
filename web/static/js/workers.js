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
  if (!events.length) {
    container.innerHTML = '<p class="text-xs text-slate-500">No events</p>';
    return;
  }

  const typeStyles = {
    thinking: 'text-slate-400 italic',
    file_write: 'text-green-400',
    error: 'text-red-400',
    complete: 'text-green-400 font-bold',
    tool_use: 'text-blue-400',
    tool_result: 'text-slate-400',
    assistant: 'text-slate-300',
    raw: 'text-slate-500',
  };

  container.innerHTML = events
    .filter(e => e.type !== 'ping' && e.type !== 'catchup_complete')
    .map(e => {
      const cls = typeStyles[e.type] || 'text-slate-400';
      const text = e.content || e.message || e.path || e.type;
      return `<div class="text-xs py-0.5 ${cls}"><span class="text-slate-600 mr-2">${e.type}</span>${escapeHtml(String(text).substring(0, 500))}</div>`;
    }).join('');
}
