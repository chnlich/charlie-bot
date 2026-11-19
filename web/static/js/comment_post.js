// ---------------------------------------------------------------------------
// Shared comment-tray client helpers (the diff page and package pages). The request
// shape and the thrown messages are one contract: each tray catches the errors
// and surfaces `err.message` in its own toast.
// ---------------------------------------------------------------------------
async function postCommentMessage(sessionId, content) {
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

// The one undo window of every click-to-send message: it closes after this many
// seconds, and the message leaves only when no cancel came first.
const COMMENT_UNDO_SECONDS = 5;

// Starts the undo window for one message. onTick receives the seconds left, once
// a second from COMMENT_UNDO_SECONDS down to 0; 0 comes right before the post. The
// returned handle's sent promise settles with postCommentMessage's result, or with
// null when cancel() wins the window. cancel() returns false once the post has begun.
function startUndoableCommentSend(sessionId, content, onTick) {
  let left = COMMENT_UNDO_SECONDS;
  let timer = null;
  let posting = false;
  let settled = false;
  let settle = null;
  const sent = new Promise((resolve, reject) => {
    settle = {resolve, reject};
  });
  const removeLeaveListeners = () => {
    window.removeEventListener('pagehide', cancelOnLeave);
    document.removeEventListener('charliebot:session-view-teardown', cancelOnLeave);
  };
  const cancel = () => {
    if (posting || settled) return false;
    settled = true;
    clearTimeout(timer);
    removeLeaveListeners();
    settle.resolve(null);
    return true;
  };
  function cancelOnLeave() {
    cancel();
  }
  const step = () => {
    if (settled) return;
    onTick(left);
    if (settled) return;
    if (left === 0) {
      posting = true;
      removeLeaveListeners();
      postCommentMessage(sessionId, content).then(
        (result) => { settled = true; settle.resolve(result); },
        (error) => { settled = true; settle.reject(error); },
      );
      return;
    }
    left -= 1;
    timer = setTimeout(step, 1000);
  };
  window.addEventListener('pagehide', cancelOnLeave);
  document.addEventListener('charliebot:session-view-teardown', cancelOnLeave);
  step();
  return {
    sent,
    cancel,
  };
}

// One comment entry: the locator line (▸) and the comment line (↳). A quote-less
// entry, as a click-to-send message has, carries only the context.
function buildCommentEntry(quote, context, comment, suffix) {
  if (quote === '') {
    return ('▸ ' + context).split('\n').concat(('↳ ' + comment).split('\n'));
  }
  const quoteLines = (context
    ? '▸ ' + context + ' › "' + quote + '"'
    : '▸ "' + quote + '"').split('\n');
  // A quote spanning lines keeps its locator shape. The suffix rides the last
  // quote line, right of the closing quote.
  quoteLines[quoteLines.length - 1] += suffix || '';
  return quoteLines.concat(('↳ ' + comment).split('\n'));
}

// One numbered message: the header line, then each entry's lines, the entry's
// first line numbered and its later lines indented under it. `spaced` puts a
// blank line after the header and between entries, the comment batch's layout;
// a pick message is not spaced.
function assembleBatchMessage(header, entries, spaced) {
  const lines = spaced ? [header, ''] : [header];
  for (let i = 0; i < entries.length; i++) {
    lines.push((i + 1) + '. ' + entries[i][0]);
    for (let j = 1; j < entries[i].length; j++) {
      lines.push('   ' + entries[i][j]);
    }
    if (spaced && i < entries.length - 1) lines.push('');
  }
  return lines.join('\n');
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
