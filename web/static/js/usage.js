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
    html += thinkingToggleHtml('streaming-thinking', thinking);
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
