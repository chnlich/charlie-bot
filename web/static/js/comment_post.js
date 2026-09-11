// ---------------------------------------------------------------------------
// Shared comment-tray client helpers (diff page, artifact pages). The request
// shape and the thrown messages are one contract: each tray catches the errors
// and surfaces `err.message` in its own toast.
// ---------------------------------------------------------------------------
async function postCommentMessage(sessionId, content) {
  const response = await fetch(`/api/chat/${encodeURIComponent(sessionId)}/message`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    credentials: 'same-origin',
    body: JSON.stringify({ content, uploaded_files: [] }),
  });
  if (response.status === 401) throw new Error('log in to comment');
  if (!response.ok) throw new Error(`Comment post failed: HTTP ${response.status}`);
}

// One tray toast at a time: showing a new one removes the previous. Each tray
// passes its own CSS class prefix, mount (the artifact layer marks nodes
// through injectRoot; the diff page appends straight to body) and removal
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
