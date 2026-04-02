// ---------------------------------------------------------------------------
// Auto-scroll helper — returns true only when user is near the bottom
// ---------------------------------------------------------------------------
function shouldAutoScroll(container, threshold = 150) {
  return container.scrollHeight - container.scrollTop - container.clientHeight < threshold;
}

// ---------------------------------------------------------------------------
// Send message
// ---------------------------------------------------------------------------
function bumpCurrentSessionToTop() {
  const nav = document.getElementById('session-list');
  const el = document.getElementById('session-' + SESSION_ID);
  if (!nav || !el) return;

  const groupItems = el.closest('.session-group-items');
  const parent = groupItems || nav;
  if (parent.firstElementChild !== el) {
    parent.insertBefore(el, parent.firstElementChild);
  }

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

function getUploadedFileDisplayName(file) {
  if (file && file.filename) return file.filename;
  const path = file && file.path ? String(file.path) : '';
  const normalized = path.replaceAll('\\', '/').replace(/\/+$/, '');
  const parts = normalized.split('/');
  return parts[parts.length - 1] || path;
}

function normalizeUploadedFiles(uploadedFiles) {
  if (!Array.isArray(uploadedFiles) || !uploadedFiles.length) return [];
  return uploadedFiles.map((file) => {
    if (typeof file === 'string') {
      return {filename: getUploadedFileDisplayName({path: file}), path: file};
    }
    return {
      filename: getUploadedFileDisplayName(file),
      path: file.path || '',
      size: file.size,
    };
  });
}

function splitLegacyUploadedFiles(content) {
  const text = typeof content === 'string' ? content : '';
  const marker = '\n\n[Attached files]\n';
  const markerIndex = text.lastIndexOf(marker);
  if (markerIndex === -1) return {content: text, uploadedFiles: []};

  const attachmentBlock = text.slice(markerIndex + marker.length);
  const lines = attachmentBlock.split('\n').filter((line) => line.length > 0);
  if (!lines.length || lines.some((line) => !line.startsWith('- ') || !line.slice(2).trim())) {
    return {content: text, uploadedFiles: []};
  }

  return {
    content: text.slice(0, markerIndex),
    uploadedFiles: lines.map((line) => {
      const path = line.slice(2).trim();
      return {filename: getUploadedFileDisplayName({path}), path};
    }),
  };
}

function normalizeUserMessage(content, uploadedFiles) {
  const normalizedFiles = normalizeUploadedFiles(uploadedFiles);
  if (normalizedFiles.length) {
    return {content: typeof content === 'string' ? content : '', uploadedFiles: normalizedFiles};
  }
  return splitLegacyUploadedFiles(content);
}

function renderUserAttachments(uploadedFiles, withTopMargin) {
  const files = normalizeUploadedFiles(uploadedFiles);
  if (!files.length) return '';
  return `<div class="message-attachments${withTopMargin ? ' mt-2' : ''}">`
    + files.map((file) => {
      const title = file.path ? ` title="${escapeHtml(file.path)}"` : '';
      return `<div class="message-attachment"${title}>`
        + '<svg class="w-3.5 h-3.5 flex-shrink-0" fill="none" stroke="currentColor" viewBox="0 0 24 24">'
        + '<path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M15.172 7l-6.586 6.586a2 2 0 102.828 2.828l6.414-6.586a4 4 0 00-5.656-5.656l-6.415 6.585a6 6 0 108.486 8.486L20.5 13"/></svg>'
        + `<span>${escapeHtml(file.filename)}</span></div>`;
    }).join('')
    + '</div>';
}

function renderUserMessageBubble(content, isVoice, timestamp, uploadedFiles) {
  const normalized = normalizeUserMessage(content, uploadedFiles);
  const timeHtml = timestamp ? '<div class="text-[10px] text-slate-400/60 mt-1">' + formatBubbleTime(timestamp) + '</div>' : '';
  const textHtml = normalized.content
    ? `<div class="whitespace-pre-wrap">${escapeHtml(normalized.content)}</div>`
    : '';
  const attachmentsHtml = renderUserAttachments(normalized.uploadedFiles, Boolean(textHtml));
  return `<div class="max-w-[75%] overflow-hidden bg-blue-600 rounded-2xl rounded-br-md px-4 py-2.5 text-sm">`
    + (isVoice ? '<span class="text-xs text-blue-200 block mb-1">&#127908; Voice</span>' : '')
    + textHtml
    + attachmentsHtml
    + timeHtml
    + '</div>';
}

async function sendMessage() {
  if (uploadsInFlight > 0) {
    showToast('Please wait for uploads to finish', true);
    return;
  }
  const input = document.getElementById('msg-input');
  const content = input.value.trim();
  const uploadedFilesForPayload = getUploadedFilesForPayload();
  if ((!content && !uploadedFilesForPayload.length) || !SESSION_ID) return;
  if (content.startsWith('/')) {
    const spaceIdx = content.indexOf(' ');
    const name = spaceIdx === -1 ? content.slice(1) : content.slice(1, spaceIdx);
    const args = spaceIdx === -1 ? '' : content.slice(spaceIdx + 1).trim();
    await executeSlashCommand(name, args, {displayText: content, uploadedFiles: uploadedFilesForPayload});
    return;
  }
  const contentWithCtx = applyWorkingContext(content);
  const payloadFiles = uploadedFilesForPayload.map((file) => ({
    filename: file.filename,
    path: file.path,
    size: file.size,
  }));
  clearSentUploadedFiles(uploadedFilesForPayload.map((file) => file.id));

  // Optimistic UI: append user message and bump session to top
  pendingUserMsg = true;
  appendMessage('user', content, false, new Date().toISOString(), payloadFiles);
  bumpCurrentSessionToTop();
  input.value = '';
  input.style.height = 'auto';
  if (DRAFT_KEY) localStorage.removeItem(DRAFT_KEY);

  // Start thinking indicator
  startThinking();

  try {
    const res = await fetch(`/api/chat/${SESSION_ID}/message`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ content: contentWithCtx, uploaded_files: payloadFiles }),
    });
    if (!res.ok) throw new Error(String(res.status));
  } catch (err) {
    console.error('Send failed:', err);
    pendingUserMsg = false;
    appendMessage('system', 'Failed to send message');
    stopThinking();
  }
}

function appendMessage(role, content, isVoice, timestamp, uploadedFiles) {
  const container = document.getElementById('messages');
  const wasAtBottom = shouldAutoScroll(container);
  const div = document.createElement('div');
  const timeHtml = timestamp ? '<div class="text-[10px] text-slate-400/60 mt-1">' + formatBubbleTime(timestamp) + '</div>' : '';

  if (role === 'user') {
    div.className = 'flex justify-end';
    div.innerHTML = renderUserMessageBubble(content, isVoice, timestamp, uploadedFiles);
  } else if (role === 'assistant') {
    div.className = 'flex justify-start';
    div.innerHTML = `<div class="max-w-[90%] overflow-hidden bg-slate-700 rounded-2xl rounded-bl-md px-4 py-2.5 text-sm">
      <div class="prose-msg">${marked.parse(fixNestedFences(content))}</div>${timeHtml}</div>`;
  } else if (role === 'plan') {
    div.className = 'flex justify-start';
    div.innerHTML = `<div class="max-w-[90%] overflow-hidden bg-slate-800 border border-blue-500/30 rounded-2xl px-4 py-3 text-sm">
      <div class="flex items-center gap-2 text-blue-400 text-xs font-semibold mb-2">
        <svg class="w-3.5 h-3.5" fill="none" stroke="currentColor" viewBox="0 0 24 24"><path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M9 5H7a2 2 0 00-2 2v12a2 2 0 002 2h10a2 2 0 002-2V7a2 2 0 00-2-2h-2M9 5a2 2 0 002 2h2a2 2 0 002-2M9 5a2 2 0 012-2h2a2 2 0 012 2m-6 9l2 2 4-4"/></svg>
        Plan
      </div>
      <div class="prose-msg">${marked.parse(fixNestedFences(content))}</div>${timeHtml}</div>`;
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
  if (role === 'user' || wasAtBottom) {
    container.scrollTop = container.scrollHeight;
  } else {
    showScrollToBottom();
  }
}

function appendSeparator(seconds) {
  const container = document.getElementById('messages');
  const wasAtBottom = shouldAutoScroll(container);
  const div = document.createElement('div');
  div.className = 'flex items-center gap-3 py-2 px-4 separator-line';
  const timeStr = seconds != null ? ' · ' + seconds + 's' : '';
  div.innerHTML = '<div class="flex-1 border-t border-slate-600/40"></div>'
    + '<span class="text-xs text-slate-500 whitespace-nowrap">response complete' + timeStr + '</span>'
    + '<div class="flex-1 border-t border-slate-600/40"></div>';
  const streamEl = document.getElementById('streaming-msg');
  container.insertBefore(div, streamEl);
  if (wasAtBottom) {
    container.scrollTop = container.scrollHeight;
  } else {
    showScrollToBottom();
  }
}

function appendCloneBanner(parentName, parentSessionId) {
  const container = document.getElementById("messages");
  const wasAtBottom = shouldAutoScroll(container);
  const div = document.createElement("div");
  div.className = "flex items-center gap-3 py-3 px-4";
  div.innerHTML = "<div class=\"flex-1 border-t border-purple-500/40\"></div>"
    + "<div class=\"flex items-center gap-2 text-purple-400 text-xs\">"
    + "<svg class=\"w-3.5 h-3.5\" fill=\"none\" stroke=\"currentColor\" viewBox=\"0 0 24 24\">"
    + "<path stroke-linecap=\"round\" stroke-linejoin=\"round\" stroke-width=\"2\" d=\"M6 3v12M6 9h6m0 0V3m0 6v6m0 0h6\"/></svg>"
    + "<span>Cloned from <a href=\"/?session=" + encodeURIComponent(parentSessionId)
    + "\" class=\"text-purple-300 hover:text-purple-200 underline\">"
    + escapeHtml(parentName) + "</a></span></div>"
    + "<div class=\"flex-1 border-t border-purple-500/40\"></div>";
  const streamEl = document.getElementById("streaming-msg");
  container.insertBefore(div, streamEl);
  if (wasAtBottom) container.scrollTop = container.scrollHeight;
  else showScrollToBottom();
}

function escapeHtml(str) {
  const d = document.createElement('div');
  d.textContent = str;
  return d.innerHTML;
}

// ---------------------------------------------------------------------------
// Scroll-to-bottom floating button
// ---------------------------------------------------------------------------
function showScrollToBottom() {
  const btn = document.getElementById('scroll-to-bottom');
  if (btn) btn.classList.remove('hidden');
}

function hideScrollToBottom() {
  const btn = document.getElementById('scroll-to-bottom');
  if (btn) btn.classList.add('hidden');
}

function scrollToBottom() {
  const container = document.getElementById('messages');
  if (container) container.scrollTop = container.scrollHeight;
  hideScrollToBottom();
}

// Hide the button when user scrolls back to bottom
document.addEventListener('DOMContentLoaded', () => {
  const container = document.getElementById('messages');
  if (container) {
    container.addEventListener('scroll', () => {
      if (shouldAutoScroll(container)) hideScrollToBottom();
    });
  }
});
