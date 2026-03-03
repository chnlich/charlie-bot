// ---------------------------------------------------------------------------
// Send message
// ---------------------------------------------------------------------------
function bumpCurrentSessionToTop() {
  const nav = document.getElementById('session-list');
  const el = document.getElementById('session-' + SESSION_ID);
  if (!nav || !el || nav.firstElementChild === el) return;
  nav.insertBefore(el, nav.firstElementChild);
  const timeEl = el.querySelector('.session-time');
  if (timeEl) {
    const now = new Date().toISOString();
    timeEl.dataset.time = now;
    timeEl.textContent = relativeTime(now);
  }
}

function formatBubbleTime(isoStr) {
  if (!isoStr) return '';
  const d = new Date(isoStr);
  return d.toLocaleString(undefined, {
    month: 'short', day: 'numeric',
    hour: 'numeric', minute: '2-digit', second: '2-digit', hour12: true, timeZoneName: 'short'
  });
}

async function sendMessage() {
  const input = document.getElementById('msg-input');
  const content = input.value.trim();
  if ((!content && !uploadedFiles.length) || !SESSION_ID) return;

  // Snapshot and clear uploaded files before sending
  const filePaths = uploadedFiles.map(f => f.path);
  uploadedFiles = [];
  renderFileChips();

  // Optimistic UI: append user message and bump session to top
  pendingUserMsg = true;
  const displayContent = content || '[Files attached]';
  appendMessage('user', displayContent, false, new Date().toISOString());
  bumpCurrentSessionToTop();
  input.value = '';
  input.style.height = 'auto';
  if (DRAFT_KEY) localStorage.removeItem(DRAFT_KEY);

  // Start thinking indicator
  startThinking();

  try {
    await fetch(`/api/chat/${SESSION_ID}/message`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ content, uploaded_files: filePaths }),
    });
  } catch (err) {
    console.error('Send failed:', err);
    appendMessage('system', 'Failed to send message');
    stopThinking();
  }
}

function appendMessage(role, content, isVoice, timestamp) {
  const container = document.getElementById('messages');
  const div = document.createElement('div');
  const timeHtml = timestamp ? '<div class="text-[10px] text-slate-400/60 mt-1">' + formatBubbleTime(timestamp) + '</div>' : '';

  if (role === 'user') {
    div.className = 'flex justify-end';
    div.innerHTML = `<div class="max-w-[75%] overflow-hidden bg-blue-600 rounded-2xl rounded-br-md px-4 py-2.5 text-sm">
      ${isVoice ? '<span class="text-xs text-blue-200 block mb-1">&#127908; Voice</span>' : ''}
      <div class="whitespace-pre-wrap">${escapeHtml(content)}</div>${timeHtml}</div>`;
  } else if (role === 'assistant') {
    div.className = 'flex justify-start';
    div.innerHTML = `<div class="max-w-[90%] overflow-hidden bg-slate-700 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm">
      <div class="prose-msg">${marked.parse(content)}</div>${timeHtml}</div>`;
  } else if (role === 'plan') {
    div.className = 'flex justify-start';
    div.innerHTML = `<div class="max-w-[90%] overflow-hidden bg-slate-800 border border-blue-500/30 rounded-2xl px-4 py-3 text-sm">
      <div class="flex items-center gap-2 text-blue-400 text-xs font-semibold mb-2">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4"/></svg>
        Plan
      </div>
      <div class="prose-msg">${marked.parse(content)}</div>${timeHtml}</div>`;
  } else if (role === 'task_delegated') {
    div.className = 'flex justify-start';
    div.innerHTML = `<div class="max-w-[90%] overflow-hidden bg-amber-900/30 border border-amber-700/30 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm text-slate-300">
      <div class="flex items-center gap-2 text-amber-400 text-xs font-semibold mb-2">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M13 5l7 7-7 7M5 5l7 7-7 7"/></svg>
        Delegated
      </div>
      <div class="whitespace-pre-wrap">${escapeHtml(content)}</div>${timeHtml}</div>`;
  } else {
    // System pill — show timestamp as hover tooltip
    const titleAttr = timestamp ? ' title="' + formatBubbleTime(timestamp) + '"' : '';
    div.className = 'flex justify-center';
    div.innerHTML = `<div class="bg-slate-700/50 text-slate-400 text-xs px-3 py-1.5 rounded-full max-w-[85%] overflow-hidden truncate"${titleAttr}>${escapeHtml(content)}</div>`;
  }

  // Insert before streaming-msg
  const streamEl = document.getElementById('streaming-msg');
  container.insertBefore(div, streamEl);
  container.scrollTop = container.scrollHeight;
}

function appendSeparator(seconds) {
  const container = document.getElementById('messages');
  const div = document.createElement('div');
  div.className = 'flex items-center gap-3 py-2 px-4 separator-line';
  const timeStr = seconds != null ? ' · ' + seconds + 's' : '';
  div.innerHTML = '<div class="flex-1 border-t border-slate-600/40"></div>'
    + '<span class="text-xs text-slate-500 whitespace-nowrap">response complete' + timeStr + '</span>'
    + '<div class="flex-1 border-t border-slate-600/40"></div>';
  const streamEl = document.getElementById('streaming-msg');
  container.insertBefore(div, streamEl);
  container.scrollTop = container.scrollHeight;
}

function escapeHtml(str) {
  const d = document.createElement('div');
  d.textContent = str;
  return d.innerHTML;
}
