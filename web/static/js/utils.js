// ---------------------------------------------------------------------------
// Relative time formatting
// ---------------------------------------------------------------------------
function relativeTime(isoStr) {
  if (!isoStr) return '';
  const d = new Date(isoStr);
  const dateStr = d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
  const timeStr = d.toLocaleTimeString(undefined, { hour: 'numeric', minute: '2-digit', hour12: true });
  return dateStr + ', ' + timeStr;
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
