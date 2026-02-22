// ---------------------------------------------------------------------------
// Usage display helpers
// ---------------------------------------------------------------------------
function formatTokens(n) {
  return Math.round(n / 1000) + 'k';
}

function updateUsageDisplay(ev) {
  const usage = ev.usage || {};
  const contextTokens = (usage.input_tokens || 0)
    + (usage.cache_creation_input_tokens || 0)
    + (usage.cache_read_input_tokens || 0);

  // Accumulate cost even for zero-token results
  usageTotalCost += (ev.total_cost_usd || 0);

  // Skip token/bar update when Claude Code emits a result with zero usage
  if (contextTokens === 0) {
    // Still update cost display
    const cost = document.getElementById('usage-cost');
    if (cost) cost.textContent = '$' + usageTotalCost.toFixed(2);
    const sidebarUsage = document.getElementById('sidebar-usage-' + SESSION_ID);
    if (sidebarUsage) {
      sidebarUsage.textContent = sidebarUsage.textContent.replace(/\$[\d.]+/, '$' + usageTotalCost.toFixed(2));
    }
    return;
  }

  // Extract context limit from modelUsage
  const modelUsage = ev.modelUsage || {};
  let contextLimit = 200000;
  for (const m in modelUsage) {
    contextLimit = modelUsage[m].contextWindow || 200000;
    break;
  }

  const pct = contextLimit > 0 ? (contextTokens / contextLimit * 100) : 0;

  // Update header indicator
  const indicator = document.getElementById('usage-indicator');
  const bar = document.getElementById('usage-bar');
  const text = document.getElementById('usage-text');
  const cost = document.getElementById('usage-cost');
  if (indicator) {
    indicator.classList.remove('hidden');
    bar.style.width = Math.min(pct, 100).toFixed(1) + '%';
    bar.className = 'h-full rounded-full transition-all duration-300 '
      + (pct > 80 ? 'bg-red-500' : pct > 50 ? 'bg-yellow-500' : 'bg-blue-500');
    text.textContent = formatTokens(contextTokens) + ' / ' + formatTokens(contextLimit);
    cost.textContent = '$' + usageTotalCost.toFixed(2);
  }

  // Update sidebar for current session
  const sidebarUsage = document.getElementById('sidebar-usage-' + SESSION_ID);
  if (sidebarUsage) {
    sidebarUsage.textContent = formatTokens(contextTokens) + ' tokens \u00b7 $' + usageTotalCost.toFixed(2);
    sidebarUsage.classList.remove('hidden');
  }
}

function showStreaming(content) {
  const el = document.getElementById('streaming-msg');
  const inner = document.getElementById('streaming-content');
  el.classList.remove('hidden');
  inner.innerHTML = marked.parse(content);
  const container = document.getElementById('messages');
  container.scrollTop = container.scrollHeight;
}

function hideStreaming() {
  document.getElementById('streaming-msg').classList.add('hidden');
  document.getElementById('streaming-content').innerHTML = '';
}
