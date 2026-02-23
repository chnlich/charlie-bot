// ---------------------------------------------------------------------------
// Relative time formatting
// ---------------------------------------------------------------------------
function relativeTime(isoStr) {
  const now = Date.now();
  const then = new Date(isoStr).getTime();
  const diff = Math.max(0, now - then);
  const mins = Math.floor(diff / 60000);
  if (mins < 1) return 'just now';
  if (mins < 60) return mins + 'm ago';
  const hrs = Math.floor(mins / 60);
  if (hrs < 24) return hrs + 'h ago';
  const days = Math.floor(hrs / 24);
  if (days < 30) return days + 'd ago';
  return new Date(isoStr).toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

function updateRelativeTimes() {
  document.querySelectorAll('.session-time[data-time]').forEach(el => {
    el.textContent = relativeTime(el.dataset.time);
  });
}

// ---------------------------------------------------------------------------
// Absolute next-run time formatting (for scheduled sessions)
// ---------------------------------------------------------------------------
function formatNextRun(isoString) {
  if (!isoString) return '';
  const d = new Date(isoString);
  const dateStr = d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
  const timeStr = d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit', hour12: true });
  return dateStr + ', ' + timeStr;
}
