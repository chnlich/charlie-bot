// The concurrency badge: how many agents run right now, and the chart of that number over time.
//
// One template fragment (web/templates/partials/concurrency_badge.html) loads this file on every app page, so the
// whole feature is one script. The server owns the counting (rust/crates/runtime/session-api/src/concurrency.rs):
// this module only displays /api/concurrency/now and /api/concurrency/history. Listeners are delegated from the
// document, because the page may replace its content — a session switch back to the welcome view replaces <main> —
// without reloading this file.
(function () {
  if (globalThis.ConcurrencyBadge) return;

  const POLL_MS = 5000;
  const DEBOUNCE_MS = 1000;
  const NOW_URL = '/api/concurrency/now';
  const HISTORY_URL = '/api/concurrency/history';
  const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
  const CHART_HEIGHT = 150;
  const PANEL_EDGE_MARGIN = 8;

  let range = '7d';
  let pollTimer = null;
  let debounceTimer = null;
  let requestSeq = 0;
  let points = [];
  let layout = null;

  // One instant in the browser's own time zone: "Oct 27, 14:05".
  function instantLabel(iso) {
    const when = new Date(iso);
    if (Number.isNaN(when.getTime())) return '';
    const minute = String(when.getMinutes()).padStart(2, '0');
    return `${MONTHS[when.getMonth()]} ${when.getDate()}, ${String(when.getHours()).padStart(2, '0')}:${minute}`;
  }

  // One day in the browser's own time zone: "Oct 27".
  function dayLabel(iso) {
    const when = new Date(iso);
    return Number.isNaN(when.getTime()) ? '' : `${MONTHS[when.getMonth()]} ${when.getDate()}`;
  }

  function turns(count) {
    return `${count} manager turn${count === 1 ? '' : 's'}`;
  }

  function runs(count) {
    return `${count} worker run${count === 1 ? '' : 's'}`;
  }

  function byId(name) {
    return document.getElementById(name);
  }

  // The nearest element a delegated event names, or null for an event whose target is not an element.
  function closest(event, selector) {
    return event.target instanceof Element ? event.target.closest(selector) : null;
  }

  function panelIsOpen() {
    const panel = byId('concurrency-panel');
    return !!panel && !panel.hidden;
  }

  // The hover tip repeats the live counts, and its z-index paints it over the open panel's range buttons and close
  // button, so the tip waits for the panel to close.
  function hideHoverTip() {
    const tip = byId('concurrency-tip');
    if (tip) tip.hidden = true;
  }

  function setPanelOpen(open) {
    const badge = byId('concurrency-badge');
    const panel = byId('concurrency-panel');
    if (!badge || !panel) return;
    if (panel.hidden === !open) return;
    panel.hidden = !open;
    badge.setAttribute('aria-expanded', open ? 'true' : 'false');
    if (open) {
      hideHoverTip();
      keepPanelInViewport();
      loadHistory();
    } else {
      hidePointTip();
    }
  }

  // The panel right-aligns to the badge, and on a phone that puts its left edge past the viewport.
  // Pull it back to the edge margin, so the whole panel stays readable.
  function keepPanelInViewport() {
    const panel = byId('concurrency-panel');
    if (!panel || panel.hidden) return;
    panel.style.left = '';
    panel.style.right = '';
    const rect = panel.getBoundingClientRect();
    const maxLeft = window.innerWidth - rect.width - PANEL_EDGE_MARGIN;
    const left = Math.min(Math.max(PANEL_EDGE_MARGIN, rect.left), maxLeft);
    if (left === rect.left) return;
    // The panel is absolutely positioned inside the anchor, so the offset is measured from the anchor's box.
    const anchorLeft = panel.offsetParent ? panel.offsetParent.getBoundingClientRect().left : 0;
    panel.style.left = `${left - anchorLeft}px`;
    panel.style.right = 'auto';
  }

  function hidePointTip() {
    const tip = document.querySelector('.cb-ptip');
    if (tip) tip.hidden = true;
    const cursor = document.querySelector('#concurrency-chart .cb-cursor');
    const mark = document.querySelector('#concurrency-chart .cb-dot-mark');
    if (cursor) cursor.setAttribute('visibility', 'hidden');
    if (mark) mark.setAttribute('visibility', 'hidden');
  }

  function renderLive(data) {
    const badge = byId('concurrency-badge');
    const count = byId('concurrency-count');
    if (!badge || !count) return;
    const running = Number(data.running) || 0;
    const manager = Number(data.manager) || 0;
    const worker = Number(data.worker) || 0;
    count.textContent = `${running} running`;
    badge.dataset.running = String(running);
    const breakdown = `${turns(manager)} · ${runs(worker)}`;
    badge.setAttribute('aria-label', `${running} running. ${breakdown}`);
    const tip = byId('concurrency-tip');
    if (tip) tip.textContent = breakdown;
  }

  function refresh() {
    return fetch(NOW_URL, {headers: {'Accept': 'application/json'}})
      .then((response) => (response.ok ? response.json() : Promise.reject(new Error(`HTTP ${response.status}`))))
      .then(renderLive)
      .catch((error) => console.error('concurrency badge refresh failed:', error));
  }

  // The WebSocket's frames arrive in bursts — one manager turn moves several rows — so a refetch waits out the burst.
  function refreshSoon() {
    if (debounceTimer !== null) return;
    debounceTimer = window.setTimeout(() => {
      debounceTimer = null;
      refresh();
    }, DEBOUNCE_MS);
  }

  function setRange(next) {
    range = next;
    document.querySelectorAll('#concurrency-panel .cb-range').forEach((button) => {
      button.setAttribute('aria-pressed', button.dataset.range === next ? 'true' : 'false');
    });
    loadHistory();
  }

  function setStatus(text) {
    const status = byId('concurrency-status');
    if (!status) return;
    status.textContent = text;
    status.hidden = !text;
  }

  function loadHistory() {
    const seq = ++requestSeq;
    setStatus('Loading…');
    fetch(`${HISTORY_URL}?range=${encodeURIComponent(range)}`, {headers: {'Accept': 'application/json'}})
      .then((response) => (response.ok ? response.json() : Promise.reject(new Error(`HTTP ${response.status}`))))
      .then((history) => {
        if (seq !== requestSeq) return;
        setStatus('');
        renderPeak(history.peak);
        drawChart(history.points || []);
      })
      .catch((error) => {
        if (seq !== requestSeq) return;
        setStatus('Could not load the history.');
        console.error('concurrency history failed:', error);
      });
  }

  function renderPeak(peak) {
    const label = byId('concurrency-peak');
    if (!label) return;
    label.textContent = peak ? `Peak ${peak.running} · ${instantLabel(peak.t)}` : '';
  }

  function drawChart(data) {
    const container = byId('concurrency-chart');
    if (!container) return;
    points = data;
    layout = null;
    if (!points.length) {
      container.innerHTML = '<p class="cb-empty">No run records yet.</p>';
      return;
    }
    const width = Math.max(240, container.clientWidth || 360);
    const height = CHART_HEIGHT;
    // The top margin holds the peak label above the maximum gridline; the bottom margin holds the day labels
    // below the zero line. The plot spans the two margins and 0 sits on its floor, where the area fill ends.
    const plotTop = 12;
    const plotBottom = height - 18;
    const maximum = points.reduce((most, point) => Math.max(most, point.running), 0);
    // The scale keeps a nonzero height for a series that is all zero, while the axis names the real maximum.
    const scale = Math.max(1, maximum);
    const step = points.length > 1 ? (width - 2) / (points.length - 1) : 0;
    const xAt = (index) => 1 + index * step;
    const yAt = (value) => plotBottom - (plotBottom - plotTop) * (value / scale);
    const segments = points.map((point, index) => `${index === 0 ? 'M' : 'L'}${xAt(index).toFixed(1)},${yAt(point.running).toFixed(1)}`);
    const area = `${segments.join('')}L${xAt(points.length - 1).toFixed(1)},${plotBottom}L1,${plotBottom}Z`;
    const first = points[0].t;
    const last = points[points.length - 1].t;
    container.innerHTML = `
      <svg width="${width}" height="${height}" viewBox="0 0 ${width} ${height}" role="img"
           aria-label="Running agents over the last ${range === 'all' ? 'recorded period' : range}">
        <line class="cb-grid" x1="1" y1="${yAt(0)}" x2="${width - 1}" y2="${yAt(0)}"></line>
        <line class="cb-grid" x1="1" y1="${yAt(scale)}" x2="${width - 1}" y2="${yAt(scale)}"></line>
        <text class="cb-axis" x="1" y="${yAt(scale) - 3}">${maximum}</text>
        <text class="cb-axis" x="1" y="${height - 4}">${dayLabel(first)}</text>
        <text class="cb-axis" x="${width - 1}" y="${height - 4}" text-anchor="end">${dayLabel(last)}</text>
        <path class="cb-area" d="${area}"></path>
        <path class="cb-line" d="${segments.join('')}"></path>
        <line class="cb-cursor" x1="0" y1="${plotTop}" x2="0" y2="${plotBottom}" visibility="hidden"></line>
        <circle class="cb-dot-mark" cx="0" cy="0" r="3" visibility="hidden"></circle>
      </svg>
      <div class="cb-ptip" hidden></div>`;
    layout = {width, height, plotTop, plotBottom, step, xAt, yAt, box: container, svg: container.querySelector('svg')};
  }

  function showPointTip(clientX) {
    if (!layout || !points.length) return;
    const rect = layout.box.getBoundingClientRect();
    const offset = clientX - rect.left;
    const index = Math.max(0, Math.min(points.length - 1, Math.round(offset / (layout.step || 1))));
    const point = points[index];
    const x = layout.xAt(index);
    const y = layout.yAt(point.running);
    const cursor = layout.box.querySelector('.cb-cursor');
    const mark = layout.box.querySelector('.cb-dot-mark');
    const tip = layout.box.querySelector('.cb-ptip');
    if (!cursor || !mark || !tip) return;
    cursor.setAttribute('x1', x.toFixed(1));
    cursor.setAttribute('x2', x.toFixed(1));
    cursor.setAttribute('visibility', 'visible');
    mark.setAttribute('cx', x.toFixed(1));
    mark.setAttribute('cy', y.toFixed(1));
    mark.setAttribute('visibility', 'visible');
    tip.textContent = `${instantLabel(point.t)} · ${point.running} running (${point.manager} manager · ${point.worker} worker)`;
    tip.hidden = false;
    // Keep the whole label inside the chart, so the panel's right edge never clips it.
    const half = tip.offsetWidth / 2;
    tip.style.left = `${Math.max(half + 2, Math.min(layout.width - half - 2, x)).toFixed(1)}px`;
    tip.style.top = `${Math.max(28, y).toFixed(1)}px`;
  }

  document.addEventListener('click', (event) => {
    if (closest(event, '#concurrency-badge')) {
      const panel = byId('concurrency-panel');
      setPanelOpen(!!panel && panel.hidden);
      return;
    }
    if (closest(event, '#concurrency-close')) {
      setPanelOpen(false);
      return;
    }
    const rangeButton = closest(event, '#concurrency-panel .cb-range');
    if (rangeButton) {
      setRange(rangeButton.dataset.range);
      return;
    }
    if (!closest(event, '#concurrency-panel')) setPanelOpen(false);
  });

  document.addEventListener('keydown', (event) => {
    if (event.key === 'Escape') setPanelOpen(false);
  });

  document.addEventListener('mouseover', (event) => {
    if (panelIsOpen()) return;
    if (closest(event, '#concurrency-badge')) {
      const tip = byId('concurrency-tip');
      if (tip && tip.textContent) tip.hidden = false;
    }
  });

  document.addEventListener('mousemove', (event) => {
    if (closest(event, '#concurrency-chart')) showPointTip(event.clientX);
  });

  document.addEventListener('mouseout', (event) => {
    const target = event.target instanceof Element ? event.target : null;
    const related = event.relatedTarget instanceof Node ? event.relatedTarget : null;
    const badge = target && target.closest('#concurrency-badge');
    if (badge && !(related && badge.contains(related))) {
      const tip = byId('concurrency-tip');
      if (tip) tip.hidden = true;
      return;
    }
    const chart = target && target.closest('#concurrency-chart');
    if (chart && !(related && chart.contains(related))) hidePointTip();
  });

  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'visible') refresh();
  });

  function poll() {
    if (document.visibilityState === 'visible') refresh();
  }

  globalThis.ConcurrencyBadge = {
    refresh,
    refreshSoon,
  };

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', refresh);
  } else {
    refresh();
  }
  pollTimer = window.setInterval(poll, POLL_MS);
})();
