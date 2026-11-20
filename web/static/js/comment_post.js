// ---------------------------------------------------------------------------
// Shared comment-tray client helpers (the diff page and package pages). The request
// shape and the thrown messages are one contract: each tray catches the errors
// and surfaces `err.message` in its own toast.
// ---------------------------------------------------------------------------
// `keepalive` lets the post outlive the document that makes it; only a send flushed at page hide sets it.
async function postCommentMessage(sessionId, content, {keepalive = false} = {}) {
  const requestId = crypto.randomUUID();
  let retryDelay = 1000;
  const waitBeforeRetry = async () => {
    await new Promise((resolve) => setTimeout(resolve, retryDelay));
    retryDelay = Math.min(retryDelay * 2, 30000);
  };
  while (true) {
    let response;
    try {
      response = await fetch(`/api/chat/${encodeURIComponent(sessionId)}/message`, {
        method: 'POST',
        headers: {'Content-Type': 'application/json'},
        credentials: 'same-origin',
        keepalive,
        body: JSON.stringify({content, uploaded_files: [], request_id: requestId}),
      });
    } catch (_error) {
      await waitBeforeRetry();
      continue;
    }
    if (response.status >= 500) {
      await waitBeforeRetry();
      continue;
    }
    if (response.status === 401) throw new Error('log in to comment');
    if (!response.ok) throw new Error(`Comment post failed: HTTP ${response.status}`);
    return response.json();
  }
}

// One numbered click message: each reference keeps its own lines beneath the shared arrow markers. An entry's answer
// is one string or an array of them, and each answer gets its own arrow line.
function buildClickMessage(header, entries) {
  if (entries.length === 0) return header + '\n\n';
  const lines = [header, ''];
  for (let i = 0; i < entries.length; i++) {
    const referenceLines = entries[i].reference.split('\n');
    lines.push((i + 1) + '. ▸ ' + referenceLines[0]);
    for (let j = 1; j < referenceLines.length; j++) lines.push('   ' + referenceLines[j]);
    const answers = Array.isArray(entries[i].answer) ? entries[i].answer : [entries[i].answer];
    for (const answer of answers) {
      const answerLines = answer.split('\n');
      lines.push('   ↳ ' + answerLines[0]);
      for (let j = 1; j < answerLines.length; j++) lines.push('   ' + answerLines[j]);
    }
    if (i < entries.length - 1) lines.push('');
  }
  return lines.join('\n');
}

// The one delayed send of every click (a Status completion, a page pick): the message waits
// COMMENT_UNDO_MS for Undo, then post runs once. Leaving the page or tearing down the session view
// flushes the open window at once instead of cancelling it, so a reader who leaves does not lose the
// send. post receives `{keepalive}`, true only on a leave flush; each caller passes it to its fetch so
// the request outlives the document. Undo works only while the page stays. The handle's sent promise
// settles with post's result, or with null when cancel() wins the window; cancel() returns false once
// post has begun.
const COMMENT_UNDO_MS = 5000;

function startUndoableSend(post) {
  let state = 'undo';
  let resolveSent = null;
  let rejectSent = null;
  const sent = new Promise((resolve, reject) => {
    resolveSent = resolve;
    rejectSent = reject;
  });
  function flushOnLeave() {
    if (state !== 'undo') return;
    clearTimeout(timer);
    run(true);
  }
  function removeLeaveListeners() {
    window.removeEventListener('pagehide', flushOnLeave);
    document.removeEventListener('charliebot:session-view-teardown', flushOnLeave);
  }
  function run(keepalive) {
    state = 'sending';
    removeLeaveListeners();
    (async () => post({keepalive}))().then(resolveSent, rejectSent);
  }
  function cancel() {
    if (state !== 'undo') return false;
    state = 'cancelled';
    clearTimeout(timer);
    removeLeaveListeners();
    resolveSent(null);
    return true;
  }
  window.addEventListener('pagehide', flushOnLeave);
  document.addEventListener('charliebot:session-view-teardown', flushOnLeave);
  const timer = setTimeout(() => run(false), COMMENT_UNDO_MS);
  return {
    sent,
    cancel,
    get sending() { return state === 'sending'; },
  };
}

// One tray toast at a time: showing a new one removes the previous. Each tray
// passes its own CSS class prefix, mount (a page layer marks nodes through
// injectRoot; the diff page appends straight to body) and removal
// timeout.
function createCommentToast(prefix, mount, timeoutMs) {
  let toast = null;
  return (message, isError) => {
    if (toast) toast.remove();
    const node = document.createElement('div');
    node.className = `${prefix}-toast${isError ? ` ${prefix}-toast-error` : ''}`;
    node.textContent = message;
    mount(node);
    toast = node;
    window.setTimeout(() => {
      node.remove();
      if (toast === node) toast = null;
    }, timeoutMs);
  };
}

// Every inline comment editor shares one key contract: Escape cancels,
// Ctrl/Meta+Enter submits, and both keys consume the event.
function bindEditorKeys(textarea, onCancel, onSubmit) {
  textarea.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') {
      event.preventDefault();
      onCancel();
    } else if ((event.ctrlKey || event.metaKey) && event.key === 'Enter') {
      event.preventDefault();
      onSubmit();
    }
  });
}

// Both trays' inline edit runs the same one-shot discipline: whichever of
// cancel/save fires first wins, an edit that is empty or whitespace counts as
// a cancel, and finish() runs after either end. commit receives the raw text —
// each tray applies its own trim rule (a suggestion edit keeps its whitespace).
function swapInInlineEditor(textarea, oldNode, commit, finish) {
  let done = false;
  const cancel = () => {
    if (done) return;
    done = true;
    finish();
  };
  const save = () => {
    if (done) return;
    if (!textarea.value.trim()) {
      cancel();
      return;
    }
    done = true;
    commit(textarea.value);
    finish();
  };
  bindEditorKeys(textarea, cancel, save);
  textarea.addEventListener('blur', save);
  oldNode.parentNode.replaceChild(textarea, oldNode);
  textarea.focus();
  textarea.select();
}
