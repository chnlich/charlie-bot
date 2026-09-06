// ---------------------------------------------------------------------------
// Shared comment-post client for the comment trays (diff page, artifact pages).
// The request shape and the thrown messages are one contract: each tray catches
// the errors and surfaces `err.message` in its own toast.
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
