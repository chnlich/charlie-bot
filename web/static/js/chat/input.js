
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
// The pending-send store: the texts this browser has not proved the server holds
// ---------------------------------------------------------------------------
// A send records its text and request id under charliebot-pending-send-<session> before the composer
// clears, and its entry leaves the list on the first proof the server holds the message: the POST
// response, the socket echo of its request id, or a loaded history that carries the id. Until then
// the record is the only copy the browser keeps — the composer and the bubble live in the tab, so a
// reload, a closed tab or a crash in between loses the text. When a session renders
// (charliebot:session-rendered), the entries the loaded history holds are dropped, the entries this
// page instance recorded stay (its POST is still carrying them; a switch away and back must not put
// one back), and the rest go into the composer in click order, ahead of the draft, and become the
// draft. Attachments of an unsent message are not stored: the record is text only.
const PENDING_SEND_KEY_PREFIX = 'charliebot-pending-send-';

// The request ids this page instance recorded. A page instance never puts its own sends back into the
// composer: while the POST is unresolved the text is still leaving, and one refused 4xx shows its bubble.
const pendingSendsRecordedHere = new Set();

function pendingSendKey(sessionId) {
  return PENDING_SEND_KEY_PREFIX + sessionId;
}

function readPendingSends(sessionId) {
  const key = pendingSendKey(sessionId);
  const raw = localStorage.getItem(key);
  if (raw === null) return [];
  let list;
  try {
    list = JSON.parse(raw);
  } catch (error) {
    console.error('Pending-send store ' + key + ' is malformed, treated as empty: ' + error);
    return [];
  }
  const wellFormed = Array.isArray(list) &&
    list.every((entry) => entry && typeof entry.request_id === 'string' && typeof entry.content === 'string');
  if (!wellFormed) {
    console.error('Pending-send store ' + key + ' is malformed, treated as empty: expected a list of {request_id, content}');
    return [];
  }
  return list;
}

function writePendingSends(sessionId, entries) {
  const key = pendingSendKey(sessionId);
  if (entries.length) localStorage.setItem(key, JSON.stringify(entries));
  else localStorage.removeItem(key);
}

function recordPendingSend(sessionId, requestId, content) {
  const entries = readPendingSends(sessionId);
  entries.push({request_id: requestId, content});
  writePendingSends(sessionId, entries);
  pendingSendsRecordedHere.add(requestId);
}

function removePendingSend(sessionId, requestId) {
  const entries = readPendingSends(sessionId);
  const remaining = entries.filter((entry) => entry.request_id !== requestId);
  if (remaining.length !== entries.length) writePendingSends(sessionId, remaining);
}

// A session render's reconciliation: an entry the loaded history carries is proof the server took it,
// an entry this page instance recorded is still in flight in this tab, and the rest return to the
// composer in click order, ahead of the draft, and become the draft.
function reconcilePendingSends(sessionId, messages) {
  const entries = readPendingSends(sessionId);
  if (!entries.length) return;
  const historyIds = new Set(messages.filter((msg) => msg.request_id).map((msg) => msg.request_id));
  const kept = entries.filter((entry) => !historyIds.has(entry.request_id));
  const returned = kept.filter((entry) => !pendingSendsRecordedHere.has(entry.request_id));
  if (returned.length) {
    const input = document.getElementById('msg-input');
    const draft = input.value || (DRAFT_KEY ? localStorage.getItem(DRAFT_KEY) : null) || '';
    input.value = returned.map((entry) => entry.content).join('\n') + (draft ? '\n' + draft : '');
    autoResize(input);
    saveDraftNow();
  }
  writePendingSends(sessionId, kept.filter((entry) => pendingSendsRecordedHere.has(entry.request_id)));
}

// The socket echo of a send's request id (websocket-handlers.js) is the first proof of receipt: it ends
// the bubble's Sending… before the POST response does and clears the store entry.
document.addEventListener('charliebot:client-send-echo', (event) => {
  setMessageDeliveryStatus({request_id: event.detail.request_id}, 'Sent');
  removePendingSend(SESSION_ID, event.detail.request_id);
});

// A session render is where the loaded history exists beside the composer: the reconciliation runs here
// (sidebar/session-view.js announces it).
document.addEventListener('charliebot:session-rendered', (event) => {
  reconcilePendingSends(event.detail.sessionId, event.detail.messages || []);
});

// Resolves true when the server accepted the message and false when it refused it with a 4xx.
// The session is the one the message was typed in: a retry after a view switch still lands there.
async function submitUserMessage(sessionId, content, extra, requestId, optimisticMessage) {
  try {
    const response = await postChatMessageWithRetry(sessionId, content, extra, requestId);
    setMessageDeliveryStatus(optimisticMessage, 'Sent');
    removePendingSend(sessionId, requestId);
    if (response.dispatch_error) appendMessage('system', response.dispatch_error);
    return true;
  } catch (error) {
    if (!(error.status >= 400 && error.status < 500)) throw error;
    // A refused send keeps its store entry: the server never took the text, so the reload puts it back.
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
  // The record lands before the composer clears: until a proof of receipt, it is the only copy the
  // browser keeps of this text (the store comment above).
  if (content) recordPendingSend(target.id, requestId, content);
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
