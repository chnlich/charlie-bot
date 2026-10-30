
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
function postChatMessage(sessionId, content, extra, requestId) {
  return fetch(`/api/chat/${sessionId}/message`, {
    method: 'POST',
    headers: JSON_HEADERS,
    body: JSON.stringify(Object.assign({content}, extra || {}, {request_id: requestId || crypto.randomUUID()})),
  });
}

async function postChatMessageWithRetry(sessionId, content, extra, requestId) {
  let retryDelay = 1000;
  while (true) {
    let response;
    try {
      response = await postChatMessage(sessionId, content, extra, requestId);
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

// Resolves true when the server accepted the message and false when it refused it with a 4xx.
// The session is the one the message was typed in: a retry after a view switch still lands there.
async function submitUserMessage(sessionId, content, extra, requestId, optimisticMessage) {
  try {
    const response = await postChatMessageWithRetry(sessionId, content, extra, requestId);
    setMessageDeliveryStatus(optimisticMessage, 'Sent');
    if (response.dispatch_error) appendMessage('system', response.dispatch_error);
    return true;
  } catch (error) {
    if (!(error.status >= 400 && error.status < 500)) throw error;
    pendingUserRequestIds.delete(requestId);
    setMessageDeliveryStatus(optimisticMessage, 'Failed');
    appendMessage('system', 'Failed to send message');
    return false;
  }
}

async function sendMessage() {
  if (blockIfUploadsInFlight()) return;
  const input = document.getElementById('msg-input');
  const content = input.value.trim();
  const uploadedFilesForPayload = getUploadedFilesForPayload();
  if ((!content && !uploadedFilesForPayload.length)) return;
  const target = composerTargetSession();
  if (!target.id) return;
  const payloadFiles = toPayloadFiles(uploadedFilesForPayload);
  clearSentUploadedFiles(uploadedFilesForPayload.map((file) => file.id));

  const inputMode = composerInputMode;
  const requestId = crypto.randomUUID();

  // The send belongs to the session the page displays at press: the composer
  // and its draft are taken now, so later typing is the next message.
  input.value = '';
  input.style.height = 'auto';
  if (DRAFT_KEY) localStorage.removeItem(DRAFT_KEY);
  clearInputModeAndNote();

  const finish = async () => {
    // Optimistic UI: append the user message. The sidebar row moves only when the refetched list repaints.
    pendingUserRequestIds.add(requestId);
    const optimisticMessage = appendMessage('user', content, inputMode, new Date().toISOString(), payloadFiles, requestId);
    // Start thinking indicator; keepSendEnabled leaves the button to the
    // in-flight lock, so a master run never locks typing.
    startThinking({keepSendEnabled: true});

    const accepted = await submitUserMessage(
        target.id, content, {uploaded_files: payloadFiles, input_mode: inputMode}, requestId, optimisticMessage);
    // Only a send that started the thinking round above ends it on refusal; /compact never starts one.
    if (!accepted) stopThinking();
  };
  // A send pressed while its session is still loading waits for that switch
  // to land; a click that moves on posts it to the session it bound.
  return bindComposerAction(target.id, 'message', finish);
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

  const target = composerTargetSession();
  if (!target.id) return;
  const requestId = crypto.randomUUID();
  const finish = async () => {
    pendingUserRequestIds.add(requestId);
    const optimisticMessage = appendMessage('user', '/compact', null, new Date().toISOString(), null, requestId);
    await submitUserMessage(target.id, '/compact', {}, requestId, optimisticMessage);
  };
  // A /compact pressed while its session is still loading waits for that
  // switch to land; a click that moves on posts it to the session it bound.
  return bindComposerAction(target.id, 'message', finish);
}

const GLOBALS = {
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
