
(function() {
  const Chat = globalThis.Chat;

// ---------------------------------------------------------------------------
// Auto-scroll helper — returns true only when user is near the bottom
// ---------------------------------------------------------------------------
function shouldAutoScroll(container, threshold = 150) {
  return container.scrollHeight - container.scrollTop - container.clientHeight < threshold;
}

function escapeHtml(str) {
  const d = document.createElement('div');
  d.textContent = str;
  return d.innerHTML;
}

function escapeChatAttr(str) {
  return escapeHtml(String(str)).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

function escapeJsSingleQuoted(str) {
  return String(str)
    .replace(/\\/g, '\\\\')
    .replace(/'/g, "\\'")
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r');
}

function formatBubbleTime(isoStr) {
  if (!isoStr) return '';
  const d = new Date(isoStr);
  return d.toLocaleString(undefined, {
    month: 'short', day: 'numeric',
    hour: 'numeric', minute: '2-digit', second: '2-digit', hour12: true, timeZoneName: 'short'
  });
}

function messageRenderId(msg) {
  if (!msg || msg.id == null || msg.id === '') return '';
  return String(msg.id);
}

function messageIdentityAttrs(msg) {
  const id = messageRenderId(msg);
  let attrs = id ? ' data-message-id="' + escapeChatAttr(id) + '"' : '';
  if (msg && msg.role) attrs += ' data-message-role="' + escapeChatAttr(msg.role) + '"';
  // The turn fold row reads its time field from here — a message without a
  // timestamp emits no attribute and the row's time field stays empty.
  if (msg && msg.timestamp) attrs += ' data-message-ts="' + escapeChatAttr(msg.timestamp) + '"';
  return attrs;
}

function isRenderedMessage(msg) {
  const id = messageRenderId(msg);
  if (!id) return false;
  return document.querySelector('[data-message-id="' + CSS.escape(id) + '"]') !== null;
}

Chat.shouldAutoScroll = shouldAutoScroll;
Chat.escapeHtml = escapeHtml;
Chat.escapeChatAttr = escapeChatAttr;
Chat.escapeJsSingleQuoted = escapeJsSingleQuoted;
Chat.formatBubbleTime = formatBubbleTime;
Chat.messageIdentityAttrs = messageIdentityAttrs;
Chat.isRenderedMessage = isRenderedMessage;
Chat.expose([
  'shouldAutoScroll',
  'escapeHtml',
  'isRenderedMessage',
]);

})();
