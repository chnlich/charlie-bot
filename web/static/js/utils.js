// ---------------------------------------------------------------------------
// Relative time formatting
// ---------------------------------------------------------------------------
function relativeTime(isoStr) {
  if (!isoStr) return '';
  const d = new Date(isoStr);
  const dateStr = d.toLocaleDateString('en-US', { month: 'short', day: 'numeric' });
  const timeStr = d.toLocaleTimeString('en-US', { hour: 'numeric', minute: '2-digit', hour12: true });
  return dateStr + ', ' + timeStr;
}

function updateRelativeTimes() {
  document.querySelectorAll('.session-time[data-time]').forEach(el => {
    el.textContent = relativeTime(el.dataset.time);
  });
  // The pending-triggers tray's remaining-time spans ride the same sweep, so
  // "(in 3h 52m)" stays current between the tray's own refetches.
  document.querySelectorAll('.tray-reltime[data-time]').forEach(el => {
    el.textContent = relativeFireIn(el.dataset.time);
  });
}

// ---------------------------------------------------------------------------
// Delayed-trigger time forms (the tray and the sidebar bell)
// ---------------------------------------------------------------------------

// "(in 3h 52m)" — the remaining time until an instant; "(due now)" once it has
// passed while the record is still pending. Hours do not roll into days: a
// 19h-away trigger reads "in 19h 17m".
function relativeFireIn(isoStr) {
  const ms = Date.parse(isoStr) - Date.now();
  if (!(ms > 0)) return '(due now)';
  const mins = Math.floor(ms / 60000);
  if (mins < 1) return '(in <1m)';
  if (mins < 60) return `(in ${mins}m)`;
  return `(in ${Math.floor(mins / 60)}h ${mins % 60}m)`;
}

// Local "HH:mm" (24h) — the bell title's and the tray's clock form.
function clockTimeHM(date) {
  return String(date.getHours()).padStart(2, '0') + ':' + String(date.getMinutes()).padStart(2, '0');
}

// Local "MM/DD HH:mm" — the tray's condition-line form.
function dateClockMDHM(date) {
  return String(date.getMonth() + 1).padStart(2, '0') + '/' + String(date.getDate()).padStart(2, '0')
    + ' ' + clockTimeHM(date);
}
