
(function() {
  const Chat = globalThis.Chat;

  let voiceContributed = false;

  function setVoiceContributed(value) {
    voiceContributed = !!value;
  }

  const _msgInput = document.getElementById('msg-input');
  if (_msgInput) {
    _msgInput.addEventListener('input', () => {
      if (!_msgInput.value) voiceContributed = false;
    });
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
  if (parent.firstElementChild && parent.firstElementChild !== el) {
    parent.insertBefore(el, parent.firstElementChild);
  }

  const timeEl = el.querySelector('.session-time');
  if (timeEl) {
    const now = new Date().toISOString();
    timeEl.dataset.time = now;
    timeEl.textContent = relativeTime(now);
  }
}

function postChatMessage(content, extra) {
  return fetch(`/api/chat/${SESSION_ID}/message`, {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(Object.assign({ content }, extra || {})),
  });
}

async function sendMessage() {
  if (blockIfUploadsInFlight()) return;
  const input = document.getElementById('msg-input');
  const content = input.value.trim();
  const uploadedFilesForPayload = getUploadedFilesForPayload();
  if ((!content && !uploadedFilesForPayload.length) || !SESSION_ID) return;
  const payloadFiles = toPayloadFiles(uploadedFilesForPayload);
  clearSentUploadedFiles(uploadedFilesForPayload.map((file) => file.id));

  const inputMode = voiceContributed ? 'voice' : null;

  // Optimistic UI: append user message and bump session to top
  pendingUserEchoes++;
  appendMessage('user', content, inputMode, new Date().toISOString(), payloadFiles);
  bumpCurrentSessionToTop();
  input.value = '';
  input.style.height = 'auto';
  if (DRAFT_KEY) localStorage.removeItem(DRAFT_KEY);
  voiceContributed = false;

  // Start thinking indicator; keepSendEnabled leaves the button to the
  // in-flight lock, so a master run never locks typing.
  startThinking({keepSendEnabled: true});

  try {
    const res = await postChatMessage(content, { uploaded_files: payloadFiles, input_mode: inputMode });
    if (!res.ok) throw new Error(String(res.status));
  } catch (err) {
    console.error('Send failed:', err);
    pendingUserEchoes--;
    appendMessage('system', 'Failed to send message');
    stopThinking();
  }
}

// ---------------------------------------------------------------------------
// Manual compaction (cc-claude only; gated by #compact-btn's disabled attribute)
// ---------------------------------------------------------------------------
async function compactContext() {
  if (blockIfUploadsInFlight()) return;
  const usageTextEl = document.getElementById('usage-text');
  const contextReading = usageTextEl ? usageTextEl.textContent : 'unknown';
  const confirmed = confirm(
    'Current context: ' + contextReading + '. Compacting costs one model call, priced by the ' +
    'size of the current context. Compact now?'
  );
  if (!confirmed) return;

  pendingUserEchoes++;
  appendMessage('user', '/compact', null, new Date().toISOString(), null);

  try {
    const res = await postChatMessage('/compact');
    if (!res.ok) throw new Error(String(res.status));
  } catch (err) {
    console.error('Compact failed:', err);
    pendingUserEchoes--;
    appendMessage('system', 'Failed to send message');
  }
}

const GLOBALS = {
  bumpCurrentSessionToTop,
  sendMessage,
  compactContext,
};
const CHAT_ONLY = {
  postChatMessage,
  setVoiceContributed,
};
Chat.wire(GLOBALS, CHAT_ONLY);

})();
