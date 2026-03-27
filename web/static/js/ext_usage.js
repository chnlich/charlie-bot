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

// Map provider key to DOM ID prefix: 'cc-opus' -> 'cc', 'codex' -> 'codex'
function _providerPrefix(providerKey) {
  if (providerKey === 'cc-opus') return 'cc';
  return providerKey;
}

function _renderProvider(providerKey, prefix, data) {
  const buckets = [
    {key: 'five_hour', bar: 'ext-usage-' + prefix + '-5h-bar', pct: 'ext-usage-' + prefix + '-5h-pct', reset: 'ext-usage-' + prefix + '-5h-reset'},
    {key: 'seven_day', bar: 'ext-usage-' + prefix + '-7d-bar', pct: 'ext-usage-' + prefix + '-7d-pct', reset: 'ext-usage-' + prefix + '-7d-reset'},
  ];

  for (const b of buckets) {
    const bucket = data[b.key];
    if (!bucket) continue;
    const pct = typeof bucket.utilization === 'number' ? bucket.utilization : 0;
    const barEl = document.getElementById(b.bar);
    const pctEl = document.getElementById(b.pct);
    const resetEl = document.getElementById(b.reset);
    if (barEl) {
      barEl.style.width = Math.min(pct, 100).toFixed(1) + '%';
      barEl.className = 'h-full rounded-full transition-all duration-300 ' + _barColor(pct);
    }
    if (pctEl) pctEl.textContent = Math.round(pct) + '%';
    if (resetEl && bucket.resets_at) resetEl.textContent = formatResetTime(providerKey, data, bucket);
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
      if (el) el.textContent = formatResetTime(key, providerData, bucket);
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
