
(function() {
  const Chat = globalThis.Chat;

  // The composer's input mode: a package that fills the composer by other means
  // names its mode through setInputMode (null clears it). The next send carries
  // it as input_mode, and a send or an emptied composer clears it.
  let composerInputMode = null;

  function setInputMode(mode) {
    composerInputMode = mode;
  }

  const _msgInput = document.getElementById('msg-input');
  if (_msgInput) {
    _msgInput.addEventListener('input', () => {
      if (!_msgInput.value) composerInputMode = null;
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

// The send-timing telemetry's page events: the click, every retry, the POST's
// outcome and (in websocket-handlers.js) the socket echo of the same request
// id. The diag package assembles them into one client-event POST per send.
function reportSendStep(step, requestId, detail) {
  document.dispatchEvent(
      new CustomEvent('charliebot:client-send-' + step, {detail: Object.assign({request_id: requestId}, detail)}));
}

async function postChatMessageWithRetry(sessionId, content, extra, requestId) {
  let retryDelay = 1000;
  while (true) {
    let response;
    try {
      response = await postChatMessage(sessionId, content, extra, requestId);
    } catch (_error) {
      reportSendStep('retry', requestId);
      await waitForSendRetry(retryDelay);
      retryDelay = Math.min(retryDelay * 2, 30000);
      continue;
    }
    if (response.status >= 500) {
      reportSendStep('retry', requestId);
      await waitForSendRetry(retryDelay);
      retryDelay = Math.min(retryDelay * 2, 30000);
      continue;
    }
    if (response.status >= 400 && response.status < 500) {
      reportSendStep('response', requestId, {ok: false});
      const error = new Error(String(response.status));
      error.status = response.status;
      throw error;
    }
    if (!response.ok) {
      reportSendStep('response', requestId, {ok: false});
      throw new Error(String(response.status));
    }
    reportSendStep('response', requestId, {ok: true});
    return response.json();
  }
}

function waitForSendRetry(delay) {
  return new Promise((resolve) => setTimeout(resolve, delay));
}

// ---------------------------------------------------------------------------
// The pending send: the text and request id of a message the page sent and has not seen accepted
// ---------------------------------------------------------------------------
// A send writes its text and request id to this browser before the post, and removes them once the server
// accepts or refuses the message. A tab closed in between leaves them. When the session's catch-up completes
// (charliebot:page-current), the messages the page holds decide: one that carries the request id was accepted,
// and the record goes. Otherwise the text returns to the composer and the note says the last send did not go.
// The request id stays, so a send of the same text reuses it and the server answers with the message it already
// holds.
const PENDING_SEND_KEY_PREFIX = 'charliebot-pending-send-';

function pendingSendKey(sessionId) {
  return PENDING_SEND_KEY_PREFIX + sessionId;
}

function readPendingSend(sessionId) {
  const raw = localStorage.getItem(pendingSendKey(sessionId));
  return raw === null ? null : JSON.parse(raw);
}

function recordPendingSend(sessionId, requestId, content) {
  localStorage.setItem(pendingSendKey(sessionId), JSON.stringify({request_id: requestId, content}));
}

function clearPendingSend(sessionId, requestId) {
  const pending = readPendingSend(sessionId);
  if (pending === null || pending.request_id !== requestId) return;
  localStorage.removeItem(pendingSendKey(sessionId));
  setPendingSendNote(false);
}

function setPendingSendNote(on) {
  const note = document.getElementById('pending-send-note');
  if (!note) return;
  note.textContent = on ? PENDING_SEND_NOTE_LABEL : '';
  note.classList.toggle('hidden', !on);
}

function reconcilePendingSend(sessionId) {
  const pending = readPendingSend(sessionId);
  if (pending === null || pendingUserRequestIds.has(pending.request_id)) return;
  if (document.querySelector('[data-request-id="' + pending.request_id + '"]') !== null) {
    localStorage.removeItem(pendingSendKey(sessionId));
    return;
  }
  // The text goes in front of whatever the composer holds, so no typed text is overwritten. Nothing is sent.
  const input = document.getElementById('msg-input');
  input.value = input.value === '' ? pending.content : pending.content + '\n' + input.value;
  autoResize(input);
  saveDraft();
  setPendingSendNote(true);
}

document.addEventListener('charliebot:page-current', (event) => reconcilePendingSend(event.detail.sessionId));
document.addEventListener('charliebot:active-session-changed', () => setPendingSendNote(false));

// Resolves true when the server accepted the message and false when it refused it with a 4xx.
// The session is the one the message was typed in: a retry after a view switch still lands there.
async function submitUserMessage(sessionId, content, extra, requestId, optimisticMessage) {
  try {
    const response = await postChatMessageWithRetry(sessionId, content, extra, requestId);
    setMessageDeliveryStatus(optimisticMessage, 'Sent');
    clearPendingSend(sessionId, requestId);
    if (response.dispatch_error) appendMessage('system', response.dispatch_error);
    return true;
  } catch (error) {
    if (!(error.status >= 400 && error.status < 500)) throw error;
    pendingUserRequestIds.delete(requestId);
    clearPendingSend(sessionId, requestId);
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
  // A text that returned to the composer after a closed tab keeps its request id: the server then answers with
  // the message it already holds, if the first post landed.
  const returned = content ? readPendingSend(target.id) : null;
  const requestId = returned !== null && returned.content === content ? returned.request_id : crypto.randomUUID();
  // The id is in flight from the press: a catch-up that lands before the bound action runs must not put the text
  // back into the composer the user has just sent it from.
  if (content) {
    recordPendingSend(target.id, requestId, content);
    pendingUserRequestIds.add(requestId);
  }
  reportSendStep('click', requestId);

  // The send belongs to the session the page displays at press: the composer
  // and its draft are taken now, so later typing is the next message.
  input.value = '';
  input.style.height = 'auto';
  if (DRAFT_KEY) localStorage.removeItem(DRAFT_KEY);
  composerInputMode = null;

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
  reportSendStep('click', requestId);
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
};
Chat.wire(GLOBALS, CHAT_ONLY);

})();
