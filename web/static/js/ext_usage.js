// ---------------------------------------------------------------------------
// External tool usage strip (multi-provider, multi-account: Claude, Codex).
// The strip DOM is built dynamically from the payload: one group per provider
// (labels "Claude" / "Codex"), one row per account.
// ---------------------------------------------------------------------------
let _extUsageData = null;
// Per-row reset element refs + captured data, rebuilt on every render so the
// 60s client-side countdown refresh can recompute reset labels without a
// server round-trip.
let _extUsageRows = [];

function _parseTimestampMs(isoString) {
  if (!isoString) return NaN;
  return new Date(isoString).getTime();
}

function _shouldWaitForFreshCodexCapData(providerData, bucket) {
  if (providerData.provider !== 'codex') return false;

  const reset = _parseTimestampMs(bucket.resets_at);
  if (!Number.isFinite(reset) || reset > Date.now()) return false;

  const observedAt = _parseTimestampMs(providerData.token_count_observed_at);
  return Number.isFinite(observedAt) && observedAt < reset;
}

function _formatLocalHMS(isoString) {
  const d = new Date(isoString);
  const hh = String(d.getHours()).padStart(2, '0');
  const mm = String(d.getMinutes()).padStart(2, '0');
  const ss = String(d.getSeconds()).padStart(2, '0');
  return hh + ':' + mm + ':' + ss;
}

function _providerRateLimitState(providerData) {
  if (providerData.provider !== 'codex') return '';
  return providerData.rate_limits_state || '';
}

function _providerStateLabel(state) {
  if (state === 'business-unlimited') return 'business / unlimited';
  return '';
}

function _bucketStateResetLabel(bucketKey, state) {
  if (state !== 'business-unlimited') return '';
  return bucketKey === 'five_hour' ? 'no 5h cap' : 'no 7d cap';
}

function _formatFiveHourReset(providerData, bucket) {
  const stale = _shouldWaitForFreshCodexCapData(providerData, bucket);
  const resetHMS = _formatLocalHMS(bucket.resets_at);
  if (stale) return '(stale \u2013 ' + resetHMS + ')';
  const sampledAt = (providerData.provider === 'codex' && providerData.token_count_observed_at)
    ? providerData.token_count_observed_at
    : providerData.fetched_at;
  const sampledHMS = _formatLocalHMS(sampledAt);
  return '(' + sampledHMS + ' \u2013 ' + resetHMS + ')';
}

function formatResetTime(providerData, bucket) {
  const now = Date.now();
  const reset = _parseTimestampMs(bucket.resets_at);
  if (_shouldWaitForFreshCodexCapData(providerData, bucket)) {
    return 'waiting for fresh cap data';
  }
  let diff = reset - now;
  if (diff < 0) return 'reset overdue';
  const days = Math.floor(diff / 86400000);
  diff -= days * 86400000;
  const hours = Math.floor(diff / 3600000);
  diff -= hours * 3600000;
  const minutes = Math.floor(diff / 60000);
  if (days > 0) return 'resets in ' + days + 'd ' + hours + 'h';
  if (hours > 0) return 'resets in ' + hours + 'h ' + minutes + 'm';
  return 'resets in ' + minutes + 'm';
}

function _barColor(pct) {
  if (pct > 80) return 'bg-red-500';
  if (pct >= 50) return 'bg-yellow-500';
  return 'bg-emerald-500';
}

function _formatSpendUsd(value) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return '\u2014';
  return '$' + value.toFixed(2);
}

function _providerGroupLabel(provider) {
  if (provider === 'claude') return 'Claude';
  if (provider === 'codex') return 'Codex';
  throw new Error('unknown usage provider: ' + provider);
}

function _el(tag, className) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  return node;
}

function _buildBucket(row, label, bucket, providerData, bucketKey) {
  const group = _el('div', 'flex items-center gap-1.5');
  const lbl = _el('span', '');
  lbl.textContent = label;
  group.appendChild(lbl);
  const barWrap = _el('div', 'w-24 h-1.5 bg-slate-700 rounded-full overflow-hidden');
  const bar = _el('div', 'h-full rounded-full transition-all duration-300');
  bar.setAttribute('data-field', bucketKey === 'five_hour' ? '5h-bar' : '7d-bar');
  barWrap.appendChild(bar);
  group.appendChild(barWrap);
  const pctEl = _el('span', '');
  pctEl.setAttribute('data-field', bucketKey === 'five_hour' ? '5h-pct' : '7d-pct');
  group.appendChild(pctEl);
  const resetEl = _el('span', 'text-slate-500');
  resetEl.setAttribute('data-field', bucketKey === 'five_hour' ? '5h-reset' : '7d-reset');
  group.appendChild(resetEl);
  row.appendChild(group);

  const state = _providerRateLimitState(providerData);
  if (state) {
    bar.style.width = '0.0%';
    bar.className = 'h-full rounded-full transition-all duration-300 bg-slate-600';
    pctEl.textContent = 'plan';
    resetEl.textContent = _bucketStateResetLabel(bucketKey, state);
  } else {
    let pct = typeof bucket.utilization === 'number' ? bucket.utilization : 0;
    const stale = _shouldWaitForFreshCodexCapData(providerData, bucket);
    if (stale) pct = 0;
    bar.style.width = Math.min(pct, 100).toFixed(1) + '%';
    bar.className = 'h-full rounded-full transition-all duration-300 ' + _barColor(pct);
    pctEl.textContent = stale ? '\u2014' : Math.round(pct) + '%';
    resetEl.textContent = bucket.resets_at
      ? (bucketKey === 'five_hour'
        ? _formatFiveHourReset(providerData, bucket)
        : formatResetTime(providerData, bucket))
      : '\u2014';
  }
  return resetEl;
}

function _buildRow(key, providerData) {
  if (providerData.error) {
    const row = _el('div', 'flex items-center gap-1.5 text-slate-600 opacity-60');
    row.setAttribute('data-key', key);
    const label = _el('span', 'text-slate-500 font-medium');
    label.textContent = providerData.account || key;
    row.appendChild(label);
    const err = _el('span', 'italic');
    err.setAttribute('data-field', 'error');
    err.textContent = providerData.error;
    row.appendChild(err);
    return {row, fiveHourResetEl: null, sevenDayResetEl: null};
  }

  const row = _el('div', 'flex items-center gap-1.5');
  row.setAttribute('data-key', key);
  const label = _el('span', 'text-slate-300 font-medium');
  label.textContent = providerData.account || key;
  row.appendChild(label);
  const fiveHourResetEl = _buildBucket(row, '5h:', providerData.five_hour || {}, providerData, 'five_hour');
  const sevenDayResetEl = _buildBucket(row, '7d:', providerData.seven_day || {}, providerData, 'seven_day');

  if (providerData.provider === 'codex') {
    const state = _providerRateLimitState(providerData);
    if (state) {
      const badge = _el('span', 'px-1.5 py-0.5 rounded bg-slate-700 text-[10px] font-medium text-slate-300');
      badge.setAttribute('data-field', 'state');
      badge.textContent = _providerStateLabel(state);
      row.appendChild(badge);
    }
    const spend = providerData.spend || {};
    const spendGroup = _el('div', 'flex items-center gap-1.5 text-slate-400');
    const l24 = _el('span', '');
    l24.textContent = '24h:';
    const s24 = _el('span', '');
    s24.setAttribute('data-field', 'spend-24h');
    s24.textContent = _formatSpendUsd(spend.last_24h_usd);
    const dot = _el('span', 'text-slate-700');
    dot.textContent = '\u00b7';
    const l7 = _el('span', '');
    l7.textContent = '7d:';
    const s7 = _el('span', '');
    s7.setAttribute('data-field', 'spend-7d');
    s7.textContent = _formatSpendUsd(spend.last_7d_usd);
    spendGroup.appendChild(l24);
    spendGroup.appendChild(s24);
    spendGroup.appendChild(dot);
    spendGroup.appendChild(l7);
    spendGroup.appendChild(s7);
    row.appendChild(spendGroup);
  } else if (providerData.provider !== 'claude') {
    throw new Error('unknown usage provider: ' + providerData.provider);
  }
  return {row, fiveHourResetEl, sevenDayResetEl};
}

function renderExtUsage(data) {
  _extUsageData = data;
  const strip = document.getElementById('ext-usage-strip');
  if (!strip) return;

  const providers = (data && data.providers) || {};
  const nodes = [];
  const rows = [];
  let currentProvider = null;
  for (const [key, providerData] of Object.entries(providers)) {
    const provider = providerData.provider;
    if (provider !== currentProvider) {
      if (currentProvider !== null) {
        const sep = _el('span', 'text-slate-700');
        sep.textContent = '\u2502';
        nodes.push(sep);
      }
      const glabel = _el('span', 'text-slate-500 font-medium');
      glabel.textContent = _providerGroupLabel(provider);
      nodes.push(glabel);
      currentProvider = provider;
    }
    const built = _buildRow(key, providerData);
    nodes.push(built.row);
    if (built.fiveHourResetEl) {
      rows.push({key, providerData, fiveHourResetEl: built.fiveHourResetEl, sevenDayResetEl: built.sevenDayResetEl});
    }
  }

  strip.replaceChildren(...nodes);
  _extUsageRows = rows;
  if (nodes.length > 0) {
    strip.classList.remove('hidden');
  }
}

function _refreshResetTimers() {
  for (const entry of _extUsageRows) {
    const providerData = entry.providerData;
    const fiveHour = providerData.five_hour;
    if (fiveHour && fiveHour.resets_at && entry.fiveHourResetEl) {
      entry.fiveHourResetEl.textContent = _formatFiveHourReset(providerData, fiveHour);
    }
    const sevenDay = providerData.seven_day;
    if (sevenDay && sevenDay.resets_at && entry.sevenDayResetEl) {
      entry.sevenDayResetEl.textContent = formatResetTime(providerData, sevenDay);
    }
  }
}

document.addEventListener('DOMContentLoaded', () => {
  fetch('/api/ext-usage')
    .then(r => r.json())
    .then(data => renderExtUsage(data))
    .catch(err => console.warn('ext-usage fetch failed:', err));

  // Refresh countdown timers every 60s (client-side only)
  setInterval(_refreshResetTimers, 60000);

  // Re-fetch usage data every 10 minutes
  setInterval(() => {
    fetch('/api/ext-usage')
      .then(r => r.json())
      .then(data => renderExtUsage(data))
      .catch(err => console.warn('ext-usage poll failed:', err));
  }, 600000);
});
