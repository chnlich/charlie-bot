// ---------------------------------------------------------------------------
// Usage display helpers
// ---------------------------------------------------------------------------
function formatTokens(n) {
  return Math.round(n / 1000) + 'k';
}

function formatUsageCostValue(cost) {
  return cost == null ? 'N/A' : '$' + cost.toFixed(2);
}

function showStreaming(draft) {
  const el = document.getElementById('streaming-msg');
  const inner = document.getElementById('streaming-content');
  const container = document.getElementById('messages');
  const wasAtBottom = shouldAutoScroll(container);
  el.classList.remove('hidden');
  const content = (draft && draft.content) || '';
  const thinking = (draft && draft.thinking) || '';
  var html = '';
  if (thinking) {
    var thinkId = 'streaming-thinking';
    html += "<button onclick=\"const el=document.getElementById(\x27" + thinkId + "\x27);el.style.display=el.style.display===\x27none\x27?\x27block\x27:\x27none\x27\" class=\"text-xs text-slate-500 hover:text-slate-400 italic mb-1\">Thinking…</button>"
      + "<div id=\"" + thinkId + "\" style=\"display:none\" class=\"text-xs text-slate-500 whitespace-pre-wrap mb-2\">" + escapeHtml(String(thinking)) + "</div>";
  }
  html += marked.parse(fixNestedFences(content));
  inner.innerHTML = html;
  renderChatMath(inner);
  if (wasAtBottom) {
    container.scrollTop = container.scrollHeight;
  } else {
    showScrollToBottom();
  }
}

function hideStreaming() {
  document.getElementById('streaming-msg').classList.add('hidden');
  document.getElementById('streaming-content').innerHTML = '';
}
