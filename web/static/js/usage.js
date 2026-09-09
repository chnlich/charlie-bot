// ---------------------------------------------------------------------------
// Usage display helpers
// ---------------------------------------------------------------------------
function formatTokens(n) {
  return Math.round(n / 1000) + 'k';
}

function formatUsageCostValue(cost) {
  return cost == null ? 'N/A' : '$' + cost.toFixed(2);
}

// Each stream delta carries the whole accumulated draft, so a per-delta paint costs
// O(deltas x draft) (the marked re-parse dominates). Coalescing bounds the paint
// rate: the leading edge keeps first paint immediate, the trailing edge lands the last draft.
const STREAM_RENDER_MS = 200;
let streamRenderedAt = -STREAM_RENDER_MS;
let streamRenderTimer = null;
let streamPendingDraft = null;

function paintStreamDraft(draft) {
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
  // The parse below must see the recording window only, and a parse that
  // throws must not leave it set into later renders.
  streamPaintCodeTokens = [];
  try {
    html += parseStreamDraft(fixNestedFences(content));
  } finally {
    streamPaintCodeTokens = null;
  }
  inner.innerHTML = html;
  // The draft text is the walk's source; renderChatMath skips its full-text
  // KaTeX scan when it carries no math delimiter. Concatenation can only
  // create a delimiter across the seam, never remove one.
  renderChatMath(inner, content + thinking);
  if (wasAtBottom) {
    container.scrollTop = container.scrollHeight;
  } else {
    showScrollToBottom();
  }
}

function flushStreamDraft() {
  streamRenderTimer = null;
  const draft = streamPendingDraft;
  streamPendingDraft = null;
  if (draft === null) return;
  streamRenderedAt = Date.now();
  paintStreamDraft(draft);
}

function showStreaming(draft) {
  if (streamRenderTimer === null && Date.now() - streamRenderedAt >= STREAM_RENDER_MS) {
    streamRenderedAt = Date.now();
    paintStreamDraft(draft);
    return;
  }
  streamPendingDraft = draft;
  if (streamRenderTimer === null) streamRenderTimer = setTimeout(flushStreamDraft, STREAM_RENDER_MS);
}

function hideStreaming() {
  // A pending trailing paint must not land after a hide: callers hide from
  // terminal or context-switch paths (committed bubble, error, session swap).
  if (streamRenderTimer !== null) {
    clearTimeout(streamRenderTimer);
    streamRenderTimer = null;
  }
  streamPendingDraft = null;
  document.getElementById('streaming-msg').classList.add('hidden');
  document.getElementById('streaming-content').innerHTML = '';
}
