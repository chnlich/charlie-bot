// ---------------------------------------------------------------------------
// Claude Code external usage strip
// ---------------------------------------------------------------------------
let _ccUsageData = null;

function formatResetTime(isoString) {
  const now = Date.now();
  const reset = new Date(isoString).getTime();
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

function renderCcUsage(data) {
  _ccUsageData = data;
  const strip = document.getElementById('cc-usage-strip');
  if (!strip) return;

  const buckets = [
    {key: 'five_hour', bar: 'cc-usage-5h-bar', pct: 'cc-usage-5h-pct', reset: 'cc-usage-5h-reset'},
    {key: 'seven_day', bar: 'cc-usage-7d-bar', pct: 'cc-usage-7d-pct', reset: 'cc-usage-7d-reset'},
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
    if (resetEl && bucket.resets_at) resetEl.textContent = formatResetTime(bucket.resets_at);
  }

  strip.classList.remove('hidden');
}

function _refreshResetTimers() {
  if (!_ccUsageData) return;
  const buckets = [
    {key: 'five_hour', el: 'cc-usage-5h-reset'},
    {key: 'seven_day', el: 'cc-usage-7d-reset'},
  ];
  for (const b of buckets) {
    const bucket = _ccUsageData[b.key];
    if (!bucket || !bucket.resets_at) continue;
    const el = document.getElementById(b.el);
    if (el) el.textContent = formatResetTime(bucket.resets_at);
  }
}

document.addEventListener('DOMContentLoaded', () => {
  fetch('/api/cc-usage')
    .then(r => r.json())
    .then(data => renderCcUsage(data))
    .catch(err => console.warn('cc-usage fetch failed:', err));

  // Refresh countdown timers every 60s (client-side only)
  setInterval(_refreshResetTimers, 60000);

  // Re-fetch usage data every 10 minutes
  setInterval(() => {
    fetch('/api/cc-usage')
      .then(r => r.json())
      .then(data => renderCcUsage(data))
      .catch(err => console.warn('cc-usage poll failed:', err));
  }, 600000);
});
