
(function() {
  const Chat = globalThis.Chat;

  // The composer's input mode: a package that fills the composer by other means
  // names its mode through setInputMode (null clears it). The next send carries
  // it as input_mode, and a send or an emptied composer clears it.
  let composerInputMode = null;

  // The note line beside the composer: a package that inserted text by other
  // means says where it came from through setInputNote. The note shares the
  // input mode's lifecycle, so clearing the mode clears and hides it too.
  function setInputNote(text) {
    const note = document.getElementById('input-note');
    if (!note) return;
    note.textContent = text;
    note.classList.remove('hidden');
  }

  // The one clear the composer input state takes: the mode and the note die
  // together, so no path drops one and leaves the other.
  function clearInputModeAndNote() {
    composerInputMode = null;
    const note = document.getElementById('input-note');
    if (note) {
      note.textContent = '';
      note.classList.add('hidden');
    }
  }

  function setInputMode(mode) {
    if (mode === null) {
      clearInputModeAndNote();
      return;
    }
    composerInputMode = mode;
  }

  const _msgInput = document.getElementById('msg-input');
  if (_msgInput) {
    _msgInput.addEventListener('input', () => {
      if (!_msgInput.value) clearInputModeAndNote();
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

function postChatMessage(content, extra, requestId) {
  return fetch(`/api/chat/${SESSION_ID}/message`, {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(Object.assign({content}, extra || {}, {request_id: requestId || crypto.randomUUID()})),
  });
}

async function postChatMessageWithRetry(content, extra, requestId) {
  let retryDelay = 1000;
  while (true) {
    let response;
    try {
      response = await postChatMessage(content, extra, requestId);
    } catch (_error) {
      await waitForSendRetry(retryDelay);
      retryDelay = Math.min(retryDelay * 2, 30000);
      continue;
    }
    if (response.status >= 500) {
      await waitForSendRetry(retryDelay);
      retryDelay = Math.min(retryDelay * 2, 30000);
      continue;
    }
    if (response.status >= 400 && response.status < 500) {
      const error = new Error(String(response.status));
      error.status = response.status;
      throw error;
    }
    if (!response.ok) throw new Error(String(response.status));
    return response.json();
  }
}

function waitForSendRetry(delay) {
  return new Promise((resolve) => setTimeout(resolve, delay));
}

async function submitUserMessage(content, extra, requestId, optimisticMessage) {
  try {
    const response = await postChatMessageWithRetry(content, extra, requestId);
    setMessageDeliveryStatus(optimisticMessage, 'Sent');
    if (response.dispatch_error) appendMessage('system', response.dispatch_error);
  } catch (error) {
    if (!(error.status >= 400 && error.status < 500)) throw error;
    pendingUserRequestIds.delete(requestId);
    setMessageDeliveryStatus(optimisticMessage, 'Failed');
    appendMessage('system', 'Failed to send message');
    stopThinking();
  }
}

async function sendMessage() {
  if (blockIfUploadsInFlight()) return;
  const input = document.getElementById('msg-input');
  const content = input.value.trim();
  const uploadedFilesForPayload = getUploadedFilesForPayload();
  if ((!content && !uploadedFilesForPayload.length) || !SESSION_ID) return;
  const payloadFiles = toPayloadFiles(uploadedFilesForPayload);
  clearSentUploadedFiles(uploadedFilesForPayload.map((file) => file.id));

  const inputMode = composerInputMode;
  const requestId = crypto.randomUUID();

  // Optimistic UI: append user message and bump session to top
  pendingUserRequestIds.add(requestId);
  const optimisticMessage = appendMessage('user', content, inputMode, new Date().toISOString(), payloadFiles, requestId);
  bumpCurrentSessionToTop();
  input.value = '';
  input.style.height = 'auto';
  if (DRAFT_KEY) localStorage.removeItem(DRAFT_KEY);
  clearInputModeAndNote();

  // Start thinking indicator; keepSendEnabled leaves the button to the
  // in-flight lock, so a master run never locks typing.
  startThinking({keepSendEnabled: true});

  await submitUserMessage(
      content, {uploaded_files: payloadFiles, input_mode: inputMode}, requestId, optimisticMessage);
}

// ---------------------------------------------------------------------------
// Manual compaction (backends whose compact facts enable it; gated by #compact-btn's disabled attribute)
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

  const requestId = crypto.randomUUID();
  pendingUserRequestIds.add(requestId);
  const optimisticMessage = appendMessage('user', '/compact', null, new Date().toISOString(), null, requestId);
  await submitUserMessage('/compact', {}, requestId, optimisticMessage);
}

const GLOBALS = {
  bumpCurrentSessionToTop,
  sendMessage,
  compactContext,
};
const CHAT_ONLY = {
  postChatMessage,
  setInputMode,
  setInputNote,
};
Chat.wire(GLOBALS, CHAT_ONLY);

})();
