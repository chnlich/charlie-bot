// ---------------------------------------------------------------------------
// External tool usage strip (multi-provider: CC Opus, Codex)
// ---------------------------------------------------------------------------
let _extUsageData = null;

function _parseTimestampMs(isoString) {
  if (!isoString) return NaN;
  return new Date(isoString).getTime();
}

function _shouldWaitForFreshCodexCapData(providerKey, providerData, bucket) {
  if (providerKey !== 'codex') return false;

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

function _providerRateLimitState(providerKey, providerData) {
  if (providerKey !== 'codex') return '';
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

function _formatFiveHourReset(providerKey, providerData, bucket) {
  const stale = _shouldWaitForFreshCodexCapData(providerKey, providerData, bucket);
  const resetHMS = _formatLocalHMS(bucket.resets_at);
  if (stale) return '(stale \u2013 ' + resetHMS + ')';
  const sampledAt = (providerKey === 'codex' && providerData.token_count_observed_at)
    ? providerData.token_count_observed_at
    : providerData.fetched_at;
  const sampledHMS = _formatLocalHMS(sampledAt);
  return '(' + sampledHMS + ' \u2013 ' + resetHMS + ')';
}

function formatResetTime(providerKey, providerData, bucket) {
  const now = Date.now();
  const reset = _parseTimestampMs(bucket.resets_at);
  if (_shouldWaitForFreshCodexCapData(providerKey, providerData, bucket)) {
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

// Map provider key to DOM ID prefix: 'cc-opus' -> 'cc', 'codex' -> 'codex'
function _providerPrefix(providerKey) {
  if (providerKey === 'cc-opus') return 'cc';
  return providerKey;
}

function _renderProvider(providerKey, prefix, data) {
  if (providerKey === 'codex') {
    const spend = data.spend || {};
    const spend24hEl = document.getElementById('ext-usage-codex-24h-spend');
    const spend7dEl = document.getElementById('ext-usage-codex-7d-spend');
    if (spend24hEl) spend24hEl.textContent = _formatSpendUsd(spend.last_24h_usd);
    if (spend7dEl) spend7dEl.textContent = _formatSpendUsd(spend.last_7d_usd);
  }

  const rateLimitState = _providerRateLimitState(providerKey, data);
  const stateEl = document.getElementById('ext-usage-' + prefix + '-state');
  if (stateEl) {
    const stateLabel = _providerStateLabel(rateLimitState);
    stateEl.textContent = stateLabel;
    stateEl.classList.toggle('hidden', !stateLabel);
  }

  const buckets = [
    {key: 'five_hour', bar: 'ext-usage-' + prefix + '-5h-bar', pct: 'ext-usage-' + prefix + '-5h-pct', reset: 'ext-usage-' + prefix + '-5h-reset'},
    {key: 'seven_day', bar: 'ext-usage-' + prefix + '-7d-bar', pct: 'ext-usage-' + prefix + '-7d-pct', reset: 'ext-usage-' + prefix + '-7d-reset'},
  ];

  for (const b of buckets) {
    const bucket = data[b.key];
    if (!bucket) continue;
    const barEl = document.getElementById(b.bar);
    const pctEl = document.getElementById(b.pct);
    const resetEl = document.getElementById(b.reset);
    if (rateLimitState) {
      if (barEl) {
        barEl.style.width = '0.0%';
        barEl.className = 'h-full rounded-full transition-all duration-300 bg-slate-600';
      }
      if (pctEl) pctEl.textContent = 'plan';
      if (resetEl) resetEl.textContent = _bucketStateResetLabel(b.key, rateLimitState);
      continue;
    }
    let pct = typeof bucket.utilization === 'number' ? bucket.utilization : 0;
    const stale = _shouldWaitForFreshCodexCapData(providerKey, data, bucket);
    if (stale) pct = 0;
    if (barEl) {
      barEl.style.width = Math.min(pct, 100).toFixed(1) + '%';
      barEl.className = 'h-full rounded-full transition-all duration-300 ' + _barColor(pct);
    }
    if (pctEl) pctEl.textContent = stale ? '\u2014' : Math.round(pct) + '%';
    if (resetEl) {
      resetEl.textContent = bucket.resets_at
        ? (b.key === 'five_hour'
          ? _formatFiveHourReset(providerKey, data, bucket)
          : formatResetTime(providerKey, data, bucket))
        : '\u2014';
    }
  }
}

function renderExtUsage(data) {
  _extUsageData = data;
  const strip = document.getElementById('ext-usage-strip');
  if (!strip) return;

  const providers = data.providers || {};
  for (const [key, providerData] of Object.entries(providers)) {
    _renderProvider(key, _providerPrefix(key), providerData);
  }

  strip.classList.remove('hidden');
}

function _refreshResetTimers() {
  if (!_extUsageData) return;
  const providers = _extUsageData.providers || {};
  for (const [key, providerData] of Object.entries(providers)) {
    const prefix = _providerPrefix(key);
    const buckets = [
      {key: 'five_hour', el: 'ext-usage-' + prefix + '-5h-reset'},
      {key: 'seven_day', el: 'ext-usage-' + prefix + '-7d-reset'},
    ];
    for (const b of buckets) {
      const bucket = providerData[b.key];
      if (!bucket || !bucket.resets_at) continue;
      const el = document.getElementById(b.el);
      if (el) {
        el.textContent = b.key === 'five_hour'
          ? _formatFiveHourReset(key, providerData, bucket)
          : formatResetTime(key, providerData, bucket);
      }
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
