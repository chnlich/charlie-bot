
(function() {
  const Chat = globalThis.Chat;

// Roles whose messages open a chat turn. Both turn-layout consumers read this
// list: rendering.js's DOM matcher and turn-engine.js's fold derive.
const STIMULUS_ROLES = ['user', 'scheduled_trigger', 'agent_message', 'worker_summary'];

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

// Missing values must render empty: the DOM textContent coercion would turn undefined
// into the literal string "undefined", and worker descriptions may be missing.
function escapeHtmlAttr(str) {
  return escapeHtml(str == null ? '' : String(str)).replace(/"/g, '&quot;').replace(/'/g, '&#39;');
}

function escapeJsSingleQuoted(str) {
  return String(str)
    .replace(/\\/g, '\\\\')
    .replace(/'/g, "\\'")
    .replace(/\n/g, '\\n')
    .replace(/\r/g, '\\r');
}

// Show-more toggle for over-limit text: the click swaps the short span for
// the full one, so the id base must be page-unique (callers randomize it).
// Every caller's host renders its text at text-xs, except the workers.js
// assistant bubble (text-sm); the pinned class holds the button at text-xs
// there too.
function showMoreToggleHtml(id, restHtml) {
  return `<span id="${id}-short">… <button onclick="document.getElementById('${id}-short').style.display='none';document.getElementById('${id}-full').style.display='inline'" class="text-blue-400 hover:underline text-xs">Show more</button></span><span id="${id}-full" style="display:none">${restHtml}</span>`;
}

// Collapsed "Thinking…" block: the button swaps the hidden div in place. The id
// must be page-unique — chat mints one per message; the streaming draft is a
// singleton, so its fixed id cannot collide.
function thinkingToggleHtml(id, thinking) {
  return `<button onclick="const el=document.getElementById('${id}');el.style.display=el.style.display==='none'?'block':'none'" class="text-xs text-slate-500 hover:text-slate-400 italic mb-1">Thinking…</button><div id="${id}" style="display:none" class="text-xs text-slate-500 whitespace-pre-wrap mb-2">${escapeHtml(String(thinking))}</div>`;
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
  let attrs = id ? ' data-message-id="' + escapeHtmlAttr(id) + '"' : '';
  if (msg && msg.role) attrs += ' data-message-role="' + escapeHtmlAttr(msg.role) + '"';
  // The turn fold row reads its time field from here — a message without a
  // timestamp emits no attribute and the row's time field stays empty.
  if (msg && msg.timestamp) attrs += ' data-message-ts="' + escapeHtmlAttr(msg.timestamp) + '"';
  return attrs;
}

function isRenderedMessage(msg) {
  const id = messageRenderId(msg);
  if (!id) return false;
  return document.querySelector('[data-message-id="' + CSS.escape(id) + '"]') !== null;
}

Chat.shouldAutoScroll = shouldAutoScroll;
Chat.escapeHtml = escapeHtml;
Chat.escapeHtmlAttr = escapeHtmlAttr;
Chat.escapeJsSingleQuoted = escapeJsSingleQuoted;
Chat.showMoreToggleHtml = showMoreToggleHtml;
Chat.thinkingToggleHtml = thinkingToggleHtml;
Chat.formatBubbleTime = formatBubbleTime;
Chat.messageIdentityAttrs = messageIdentityAttrs;
Chat.isRenderedMessage = isRenderedMessage;
Chat.STIMULUS_ROLES = STIMULUS_ROLES;
Chat.expose([
  'shouldAutoScroll',
  'escapeHtml',
  'escapeHtmlAttr',
  'isRenderedMessage',
  'showMoreToggleHtml',
  'thinkingToggleHtml',
  'STIMULUS_ROLES',
]);

})();
