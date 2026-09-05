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
// Right-edge panel resize (LaTeX and backlog panels): drag-left enlarges.
// Width follows the mouse in px and persists as a parent percentage on
// release; the left-docked sidebar keeps its own px-bounded handler.
// ---------------------------------------------------------------------------
function initPanelResize(opts) {
  const handle = document.getElementById(opts.handleId);
  const panel = document.getElementById(opts.panelId);
  if (!handle || !panel) return;
  const container = panel.parentElement;
  const saved = localStorage.getItem(opts.storageKey);
  if (saved) panel.style.width = saved + '%';

  let startX, startW, containerW;
  handle.addEventListener('mousedown', (e) => {
    e.preventDefault();
    startX = e.clientX;
    containerW = container.offsetWidth;
    startW = panel.offsetWidth;
    handle.classList.add('active');
    document.body.classList.add('resizing');
    opts.onDragStart();

    function onMove(e) {
      const delta = startX - e.clientX;
      const w = Math.min(Math.max(startW + delta, containerW * 0.2), containerW * 0.8);
      panel.style.width = w + 'px';
    }
    function onUp() {
      handle.classList.remove('active');
      document.body.classList.remove('resizing');
      const pct = (panel.offsetWidth / container.offsetWidth * 100).toFixed(1);
      localStorage.setItem(opts.storageKey, pct);
      panel.style.width = pct + '%';
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      opts.onDragEnd();
    }
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  });
}
